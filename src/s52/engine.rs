//! S-52 presentation engine - decides what to render and how.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Mutex;

use crate::senc::{Feature, s57_code_to_acronym};

use super::cs::{execute_cs, CsContext};
use super::instruction::RenderInstruction;
use super::lookup::{DisplayCategory, DisplayPriority, GeometryType, LookupEntry, LookupTables, TableName};
use super::parser::parse_chartsymbols;
use super::settings::MarinerSettings;

/// Cache key for CS procedure results.
/// Combines object class, procedure name, and a hash of relevant attributes.
#[derive(Clone, Eq, PartialEq, Hash)]
struct CsCacheKey {
    object_class: u16,
    /// The feature's own primitive. OBSTRN04 and WRECKS02 branch on it and
    /// return entirely different instructions — a point obstruction gets a
    /// symbol, an area one gets a fill and no symbol at all. Without this in
    /// the key, two obstructions of the same class with the same attributes
    /// but different geometry share an entry, and whichever resolved first
    /// decides how both are drawn: the area silently loses its fill, or the
    /// point becomes invisible.
    feature_type: crate::senc::FeatureType,
    procedure: String,
    attr_hash: u64,
}

#[derive(Clone, Eq, PartialEq, Hash)]
struct ResolveCacheKey {
    object_class: u16,
    geom_type: GeometryType,
    attr_hash: u64,
    settings_hash: u64,
}

fn settings_hash(settings: &MarinerSettings) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    settings.show_displaybase.hash(&mut hasher);
    settings.show_standard.hash(&mut hasher);
    settings.show_other.hash(&mut hasher);
    settings.show_text.hash(&mut hasher);
    settings.show_soundings.hash(&mut hasher);
    settings.safety_depth.to_bits().hash(&mut hasher);
    settings.safety_contour.to_bits().hash(&mut hasher);
    settings.shallow_contour.to_bits().hash(&mut hasher);
    settings.deep_contour.to_bits().hash(&mut hasher);
    settings.depth_unit.hash(&mut hasher);
    settings.depth_shade_mode.hash(&mut hasher);
    settings.symbolized_boundaries.hash(&mut hasher);
    settings.simplified_points.hash(&mut hasher);
    settings.show_important_text_only.hash(&mut hasher);
    settings.show_chart_boundaries.hash(&mut hasher);
    // These three gate resolve_feature directly. Leaving them out of the key
    // means a toggle at runtime is answered from the cache with the old verdict.
    settings.show_meta_objects.hash(&mut hasher);
    settings.show_quality_of_data.hash(&mut hasher);
    settings.use_super_scamin.hash(&mut hasher);
    settings.depth_relief.hash(&mut hasher);
    hasher.finish()
}

/// Compute a fast hash of a feature's integer attribute map.
fn feature_attr_hash(feature: &Feature) -> u64 {
    use crate::senc::AttributeValue;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    // Every attribute exactly as the procedures will read it, sorted by name.
    // This once hashed a summary — floats cut to integers, text reduced to
    // the numbers in it — so DRVAL1 −0.5 and 0.5 shared a cache entry and
    // drew the same depth shade, and every virtual AIS aid drew whatever the
    // first one's SYMINS said.
    let mut attrs: Vec<_> = feature.attributes.iter().collect();
    attrs.sort_by_key(|(k, _)| *k);
    for (name, value) in attrs {
        name.hash(&mut hasher);
        match value {
            AttributeValue::Integer(v) => (0u8, *v).hash(&mut hasher),
            AttributeValue::Float(v) => (1u8, v.to_bits()).hash(&mut hasher),
            AttributeValue::String(s) => (2u8, s.as_str()).hash(&mut hasher),
        }
    }
    hasher.finish()
}

/// S-52 presentation engine.
/// Uses lookup tables from chartsymbols.xml to decide feature visibility and styling.
pub struct S52Engine {
    /// Lookup tables loaded from chartsymbols.xml
    pub tables: LookupTables,
    /// Mariner display settings
    pub settings: MarinerSettings,
    /// Cache for CS procedure results: (class, procedure, attr_hash) → instructions.
    /// Uses Mutex instead of RefCell to allow sharing across rayon threads.
    cs_cache: Mutex<HashMap<CsCacheKey, Vec<RenderInstruction>>>,
    /// Cache for fully resolved portrayal results.
    resolve_cache: Mutex<HashMap<ResolveCacheKey, Option<ResolvedFeature>>>,
}

/// Result of resolving a feature through the S-52 lookup + CS pipeline.
/// Contains all information needed to render the feature.
#[derive(Debug, Clone)]
pub struct ResolvedFeature {
    /// S-52 display priority (0-9)
    pub priority: u8,
    /// Display category
    pub category: DisplayCategory,
    /// Fully expanded render instructions (CS procedures already resolved)
    pub instructions: Vec<RenderInstruction>,
}

impl S52Engine {
    /// Whether DEPCNT02 promotes this feature to DISPLAYBASE.
    ///
    /// The procedure's own commentary: "The contour selected is highlighted as
    /// the safety contour and put in DISPLAYBASE." s52plib implements both
    /// halves — `m_DisplayCat = DISPLAYBASE` and `Scamin = 1e8+1` — because the
    /// safety contour is the one line on the chart that must never be dropped.
    /// Manx did neither, so with a 2 m safety contour three segments of it
    /// were SCAMIN-filtered out of a 1:11600 view.
    ///
    /// `selected` is the chart's safety contour ([`select_safety_contour`]);
    /// `None` tests against the mariner's setting as-is.
    ///
    /// [`select_safety_contour`]: super::cs::select_safety_contour
    pub fn is_promoted_safety_contour(
        &self,
        feature: &crate::senc::Feature,
        selected: Option<f64>,
    ) -> bool {
        // DEPCNT (43). DEPARE's own boundary reaches the same test through
        // DEPCNT02's "continuation A", but Manx draws that as an area
        // boundary, which is not SCAMIN-filtered separately.
        feature.type_code == 43 && super::cs::is_safety_contour(feature, &self.settings, selected)
    }

    /// The chart's selected safety contour: the shallowest DEPCNT line at
    /// least as deep as the mariner's setting, as OpenCPN's
    /// `s57chart::SetSafetyContour` picks it.
    ///
    /// Only contour *lines* are candidates. The value decides which line is
    /// drawn as the safety contour, so a depth that no line carries selects
    /// nothing at all: the Danish cell E94704 has lines at 2, 4, 6 and 10 m
    /// and two small depth areas limited at 3 m, and counting area limits
    /// chose 3 m there — and left the 4 m line, the real safety contour,
    /// drawn as an ordinary contour.
    ///
    /// One deliberate difference from OpenCPN: it drops the safety contour
    /// altogether when the next line is deeper than the deep contour. S-52
    /// asks for the next deeper contour, and a 10 m line in a chart with no
    /// shallower one is still the edge of safe water.
    pub fn chart_safety_contour(&self, features: &[crate::senc::Feature]) -> Option<f64> {
        let depths = features
            .iter()
            .filter(|f| f.type_code == 43)
            .filter_map(|f| f.valdco());
        super::cs::select_safety_contour(depths, self.settings.safety_contour as f64)
    }

