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
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    // Hash attributes sorted by name for consistency
    let mut attrs: Vec<_> = feature.attributes_as_map().into_iter().collect();
    attrs.sort_by_key(|(k, _)| *k);
    for (name, values) in &attrs {
        name.hash(&mut hasher);
        values.hash(&mut hasher);
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
    /// navcore did neither, so with a 2 m safety contour three segments of it
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
        // DEPCNT02's "continuation A", but navcore draws that as an area
        // boundary, which is not SCAMIN-filtered separately.
        feature.type_code == 43 && super::cs::is_safety_contour(feature, &self.settings, selected)
    }

    /// The chart's selected safety contour: the shallowest DEPCNT value or
    /// depth-area limit at least as deep as the mariner's setting.
    pub fn chart_safety_contour(&self, features: &[crate::senc::Feature]) -> Option<f64> {
        let depths = features.iter().flat_map(|f| {
            let mut out = [None, None];
            match f.type_code {
                43 => out[0] = f.valdco(),
                // DEPARE: an area's limits are contours too, and some cells
                // carry depth areas without separate DEPCNT lines.
                42 => {
                    out[0] = f.drval1();
                    out[1] = f.drval2();
                }
                _ => {}
            }
            out.into_iter().flatten()
        });
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

    /// Resolve a feature for the conformance harness (`navcore --dump-ir`).
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

    #[test]
    fn test_engine_load() {
        let path = "assets/s52/chartsymbols.xml";
        if !Path::new(path).exists() {
            eprintln!("Skipping test - chartsymbols.xml not found");
            return;
        }

        let engine = S52Engine::load(path).expect("Failed to load");
        println!("{}", engine.summary());

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
}
