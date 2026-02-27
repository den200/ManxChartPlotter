//! S-52 presentation engine - decides what to render and how.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Mutex;

use crate::senc::{Feature, ObjectClass};

use super::cs::execute_cs;
use super::instruction::RenderInstruction;
use super::lookup::{DisplayCategory, DisplayPriority, GeometryType, LookupEntry, LookupTables};
use super::parser::parse_chartsymbols;
use super::settings::MarinerSettings;

/// Cache key for CS procedure results.
/// Combines object class, procedure name, and a hash of relevant attributes.
#[derive(Clone, Eq, PartialEq, Hash)]
struct CsCacheKey {
    object_class: u16,
    procedure: String,
    attr_hash: u64,
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
    /// Create engine by loading chartsymbols.xml
    pub fn load<P: AsRef<Path>>(chartsymbols_path: P) -> Result<Self, String> {
        let tables = parse_chartsymbols(chartsymbols_path)?;

        // Debug: verify depth colors are loaded
        let depth_colors = ["DEPIT", "DEPVS", "DEPMS", "DEPMD", "DEPDW"];
        for color_name in &depth_colors {
            if let Some([r, g, b]) = tables.get_color(color_name) {
                eprintln!("DEBUG S52: {} = RGB({},{},{})", color_name, r, g, b);
            } else {
                eprintln!("DEBUG S52: {} NOT FOUND in color table!", color_name);
            }
        }

        Ok(Self {
            tables,
            settings: MarinerSettings::default(),
            cs_cache: Mutex::new(HashMap::new()),
        })
    }

    /// Create engine with pre-loaded tables
    pub fn new(tables: LookupTables) -> Self {
        Self {
            tables,
            settings: MarinerSettings::default(),
            cs_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Update mariner settings. Invalidates CS result cache.
    pub fn set_settings(&mut self, settings: MarinerSettings) {
        self.settings = settings;
        self.cs_cache.lock().unwrap().clear();
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
        let acronym = feature.object_class.acronym();

        let entry = self.tables.lookup_best_fast(acronym, geom_type, feature)?;

        // Display category check
        if !self.settings.should_show(entry.display_category) {
            return None;
        }

        // Parse and expand instructions (with CS result caching)
        let raw_instructions = RenderInstruction::parse_all(&entry.instruction);
        let mut expanded = Vec::with_capacity(raw_instructions.len());
        // Lazy: only compute attr hash if we hit a CS instruction
        let mut attr_hash: Option<u64> = None;

        for instr in raw_instructions {
            match &instr {
                RenderInstruction::ConditionalSymbology { procedure } => {
                    let hash = *attr_hash.get_or_insert_with(|| feature_attr_hash(feature));
                    let cache_key = CsCacheKey {
                        object_class: feature.type_code,
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
                    if let Some(cs_results) = execute_cs(procedure, feature, &self.settings) {
                        self.cs_cache.lock().unwrap().insert(cache_key, cs_results.clone());
                        expanded.extend(cs_results);
                    }
                }
                _ => expanded.push(instr),
            }
        }

        Some(ResolvedFeature {
            priority: entry.display_priority.as_u8(),
            category: entry.display_category,
            instructions: expanded,
        })
    }

    /// Get display priority for a feature from its best matching lookup entry.
    /// Returns default priority (0) if no lookup found.
    pub fn get_display_priority(
        &self,
        feature: &Feature,
        geom_type: GeometryType,
    ) -> u8 {
        let acronym = feature.object_class.acronym();
        self.tables
            .lookup_best_fast(acronym, geom_type, feature)
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
        let acronym = feature.object_class.acronym();
        let entry = self.tables.lookup_best_fast(acronym, geom_type, feature)?;

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
        if !self.settings.should_show(entry.display_category) {
            return None;
        }

        Some(entry)
    }

    /// Check if an ObjectClass should be rendered (quick check without attributes)
    pub fn should_render_class(
        &self,
        object_class: ObjectClass,
        geom_type: GeometryType,
    ) -> bool {
        let acronym = object_class.acronym();

        // Look up first entry for this class
        if let Some(entries) = self.tables.lookup(acronym, geom_type) {
            if let Some(entry) = entries.first() {
                return self.settings.should_show(entry.display_category);
            }
        }

        // Unknown class - default to showing it
        true
    }

    /// Get display category for an object class
    pub fn get_display_category(
        &self,
        object_class: ObjectClass,
        geom_type: GeometryType,
    ) -> Option<DisplayCategory> {
        let acronym = object_class.acronym();
        self.tables
            .lookup(acronym, geom_type)?
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
            if let super::instruction::RenderInstruction::AreaColor { color } = instr {
                return self.get_color_index(color);
            }
        }
        // If no direct AC found, try CS procedures that may produce AC results
        for instr in &instructions {
            if let super::instruction::RenderInstruction::ConditionalSymbology { procedure } = instr {
                if let Some(cs_instructions) = super::cs::execute_cs(procedure, feature, &self.settings) {
                    for cs_instr in &cs_instructions {
                        if let super::instruction::RenderInstruction::AreaColor { color } = cs_instr {
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
        let acronym = feature.object_class.acronym();
        let entry = self.tables.lookup_best_fast(acronym, geom_type, feature)?;
        Some(super::instruction::RenderInstruction::parse_all(&entry.instruction))
    }

    /// Get area color for a feature from chartsymbols.xml.
    /// Returns the first AC instruction color, or None if not found.
    /// Also handles CS() instructions that may return AC() results.
    pub fn get_area_color(&self, feature: &Feature) -> Option<[f32; 4]> {
        let instructions = self.get_instructions(feature, GeometryType::Area)?;
        for instr in &instructions {
            if let super::instruction::RenderInstruction::AreaColor { color } = instr {
                return self.get_color(color);
            }
        }
        // If no direct AC found, try CS procedures that may produce AC results
        for instr in &instructions {
            if let super::instruction::RenderInstruction::ConditionalSymbology { procedure } = instr {
                if let Some(cs_instructions) = super::cs::execute_cs(procedure, feature, &self.settings) {
                    for cs_instr in &cs_instructions {
                        if let super::instruction::RenderInstruction::AreaColor { color } = cs_instr {
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

        // Check display category for ROADWY (should be "Other")
        let cat = engine.get_display_category(ObjectClass::Road, GeometryType::Line);
        assert_eq!(cat, Some(DisplayCategory::Other));

        // With default settings (show_other=false), roads should not render
        assert!(!engine.should_render_class(ObjectClass::Road, GeometryType::Line));

        // Coastlines should always render (Displaybase)
        let cat = engine.get_display_category(ObjectClass::Coastline, GeometryType::Line);
        assert_eq!(cat, Some(DisplayCategory::Displaybase));
        assert!(engine.should_render_class(ObjectClass::Coastline, GeometryType::Line));
    }
}