    fn bypass_display_category_filter(&self, type_code: u16) -> bool {
        // M_QUAL (308) has display category OTHER, so the standard display
        // hides it. OpenCPN's "Quality of data" switch forces it visible
        // independently of the category; mirror that rather than reclassifying.
        if type_code == 308 && self.settings.show_quality_of_data {
            return true;
        }
        // SOUNDG (129) is display category OTHER, but OpenCPN gates soundings on
        // their own switch (`m_bShowSoundg`), not on the category.
        if type_code == 129 && self.settings.show_soundings {
            return true;
        }
        Self::bypass_display_category_filter_static(type_code)
    }

    fn bypass_display_category_filter_static(type_code: u16) -> bool {
        // M_CSCL (compilation scale, code 301) is metadata used by SCAMIN and
        // cross-chart cell selection. It carries no visible portrayal, so
        // letting it pass the category filter is harmless and keeps the
        // meta-feature book-keeping live. Do NOT bypass M_COVR (302) or Lake
        // (69): those are visible features and must honour the user's display
        // category setting the same way OpenCPN does.
        matches!(type_code, 301)
    }

    /// Create engine by loading chartsymbols.xml
    pub fn load<P: AsRef<Path>>(chartsymbols_path: P) -> Result<Self, String> {
        let tables = parse_chartsymbols(chartsymbols_path)?;

        // Debug: verify depth colors are loaded
        let depth_colors = ["DEPIT", "DEPVS", "DEPMS", "DEPMD", "DEPDW"];
        for color_name in &depth_colors {
            if let Some([r, g, b]) = tables.get_color(color_name) {
                log::warn!("DEBUG S52: {} = RGB({},{},{})", color_name, r, g, b);
            } else {
                log::warn!("DEBUG S52: {} NOT FOUND in color table!", color_name);
            }
        }

        Ok(Self {
            tables,
            settings: MarinerSettings::default(),
            cs_cache: Mutex::new(HashMap::new()),
            resolve_cache: Mutex::new(HashMap::new()),
        })
    }

    /// Create engine with pre-loaded tables
    pub fn new(tables: LookupTables) -> Self {
        Self {
            tables,
            settings: MarinerSettings::default(),
            cs_cache: Mutex::new(HashMap::new()),
            resolve_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Update mariner settings. Invalidates CS result cache.
    pub fn set_settings(&mut self, settings: MarinerSettings) {
        self.settings = settings;
        self.cs_cache.lock().unwrap().clear();
        self.resolve_cache.lock().unwrap().clear();
    }

    /// Unified feature resolution: lookup + CS expansion.
    ///
    /// Performs the complete S-52 pipeline for a single feature:
    /// 1. Finds the best matching LUP entry (skips attr alloc for generic entries)
    /// 2. Checks display category against mariner settings
    /// 3. Parses instruction string
    /// 4. Expands CS() procedures inline
    ///
    /// Returns None if the feature should not be rendered (no LUP match or filtered out).
    pub fn resolve_feature(
        &self,
        feature: &Feature,
        geom_type: GeometryType,
    ) -> Option<ResolvedFeature> {
        self.resolve_feature_ctx(feature, geom_type, CsContext::EMPTY)
    }

    /// [`resolve_feature`] with the feature's surroundings supplied.
    ///
    /// Only UDWHAZ03 (via OBSTRN04/WRECKS02) reads the context today, but it
    /// participates in both cache keys: the same rock in shallow water and in
    /// deep water resolves differently and must not share a cache entry.
    ///
    /// [`resolve_feature`]: S52Engine::resolve_feature
    pub fn resolve_feature_ctx(
        &self,
        feature: &Feature,
        geom_type: GeometryType,
        ctx: &CsContext,
    ) -> Option<ResolvedFeature> {
        let acronym = s57_code_to_acronym(feature.type_code);
        let attr_hash = feature_attr_hash(feature) ^ ctx.hash_value();
        let resolve_key = ResolveCacheKey {
            object_class: feature.type_code,
            geom_type,
            attr_hash,
            settings_hash: settings_hash(&self.settings),
        };

        if let Some(cached) = self.resolve_cache.lock().unwrap().get(&resolve_key).cloned() {
            return cached;
        }

        let table = TableName::preferred_ext(
            geom_type,
            self.settings.symbolized_boundaries,
            self.settings.simplified_points,
        );
        let entry = self.tables.lookup_best_fast(acronym, geom_type, table, feature)?;

        log::debug!(
            "RESOLVE: {} geom={:?} table={:?} → instr='{}'",
            acronym, geom_type, table, entry.instruction
        );

        // M_QUAL belongs to the quality-of-data switch alone: s52plib puts it
        // on the no-show list whenever that switch is off
        // (`SetQualityOfData(false)`, honoured by `ObjectRenderCheckRules` in
        // every display category), whether meta objects are shown or not.
        if acronym == "M_QUAL" && !self.settings.show_quality_of_data {
            return None;
        }

        // Meta-object filter, mirroring s52plib.cpp ObjectRenderCheckCat.
        //
        // Under "All" (the OTHER category selected) the filter applies only to
        // meta objects that are *themselves* category OTHER — an M_NSYS is
        // category STANDARD, so OpenCPN keeps drawing its navigational-system
        // boundary. Under any narrower category every `M_*` class is dropped.
        // Treating them all alike hid 419 M_NSYS features the reference shows.
        if !self.settings.show_meta_objects && acronym.starts_with("M_") {
            let meta_filtered = if self.settings.show_other {
                entry.display_category == DisplayCategory::Other
            } else {
                true
            };
            if meta_filtered && !(self.settings.show_quality_of_data && acronym == "M_QUAL") {
                return None;
            }
        }

        // Display category check. DEPCNT02 promotes the selected safety
        // contour to DISPLAYBASE, so it survives any display category.
        let category_hidden = !self.settings.should_show(entry.display_category)
            && !self.bypass_display_category_filter(feature.type_code)
            && !self.is_promoted_safety_contour(feature, ctx.safety_contour);
        // UDWHAZ03 promotes an isolated danger to DISPLAYBASE
        // (s52cnsy.cpp sets `m_DisplayCat = DISPLAYBASE`), but whether it is
        // one is only known once its CS has run. Rocks, wrecks and
        // obstructions are category OTHER, so testing the category first
        // hid the ISODGR51 danger symbol under the Standard display.
        const OBSTRN: u16 = 86;
        const UWTROC: u16 = 153;
        const WRECKS: u16 = 159;
        let may_be_isolated_danger = geom_type == GeometryType::Point
            && matches!(feature.type_code, OBSTRN | UWTROC | WRECKS)
            && !ctx.is_empty();
        if category_hidden && !may_be_isolated_danger {
            return None;
        }

        // Parse and expand instructions (with CS result caching)
        let raw_instructions = RenderInstruction::parse_all(&entry.instruction);
        let mut expanded = Vec::with_capacity(raw_instructions.len());
        // Lazy: only compute attr hash if we hit a CS instruction
        let mut cached_attr_hash: Option<u64> = Some(attr_hash);

        for instr in raw_instructions {
            match &instr {
                RenderInstruction::ConditionalSymbology { procedure } => {
                    let hash = *cached_attr_hash.get_or_insert_with(|| feature_attr_hash(feature));
                    let cache_key = CsCacheKey {
                        object_class: feature.type_code,
                        feature_type: feature.feature_type,
                        procedure: procedure.clone(),
                        attr_hash: hash,
                    };

                    // Check cache first
                    {
                        let cache = self.cs_cache.lock().unwrap();
                        if let Some(cached) = cache.get(&cache_key) {
                            expanded.extend(cached.clone());
                            continue;
                        }
                    }

                    // Cache miss: execute CS and store result
                    if let Some(cs_results) = execute_cs(procedure, feature, &self.settings, ctx) {
                        self.cs_cache.lock().unwrap().insert(cache_key, cs_results.clone());
                        expanded.extend(cs_results);
                    }
                }
                _ => expanded.push(instr),
            }
        }

        let mut category = entry.display_category;
        if category_hidden {
            let isolated = expanded.iter().any(
                |i| matches!(i, RenderInstruction::Symbol { name, .. } if name == "ISODGR51"),
            );
            if !isolated {
                self.resolve_cache.lock().unwrap().insert(resolve_key, None);
                return None;
            }
            category = DisplayCategory::Displaybase;
        }

        let resolved = Some(ResolvedFeature {
            priority: entry.display_priority.as_u8(),
            category,
            instructions: expanded,
        });
        self.resolve_cache
            .lock()
            .unwrap()
            .insert(resolve_key, resolved.clone());
        resolved
    }

    /// Resolve a feature for the conformance harness (`manx --dump-ir`).
    ///
    /// Same pipeline as [`resolve_feature`] but with nothing filtered out: the
    /// chosen lookup entry is returned even when the display category would
    /// hide it, and CS procedures are expanded regardless. That matches what
    /// tools/s52oracle reports from OpenCPN's engine, so a diff of the two
    /// streams isolates symbology-resolution differences from visibility rules.
    pub fn resolve_ir(
        &self,
        feature: &Feature,
        geom_type: GeometryType,
        ctx: &CsContext,
    ) -> (Option<&LookupEntry>, Vec<RenderInstruction>) {
        let acronym = s57_code_to_acronym(feature.type_code);
        let table = TableName::preferred_ext(
            geom_type,
            self.settings.symbolized_boundaries,
            self.settings.simplified_points,
        );
        let Some(entry) = self.tables.lookup_best_fast(acronym, geom_type, table, feature) else {
            return (None, Vec::new());
        };

        let mut expanded = Vec::new();
        for instr in RenderInstruction::parse_all(&entry.instruction) {
            match &instr {
                RenderInstruction::ConditionalSymbology { procedure } => {
                    if let Some(results) = execute_cs(procedure, feature, &self.settings, ctx) {
                        expanded.extend(results);
                    }
                }
                _ => expanded.push(instr),
            }
        }
        (Some(entry), expanded)
    }

    /// The display category of the lookup row that actually applies to this
    /// feature.
    ///
    /// [`get_display_category`] answers for the object *class*, taking the
    /// first row it finds; where rows differ by attribute — an OBSTRN that is
    /// DISPLAYBASE only when WATLEV says it is awash — that is a different row
    /// from the one portrayal uses. The SCAMIN bypass has to agree with
    /// portrayal, or a safety-critical object gets filtered by a SCAMIN that
    /// S-52 says must not apply to it.
    ///
    /// [`get_display_category`]: S52Engine::get_display_category
    pub fn display_category_for(
        &self,
        feature: &Feature,
        geom_type: GeometryType,
    ) -> Option<DisplayCategory> {
        let acronym = s57_code_to_acronym(feature.type_code);
        let table = TableName::preferred_ext(
            geom_type,
            self.settings.symbolized_boundaries,
            self.settings.simplified_points,
        );
        self.tables
            .lookup_best_fast(acronym, geom_type, table, feature)
            .map(|e| e.display_category)
    }

    /// Get display priority for a feature from its best matching lookup entry.
    /// Returns default priority (0) if no lookup found.
    pub fn get_display_priority(
        &self,
        feature: &Feature,
        geom_type: GeometryType,
    ) -> u8 {
        let acronym = s57_code_to_acronym(feature.type_code);
        let table = TableName::preferred_ext(
            geom_type,
            self.settings.symbolized_boundaries,
            self.settings.simplified_points,
        );
        self.tables
            .lookup_best_fast(acronym, geom_type, table, feature)
            .map(|e| e.display_priority.as_u8())
            .unwrap_or(0)
    }

    /// Check if a feature should be rendered at the given scale.
    ///
    /// Returns Some(lookup_entry) if feature should be rendered, None to skip.
    /// DISPLAYBASE and GROUP1 features bypass SCAMIN (always visible for safety).
    pub fn should_render(
        &self,
        feature: &Feature,
        geom_type: GeometryType,
        view_scale: f64,
    ) -> Option<&LookupEntry> {
        // 1. Lookup in chartsymbols.xml first (need category for SCAMIN bypass)
        let acronym = s57_code_to_acronym(feature.type_code);
        let table = TableName::preferred_ext(
            geom_type,
            self.settings.symbolized_boundaries,
            self.settings.simplified_points,
        );
        let entry = self.tables.lookup_best_fast(acronym, geom_type, table, feature)?;

        // 2. SCAMIN/SCAMAX check - bypass for safety-critical features
        let is_safety = entry.display_category == DisplayCategory::Displaybase
            || entry.display_priority == DisplayPriority::Group1;

        if !is_safety {
            if let Some(scamin) = feature.scamin() {
                if view_scale > scamin {
                    return None;
                }
            }
            if let Some(scamax) = feature.scamax() {
                if view_scale < scamax {
                    return None;
                }
            }
        }

        // 3. Display category check
        if !self.settings.should_show(entry.display_category)
            && !self.bypass_display_category_filter(feature.type_code)
        {
            return None;
        }

        Some(entry)
    }

    /// Check if an object class should be rendered (quick check without attributes)
    pub fn should_render_class(
        &self,
        type_code: u16,
        geom_type: GeometryType,
    ) -> bool {
        let acronym = s57_code_to_acronym(type_code);
        let table = TableName::preferred_ext(
            geom_type,
            self.settings.symbolized_boundaries,
            self.settings.simplified_points,
        );

        // Look up first entry for this class
        if let Some(entries) = self.tables.lookup(acronym, geom_type, table) {
            if let Some(entry) = entries.first() {
                return self.settings.should_show(entry.display_category)
                    || self.bypass_display_category_filter(type_code);
            }
        }

        // Unknown class - default to showing it
        true
    }

    /// Get display category for an object class by type code
    pub fn get_display_category(
        &self,
        type_code: u16,
        geom_type: GeometryType,
    ) -> Option<DisplayCategory> {
        let acronym = s57_code_to_acronym(type_code);
        let table = TableName::preferred_ext(
            geom_type,
            self.settings.symbolized_boundaries,
            self.settings.simplified_points,
        );
        self.tables
            .lookup(acronym, geom_type, table)?
            .first()
            .map(|e| e.display_category)
    }

    /// Get color from lookup tables by name
    pub fn get_color(&self, name: &str) -> Option<[f32; 4]> {
        self.tables.get_color_f32(name)
    }

    /// Get color index for a token name (for palette-indexed rendering)
    pub fn get_color_index(&self, name: &str) -> Option<u16> {
        self.tables.get_color_index(name)
    }

    /// Get area color index for a feature from chartsymbols.xml.
    /// Returns the palette color index from the first AC instruction, or None if not found.
    /// Also handles CS() instructions that may return AC() results.
    pub fn get_area_color_index(&self, feature: &Feature) -> Option<u16> {
        let instructions = self.get_instructions(feature, GeometryType::Area)?;
        for instr in &instructions {
            if let super::instruction::RenderInstruction::AreaColor { color, .. } = instr {
                return self.get_color_index(color);
            }
        }
        // If no direct AC found, try CS procedures that may produce AC results
        for instr in &instructions {
            if let super::instruction::RenderInstruction::ConditionalSymbology { procedure } = instr {
                if let Some(cs_instructions) =
                    super::cs::execute_cs(procedure, feature, &self.settings, CsContext::EMPTY)
                {
                    for cs_instr in &cs_instructions {
                        if let super::instruction::RenderInstruction::AreaColor { color, .. } = cs_instr {
                            return self.get_color_index(color);
                        }
                    }
                }
            }
        }
        None
    }

    /// Get parsed render instructions for a feature.
    /// Looks up the feature in chartsymbols.xml and parses the instruction string.
    pub fn get_instructions(
        &self,
        feature: &Feature,
        geom_type: GeometryType,
    ) -> Option<Vec<super::instruction::RenderInstruction>> {
        let acronym = s57_code_to_acronym(feature.type_code);
        let table = TableName::preferred_ext(
            geom_type,
            self.settings.symbolized_boundaries,
            self.settings.simplified_points,
        );
        let entry = self.tables.lookup_best_fast(acronym, geom_type, table, feature)?;
        Some(super::instruction::RenderInstruction::parse_all(&entry.instruction))
    }

    /// Get area color for a feature from chartsymbols.xml.
    /// Returns the first AC instruction color, or None if not found.
    /// Also handles CS() instructions that may return AC() results.
    pub fn get_area_color(&self, feature: &Feature) -> Option<[f32; 4]> {
        let instructions = self.get_instructions(feature, GeometryType::Area)?;
        for instr in &instructions {
            if let super::instruction::RenderInstruction::AreaColor { color, .. } = instr {
                return self.get_color(color);
            }
        }
        // If no direct AC found, try CS procedures that may produce AC results
        for instr in &instructions {
            if let super::instruction::RenderInstruction::ConditionalSymbology { procedure } = instr {
                if let Some(cs_instructions) =
                    super::cs::execute_cs(procedure, feature, &self.settings, CsContext::EMPTY)
                {
                    for cs_instr in &cs_instructions {
                        if let super::instruction::RenderInstruction::AreaColor { color, .. } = cs_instr {
                            return self.get_color(color);
                        }
                    }
                }
            }
        }
        None
    }

    /// Get line style parameters for a feature from chartsymbols.xml.
    /// Returns (pattern, width, color_rgba) from the first LS instruction found.
    pub fn get_line_style_params(
        &self,
        feature: &Feature,
    ) -> Option<(super::instruction::LinePattern, u8, [f32; 4])> {
        let instructions = self.get_instructions(feature, GeometryType::Line)?;
        for instr in instructions {
            if let super::instruction::RenderInstruction::LineStyle {
                pattern,
                width,
                color,
            } = instr
            {
                if let Some(rgba) = self.get_color(&color) {
                    return Some((pattern, width, rgba));
                }
            }
        }
        None
    }

    /// Summary for debugging
    pub fn summary(&self) -> String {
        format!(
            "S52Engine: {} entries, {} colors, settings: disp={}/{}/{} other={}",
            self.tables.entry_count(),
            self.tables.color_count(),
            if self.settings.show_displaybase { "B" } else { "-" },
            if self.settings.show_standard { "S" } else { "-" },
            if self.settings.show_other { "O" } else { "-" },
            self.settings.show_other,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::senc::{AttributeValue, FeatureType, ObjectClass};

    const CHARTSYMBOLS: &str = "assets/s52/chartsymbols.xml";

    /// An engine against the contours these cases were written for, so they
    /// do not drift with `MarinerSettings::default()`.
    fn engine(show_other: bool) -> S52Engine {
        let mut engine = S52Engine::load(CHARTSYMBOLS).expect("chartsymbols.xml loads");
        engine.set_settings(MarinerSettings {
            safety_depth: 10.0,
            safety_contour: 10.0,
            shallow_contour: 2.0,
            deep_contour: 30.0,
            show_other,
            ..Default::default()
        });
        engine
    }

    fn feature(
        type_code: u16,
        object_class: ObjectClass,
        feature_type: FeatureType,
        attrs: &[(&str, AttributeValue)],
    ) -> Feature {
        let mut attributes = crate::senc::Attributes::new();
        for (k, v) in attrs {
            attributes.insert(k, v.clone());
        }
        Feature {
            type_code,
            object_class,
            feature_type,
            attributes,
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        }
    }

    fn rock(valsou: f64) -> Feature {
        feature(
            153,
            ObjectClass::UnderwaterRock,
            FeatureType::Point,
            &[
                ("VALSOU", AttributeValue::Float(valsou)),
                ("WATLEV", AttributeValue::Integer(3)),
            ],
        )
    }

    fn water(drval1: f64) -> CsContext {
        CsContext {
            area_drval1: vec![drval1],
            ..Default::default()
        }
    }

    fn s52(resolved: &Option<ResolvedFeature>) -> Vec<String> {
        resolved
            .as_ref()
            .map(|r| r.instructions.iter().map(|i| i.to_s52()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn test_engine_load() {
        let engine = S52Engine::load(CHARTSYMBOLS).expect("Failed to load");

        // Check display category for ROADWY (code 116, should be "Other")
        let cat = engine.get_display_category(116, GeometryType::Line);
        assert_eq!(cat, Some(DisplayCategory::Other));

        // Default settings show the OTHER category ("ENC display: All"), so
        // roads render; selecting STANDARD hides them again.
        assert!(engine.should_render_class(116, GeometryType::Line));
        let mut engine_std = engine;
        let mut settings = engine_std.settings.clone();
        settings.show_other = false;
        engine_std.set_settings(settings.clone());
        assert!(!engine_std.should_render_class(116, GeometryType::Line));
        settings.show_other = true;
        engine_std.set_settings(settings);
        let engine = engine_std;

        // Coastlines (code 30) should always render (Displaybase)
        let cat = engine.get_display_category(30, GeometryType::Line);
        assert_eq!(cat, Some(DisplayCategory::Displaybase));
        assert!(engine.should_render_class(30, GeometryType::Line));
    }

    /// Rocks are category OTHER, but UDWHAZ03 promotes an isolated danger to
    /// DISPLAYBASE. Under the Standard display the ISODGR51 symbol must
    /// still be drawn — it is the one symbol that warns of a rock in water
    /// the mariner would otherwise cross.
    #[test]
    fn isolated_danger_survives_the_standard_display() {
        let engine = engine(false);
        let resolved = engine.resolve_feature_ctx(&rock(5.0), GeometryType::Point, &water(20.0));
        assert_eq!(s52(&resolved), ["SY(ISODGR51)"]);
        assert_eq!(resolved.unwrap().category, DisplayCategory::Displaybase);

        // The same rock inside shallow water is an ordinary OTHER object and
        // the Standard display hides it.
        assert!(engine
            .resolve_feature_ctx(&rock(5.0), GeometryType::Point, &water(0.0))
            .is_none());
    }

    /// The caches key on the surroundings: the rock resolved first in
    /// shallow water must not be served from the cache when it is asked
    /// about again in deep water, or vice versa.
    #[test]
    fn surroundings_are_part_of_the_cache_key() {
        let engine = engine(true);
        let shallow = engine.resolve_feature_ctx(&rock(5.0), GeometryType::Point, &water(0.0));
        assert_eq!(s52(&shallow)[0], "SY(DANGER51)");
        let deep = engine.resolve_feature_ctx(&rock(5.0), GeometryType::Point, &water(20.0));
        assert_eq!(s52(&deep), ["SY(ISODGR51)"]);
        let shallow = engine.resolve_feature_ctx(&rock(5.0), GeometryType::Point, &water(0.0));
        assert_eq!(s52(&shallow)[0], "SY(DANGER51)");
        assert_eq!(shallow.unwrap().category, DisplayCategory::Other);
    }

    /// OBSTRN04 draws a point and an area completely differently; two
    /// obstructions with identical attributes but different primitives must
    /// not share a CS cache entry.
    #[test]
    fn point_and_area_obstructions_do_not_share_cs_results() {
        let engine = engine(true);
        let attrs = [("VALSOU", AttributeValue::Float(15.0))];
        let point = feature(86, ObjectClass::Obstruction, FeatureType::Point, &attrs);
        let area = feature(86, ObjectClass::Obstruction, FeatureType::Area, &attrs);

        let p = s52(&engine.resolve_feature(&point, GeometryType::Point));
        assert_eq!(p[0], "SY(DANGER51)");
        let a = s52(&engine.resolve_feature(&area, GeometryType::Area));
        assert_eq!(a[0], "LS(DOTT,2,CHBLK)");
        assert!(a.iter().all(|i| !i.starts_with("SY(")), "area drew a symbol: {a:?}");
    }

    /// DEPCNT02 promotes the safety contour to DISPLAYBASE: it survives the
    /// Standard display that hides every other contour.
    #[test]
    fn safety_contour_survives_the_standard_display() {
        let engine = engine(false);
        let contour = |valdco: f64| {
            feature(43, ObjectClass::DepthContour, FeatureType::Line, &[("VALDCO", AttributeValue::Float(valdco))])
        };
        let safety = engine.resolve_feature(&contour(10.0), GeometryType::Line);
        assert_eq!(s52(&safety), ["LS(SOLD,2,DEPSC)"]);
        assert!(engine.resolve_feature(&contour(20.0), GeometryType::Line).is_none());

        // With the chart's own selection the 20 m line is the safety contour.
        let selected = CsContext {
            safety_contour: Some(20.0),
            ..Default::default()
        };
        let resolved = engine.resolve_feature_ctx(&contour(20.0), GeometryType::Line, &selected);
        assert_eq!(s52(&resolved), ["LS(SOLD,2,DEPSC)"]);
    }

    /// The CS cache is keyed without the settings, so `set_settings` must
    /// clear it — otherwise a new safety contour leaves every depth area
    /// painted for the old one.
    #[test]
    fn changing_contours_reshades_depth_areas() {
        let mut engine = engine(true);
        let area = feature(
            42,
            ObjectClass::DepthArea,
            FeatureType::Area,
            &[("DRVAL1", AttributeValue::Float(5.0)), ("DRVAL2", AttributeValue::Float(15.0))],
        );
        let before = engine.resolve_feature(&area, GeometryType::Area);
        assert_eq!(s52(&before), ["AC(DEPMS)"]); // 5 m: past shallow (2), short of safety (10)

        let mut settings = engine.settings.clone();
        settings.safety_contour = 5.0;
        engine.set_settings(settings);
        let after = engine.resolve_feature(&area, GeometryType::Area);
        assert_eq!(s52(&after), ["AC(DEPMD)"]);
    }

    #[test]
    fn chart_safety_contour_is_the_next_deeper_depth_the_chart_has() {
        let mut engine = engine(true);
        let mut settings = engine.settings.clone();
        settings.safety_contour = 3.0;
        engine.set_settings(settings);
        let line = |v: f64| {
            feature(43, ObjectClass::DepthContour, FeatureType::Line, &[("VALDCO", AttributeValue::Float(v))])
        };
        let area = |d1: f64, d2: f64| {
            feature(
                42,
                ObjectClass::DepthArea,
                FeatureType::Area,
                &[("DRVAL1", AttributeValue::Float(d1)), ("DRVAL2", AttributeValue::Float(d2))],
            )
        };
        // The shape of cell E94704: lines at 2, 4, 6 and 10 m, and a small
        // depth area limited at 3 m. The safety contour is the 4 m line —
        // a depth area's limit is not a line, and choosing it would leave
        // no line highlighted at all.
        let features = vec![line(2.0), line(4.0), line(6.0), line(10.0), area(2.0, 3.0), area(3.0, 4.0)];
        assert_eq!(engine.chart_safety_contour(&features), Some(4.0));
        // A setting that matches a line exactly selects that line.
        let features = vec![line(2.0), line(3.0), line(10.0)];
        assert_eq!(engine.chart_safety_contour(&features), Some(3.0));
        // The next line may be deep — still the edge of safe water (S-52;
        // OpenCPN drops it beyond the deep contour, Manx does not).
        let features = vec![line(10.0), line(20.0), area(0.0, 10.0)];
        assert_eq!(engine.chart_safety_contour(&features), Some(10.0));
        // No line at or below the setting: nothing to highlight.
        let features = vec![line(1.0), line(2.0), area(2.0, 5.0)];
        assert_eq!(engine.chart_safety_contour(&features), None);
    }

    /// An engine with the given display categories switched on, against the
    /// same pinned contours as [`engine`]. `(false, false)` is OpenCPN's
    /// "Base" display, `(true, false)` Standard and `(true, true)` All.
    fn engine_showing(standard: bool, other: bool) -> S52Engine {
        let mut engine = engine(other);
        let mut settings = engine.settings.clone();
        settings.show_standard = standard;
        engine.set_settings(settings);
        engine
    }

    fn with_settings(mut engine: S52Engine, change: impl FnOnce(&mut MarinerSettings)) -> S52Engine {
        let mut settings = engine.settings.clone();
        change(&mut settings);
        engine.set_settings(settings);
        engine
    }

    fn road(attrs: &[(&str, AttributeValue)]) -> Feature {
        feature(116, ObjectClass::Road, FeatureType::Line, attrs)
    }

    /// M_NSYS with no attributes: the LC(MARSYS51) row, category STANDARD.
    fn m_nsys() -> Feature {
        feature(306, ObjectClass::Other, FeatureType::Area, &[])
    }

    /// M_QUAL with CATZOC 1: AP(DQUALA11);LS(DASH,2,CHGRD), category OTHER.
    fn m_qual() -> Feature {
        feature(308, ObjectClass::QualityOfData, FeatureType::Area, &[("CATZOC", AttributeValue::Integer(1))])
    }

    fn depth_area(drval1: f64, drval2: f64) -> Feature {
        feature(
            42,
            ObjectClass::DepthArea,
            FeatureType::Area,
            &[("DRVAL1", AttributeValue::Float(drval1)), ("DRVAL2", AttributeValue::Float(drval2))],
        )
    }

    /// `settings` is public, so a caller can change it without going through
    /// `set_settings` and its cache flush. The resolve cache keys on a hash
    /// of the settings for exactly that case: a toggle must not be answered
    /// with the verdict cached under the old settings.
    #[test]
    fn settings_changed_in_place_are_not_answered_from_the_cache() {
        let mut engine = engine(true);
        let roadway = road(&[]);
        // ROADWY is category OTHER: drawn under "All"...
        assert_eq!(s52(&engine.resolve_feature(&roadway, GeometryType::Line)), ["LS(SOLD,2,LANDF)"]);
        // ...and hidden under Standard, even though the "All" verdict is cached.
        engine.settings.show_other = false;
        assert!(engine.resolve_feature(&roadway, GeometryType::Line).is_none());
        // And back again.
        engine.settings.show_other = true;
        assert!(engine.resolve_feature(&roadway, GeometryType::Line).is_some());

        // The quality-of-data switch, one of the three settings the hash
        // comment singles out, toggled the same way.
        let mut engine = engine_showing(true, false);
        assert!(engine.resolve_feature(&m_qual(), GeometryType::Area).is_none());
        engine.settings.show_quality_of_data = true;
        assert!(engine.resolve_feature(&m_qual(), GeometryType::Area).is_some());
    }

    /// s52plib.cpp `ObjectRenderCheckCat`: under "All" the meta-object filter
    /// applies only to objects whose own category is OTHER, so M_NSYS
    /// (STANDARD) keeps its navigational-system boundary while M_QUAL (OTHER)
    /// is dropped. Under any narrower display every `M_*` class goes ("We
    /// want to filter out M_NSYS objects everywhere except OTHER category").
    #[test]
    fn meta_objects_follow_opencpns_category_filter() {
        let all = engine_showing(true, true);
        assert_eq!(s52(&all.resolve_feature(&m_nsys(), GeometryType::Area)), ["LC(MARSYS51)"]);
        assert!(all.resolve_feature(&m_qual(), GeometryType::Area).is_none());

        let standard = engine_showing(true, false);
        assert!(standard.resolve_feature(&m_nsys(), GeometryType::Area).is_none());
        assert!(standard.resolve_feature(&m_qual(), GeometryType::Area).is_none());

        // Showing meta objects lifts the filter: M_NSYS is STANDARD and draws.
        let shown = with_settings(engine_showing(true, false), |s| s.show_meta_objects = true);
        assert_eq!(s52(&shown.resolve_feature(&m_nsys(), GeometryType::Area)), ["LC(MARSYS51)"]);
    }

    /// The quality-of-data switch forces M_QUAL visible (OpenCPN's
    /// `SetQualityOfData` forces its `nViz` on) — and only M_QUAL: it is not a
    /// licence for every other meta object or OTHER-category feature.
    #[test]
    fn quality_of_data_rescues_m_qual_and_nothing_else() {
        let standard = with_settings(engine_showing(true, false), |s| s.show_quality_of_data = true);
        assert_eq!(
            s52(&standard.resolve_feature(&m_qual(), GeometryType::Area)),
            ["AP(DQUALA11)", "LS(DASH,2,CHGRD)"]
        );
        assert!(standard.resolve_feature(&m_nsys(), GeometryType::Area).is_none());
        assert!(standard.resolve_feature(&road(&[]), GeometryType::Line).is_none());
        assert!(standard.should_render(&road(&[]), GeometryType::Line, 10_000.0).is_none());
        assert!(standard.should_render(&m_qual(), GeometryType::Area, 10_000.0).is_some());

        // Under "All", M_QUAL is still the quality switch's to show.
        let all = with_settings(engine_showing(true, true), |s| s.show_quality_of_data = true);
        assert!(all.resolve_feature(&m_qual(), GeometryType::Area).is_some());
    }

    /// OpenCPN hides M_QUAL whenever "Quality of data" is off: the
    /// constructor and `SetQualityOfData(false)` put it on the no-show list,
    /// which `ObjectRenderCheckRules` honours in every display category, and
    /// `ObjectRenderCheckCat` drops it under "All" even with meta objects on
    /// (`if (!strncmp(OBCL, "M_QUAL", 6) && !m_qualityOfDataOn) return false`).
    #[test]
    fn showing_meta_objects_does_not_switch_on_quality_of_data() {
        let all = with_settings(engine_showing(true, true), |s| s.show_meta_objects = true);
        assert!(all.resolve_feature(&m_qual(), GeometryType::Area).is_none());
    }

    /// UDWHAZ03 is the only procedure that promotes to DISPLAYBASE
    /// (s52cnsy.cpp sets `m_DisplayCat` there and in DEPCNT02, nowhere else),
    /// so only rocks, wrecks and obstructions get the second look. A new
    /// object whose SYMINS happens to draw ISODGR51 keeps its own category:
    /// SYMINS01 hands the string back and promotes nothing.
    #[test]
    fn only_danger_classes_are_promoted_by_the_isolated_danger_symbol() {
        let base = engine_showing(false, false);
        let newobj = feature(
            163,
            ObjectClass::Other,
            FeatureType::Point,
            &[("SYMINS", AttributeValue::String("SY(ISODGR51)".into()))],
        );
        assert!(base.resolve_feature_ctx(&newobj, GeometryType::Point, &water(20.0)).is_none());

        // The same display does promote a rock in deep water.
        let resolved = base.resolve_feature_ctx(&rock(5.0), GeometryType::Point, &water(20.0));
        assert_eq!(s52(&resolved), ["SY(ISODGR51)"]);

        // NEWOBJ is STANDARD, so the Standard display draws its SYMINS as is.
        let standard = engine_showing(true, false);
        let resolved = standard.resolve_feature_ctx(&newobj, GeometryType::Point, &water(20.0));
        assert_eq!(s52(&resolved), ["SY(ISODGR51)"]);
        assert_eq!(resolved.unwrap().category, DisplayCategory::Standard);
    }

    /// Both caches key on the feature's attribute hash XOR its surroundings'
    /// hash. For a fixed feature XOR keeps every distinct surrounding
    /// distinct; OR and AND fold some pairs together, and then the same rock
    /// in deep water is answered with what it resolved to in shallow water.
    /// Search for a pair each lossy combination would merge and check the
    /// engine still tells them apart.
    #[test]
    fn surroundings_a_lossy_hash_would_merge_still_resolve_apart() {
        type Combine = fn(u64, u64) -> u64;
        type Suitable = fn(u32) -> bool;
        let lossy: [(&str, Combine, Suitable); 2] = [
            // OR keeps the feature's one-bits: few zero-bits leave few for
            // the surroundings to vary, so collisions come quickly.
            ("or", |f, c| f | c, |ones| ones >= 42),
            ("and", |f, c| f & c, |ones| ones <= 22),
        ];
        for (name, combine, suitable) in lossy {
            // SCAMIN does not enter portrayal; it only changes the hash.
            let rock = (0..200_000)
                .map(|k| {
                    let mut r = rock(5.0);
                    r.attributes.insert("SCAMIN", AttributeValue::Integer(k));
                    r
                })
                .find(|r| suitable(feature_attr_hash(r).count_ones()))
                .expect("a rock whose hash suits the search");
            let f = feature_attr_hash(&rock);

            // Surrounding depth areas from 0 to 20 m; 10 m is the safety
            // contour, so one side is an isolated danger and the other not.
            let mut seen: HashMap<u64, f64> = HashMap::new();
            let (a, b) = (0..2_000_000)
                .find_map(|i| {
                    let d = i as f64 * 0.00001;
                    let key = combine(f, water(d).hash_value());
                    match seen.get(&key) {
                        Some(&other) if (other >= 10.0) != (d >= 10.0) => Some((other, d)),
                        Some(_) => None,
                        None => {
                            seen.insert(key, d);
                            None
                        }
                    }
                })
                .unwrap_or_else(|| panic!("no surroundings merge under {name}"));

            let engine = engine(true);
            for d in [a, b] {
                let resolved = s52(&engine.resolve_feature_ctx(&rock, GeometryType::Point, &water(d)));
                let expected = if d >= 10.0 { "SY(ISODGR51)" } else { "SY(DANGER51)" };
                assert_eq!(resolved[0], expected, "{name}: rock in {d} m of water");
            }
        }
    }

    /// The resolve and CS caches key on `attributes_as_map`, which truncates
    /// floats to integers. DRVAL1 -0.5 (drying) and 0.5 both hash as 0, so
    /// whichever depth area resolves first decides the colour of both.
    /// s52cnsy.cpp DEPARE01/SEABED01 starts from DEPIT and moves to DEPVS
    /// only when `drval1 >= 0.0 && drval2 > 0.0`.
    #[test]
    fn depth_areas_either_side_of_the_datum_do_not_share_a_cache_entry() {
        let engine = engine(true);
        let drying = engine.resolve_feature(&depth_area(-0.5, 1.0), GeometryType::Area);
        assert_eq!(s52(&drying), ["AC(DEPIT)"]);
        let wet = engine.resolve_feature(&depth_area(0.5, 1.0), GeometryType::Area);
        assert_eq!(s52(&wet), ["AC(DEPVS)"]);
    }

    /// `attributes_as_map` keeps only the integers it can parse out of a
    /// string attribute, so every SYMINS hashes the same. s52cnsy.cpp
    /// SYMINS01 returns each feature's own SYMINS string; two virtual aids
    /// with different instructions must draw differently.
    #[test]
    fn new_objects_with_different_symins_draw_their_own() {
        let engine = engine(true);
        let newobj = |symins: &str| {
            feature(163, ObjectClass::Other, FeatureType::Point, &[("SYMINS", AttributeValue::String(symins.into()))])
        };
        let buoy = engine.resolve_feature(&newobj("SY(BOYSAW12)"), GeometryType::Point);
        assert_eq!(s52(&buoy), ["SY(BOYSAW12)"]);
        let beacon = engine.resolve_feature(&newobj("SY(BCNSAW13)"), GeometryType::Point);
        assert_eq!(s52(&beacon), ["SY(BCNSAW13)"]);
    }

    /// The conformance harness's view: the lookup row and the expanded
    /// instructions, with nothing hidden by the display category.
    #[test]
    fn resolve_ir_expands_cs_and_ignores_the_display_category() {
        let standard = engine_showing(true, false);
        // ROADWY is OTHER, hidden from the Standard display, but the IR still
        // reports its row: LS(SOLD,2,LANDF) from chartsymbols.xml.
        let (entry, ir) = standard.resolve_ir(&road(&[]), GeometryType::Line, CsContext::EMPTY);
        assert_eq!(entry.map(|e| e.display_category), Some(DisplayCategory::Other));
        assert_eq!(ir.iter().map(|i| i.to_s52()).collect::<Vec<_>>(), ["LS(SOLD,2,LANDF)"]);

        // DEPARE's row is CS(DEPARE01); the IR carries what it expands to,
        // not the call. 5..15 m: past shallow (2), short of safety (10).
        let (entry, ir) = standard.resolve_ir(&depth_area(5.0, 15.0), GeometryType::Area, CsContext::EMPTY);
        assert!(entry.is_some_and(|e| e.instruction.contains("CS(DEPARE")));
        assert_eq!(ir.iter().map(|i| i.to_s52()).collect::<Vec<_>>(), ["AC(DEPMS)"]);
    }

    /// The row that applies to the feature, not the class's first row: OBSTRN
    /// points are OTHER except the WATLEV 7 (floating) row, which is
    /// DISPLAYBASE.
    #[test]
    fn display_category_follows_the_row_the_attributes_select() {
        let engine = engine(true);
        let floating = feature(86, ObjectClass::Obstruction, FeatureType::Point, &[("WATLEV", AttributeValue::Integer(7))]);
        assert_eq!(engine.get_display_category(86, GeometryType::Point), Some(DisplayCategory::Other));
        assert_eq!(engine.display_category_for(&floating, GeometryType::Point), Some(DisplayCategory::Displaybase));
        assert_eq!(engine.display_category_for(&road(&[]), GeometryType::Line), Some(DisplayCategory::Other));
    }

    /// chartsymbols.xml: COALNE lines are "Routing" (7), ROADWY lines
    /// "Point Symbol" (4), LNDARE areas "Group 1" (1).
    #[test]
    fn display_priority_comes_from_the_lookup_row() {
        let engine = engine(true);
        let coastline = feature(30, ObjectClass::Coastline, FeatureType::Line, &[]);
        assert_eq!(engine.get_display_priority(&coastline, GeometryType::Line), 7);
        assert_eq!(engine.get_display_priority(&road(&[]), GeometryType::Line), 4);
        let land = feature(71, ObjectClass::LandArea, FeatureType::Area, &[]);
        assert_eq!(engine.get_display_priority(&land, GeometryType::Area), 1);
        assert_eq!(engine.resolve_feature(&coastline, GeometryType::Line).unwrap().priority, 7);
    }

    /// SCAMIN hides an object once the display scale is *smaller* than it:
    /// s52plib.cpp `ObjectRenderCheckCat` drops it only when
    /// `chart_scale > Scamin`, so at exactly SCAMIN it still shows. SCAMAX
    /// (S-57: the largest scale the object may be used at) is the mirror image.
    #[test]
    fn scamin_and_scamax_bound_the_scales_an_object_shows_at() {
        let engine = engine(true);
        let roadway = road(&[("SCAMIN", AttributeValue::Integer(20_000))]);
        assert!(engine.should_render(&roadway, GeometryType::Line, 19_999.0).is_some());
        assert!(engine.should_render(&roadway, GeometryType::Line, 20_000.0).is_some());
        assert!(engine.should_render(&roadway, GeometryType::Line, 20_001.0).is_none());

        let roadway = road(&[("SCAMAX", AttributeValue::Integer(5_000))]);
        assert!(engine.should_render(&roadway, GeometryType::Line, 4_999.0).is_none());
        assert!(engine.should_render(&roadway, GeometryType::Line, 5_000.0).is_some());
        assert!(engine.should_render(&roadway, GeometryType::Line, 5_001.0).is_some());

        let returned = engine.should_render(&road(&[]), GeometryType::Line, 1.0e6);
        assert_eq!(returned.map(|e| e.instruction.as_str()), Some("LS(SOLD,2,LANDF)"));
    }

    /// s52plib.cpp: "According to S52 specs, SCAMIN must not apply to GROUP1
    /// objects, Meta Objects or DisplayCategoryBase objects" — either one
    /// suffices.
    #[test]
    fn displaybase_or_group_one_objects_ignore_scamin() {
        let engine = engine(true);
        let scamin = ("SCAMIN", AttributeValue::Integer(1_000));
        // A floating obstruction: DISPLAYBASE, priority "Point Symbol".
        let floating = feature(
            86,
            ObjectClass::Obstruction,
            FeatureType::Point,
            &[("WATLEV", AttributeValue::Integer(7)), scamin.clone()],
        );
        assert!(engine.should_render(&floating, GeometryType::Point, 50_000.0).is_some());
        // A dredged area: STANDARD, priority "Group 1".
        let dredged = feature(46, ObjectClass::DredgedArea, FeatureType::Area, &[scamin.clone()]);
        assert!(engine.should_render(&dredged, GeometryType::Area, 50_000.0).is_some());
        // A road is neither, and SCAMIN applies.
        assert!(engine.should_render(&road(&[scamin]), GeometryType::Line, 50_000.0).is_none());
    }

    #[test]
    fn should_render_honours_the_display_category() {
        let standard = engine_showing(true, false);
        assert!(standard.should_render(&road(&[]), GeometryType::Line, 10_000.0).is_none());
        let coastline = feature(30, ObjectClass::Coastline, FeatureType::Line, &[]);
        assert!(standard.should_render(&coastline, GeometryType::Line, 10_000.0).is_some());
        // M_QUAL is OTHER; with the quality switch off it stays hidden.
        assert!(standard.should_render(&m_qual(), GeometryType::Area, 10_000.0).is_none());
        // M_CSCL is OTHER too, but passes the category filter: it carries no
        // visible portrayal and the SCAMIN book-keeping needs it.
        let cscl = feature(301, ObjectClass::CompilationScale, FeatureType::Area, &[]);
        assert!(standard.should_render(&cscl, GeometryType::Area, 10_000.0).is_some());
        assert!(standard.should_render_class(301, GeometryType::Area));
        assert!(!standard.should_render_class(308, GeometryType::Area));
    }

    /// Colours come from the DAY_BRIGHT table, scaled to 0..1 with opaque
    /// alpha, and indices are positions in that table.
    #[test]
    fn colours_and_indices_come_from_the_day_bright_table() {
        let engine = engine(true);
        let rgb = |r: f32, g: f32, b: f32| Some([r / 255.0, g / 255.0, b / 255.0, 1.0]);
        // <color name="DEPDW" r="212" g="234" b="238"/>
        assert_eq!(engine.get_color("DEPDW"), rgb(212.0, 234.0, 238.0));
        assert_eq!(engine.get_color("NOSUCH"), None);
        // CHBLK is the third colour in DAY_BRIGHT, LANDA the twenty-second.
        assert_eq!(engine.get_color_index("CHBLK"), Some(2));
        assert_eq!(engine.get_color_index("LANDA"), Some(21));
        assert_eq!(engine.get_color_index("NOSUCH"), None);
    }

    /// LNDARE's area row is "AC(LANDA);TX(OBJNAM,…)"; DEPARE's is a CS call
    /// that produces the fill.
    #[test]
    fn area_colour_comes_from_the_first_fill_direct_or_from_cs() {
        let engine = engine(true);
        let land = feature(71, ObjectClass::LandArea, FeatureType::Area, &[]);
        let instructions = engine.get_instructions(&land, GeometryType::Area).unwrap();
        assert_eq!(
            instructions.iter().map(|i| i.to_s52()).collect::<Vec<_>>(),
            ["AC(LANDA)", "TX(OBJNAM,1,2,3,'15118',-1,-1,CHBLK,26)"]
        );
        // <color name="LANDA" r="201" g="185" b="122"/>, index 21.
        assert_eq!(engine.get_area_color(&land), Some([201.0 / 255.0, 185.0 / 255.0, 122.0 / 255.0, 1.0]));
        assert_eq!(engine.get_area_color_index(&land), Some(21));

        // 5..15 m resolves to DEPMS (152, 197, 242), the thirty-first colour.
        let depth = depth_area(5.0, 15.0);
        assert_eq!(engine.get_area_color(&depth), Some([152.0 / 255.0, 197.0 / 255.0, 242.0 / 255.0, 1.0]));
        assert_eq!(engine.get_area_color_index(&depth), Some(30));

        // COALNE has no area row at all; M_NSYS has one, but it only draws
        // a boundary.
        let coastline = feature(30, ObjectClass::Coastline, FeatureType::Line, &[]);
        assert_eq!(engine.get_area_color(&coastline), None);
        assert_eq!(engine.get_area_color_index(&coastline), None);
        assert_eq!(engine.get_area_color(&m_nsys()), None);
        assert_eq!(engine.get_area_color_index(&m_nsys()), None);
    }

    /// ROADWY's line row is LS(SOLD,2,LANDF); LANDF is (139, 102, 31).
    #[test]
    fn line_style_params_come_from_the_ls_row() {
        let engine = engine(true);
        let (pattern, width, rgba) = engine.get_line_style_params(&road(&[])).unwrap();
        assert_eq!(pattern, crate::s52::instruction::LinePattern::Solid);
        assert_eq!(width, 2);
        assert_eq!(rgba, [139.0 / 255.0, 102.0 / 255.0, 31.0 / 255.0, 1.0]);
    }

    /// The summary reports the table sizes and which categories are shown.
    #[test]
    fn summary_reports_tables_and_categories() {
        let standard = engine_showing(true, false);
        let summary = standard.summary();
        let sizes = format!("{} entries, {} colors", standard.tables.entry_count(), standard.tables.color_count());
        assert!(summary.contains(&sizes), "{summary}");
        assert!(standard.tables.entry_count() > 1000, "chartsymbols.xml has thousands of rows");
        assert!(summary.contains("disp=B/S/-"), "{summary}");
        assert!(engine_showing(false, true).summary().contains("disp=B/-/O"));
    }
}
