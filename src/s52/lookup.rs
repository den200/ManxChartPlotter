//! S-52 lookup table data structures.

use std::collections::HashMap;

/// S-52 display category - controls feature visibility based on user settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DisplayCategory {
    /// Always shown - safety-critical features (ECDIS requirement)
    Displaybase,
    /// Shown by default - standard chart features
    Standard,
    /// Hidden by default - supplementary information
    Other,
    /// Mariner-selectable - user can toggle
    Mariners,
}

impl DisplayCategory {
    /// Parse from XML string
    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim() {
            "Displaybase" => Some(Self::Displaybase),
            "Standard" => Some(Self::Standard),
            "Other" => Some(Self::Other),
            "Mariners" | "Mariners Standard" | "Mariners Other" => Some(Self::Mariners),
            _ => None,
        }
    }
}

/// S-52 display priority - controls draw order.
/// Higher priority features are drawn on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DisplayPriority {
    /// Group 1 areas (background)
    Group1,
    /// Area 1 (land, etc.)
    Area1,
    /// Area 2 (depth areas, etc.)
    Area2,
    /// Point symbols
    Point,
    /// Lines
    Line,
    /// Area pattern (hatching)
    AreaPattern,
    /// Area symbols (point symbols in area)
    AreaSymbol,
    /// Routing (traffic lanes, etc.)
    Routing,
    /// Hazards (wrecks, obstructions)
    Hazards,
    /// Mariners (user-added)
    Mariners,
}

impl DisplayPriority {
    /// Parse from XML string like "Area 2" or "Point Symbol"
    ///
    /// Handles variations from chartsymbols.xml disp-prio values:
    /// - "Point" / "Point Symbol"
    /// - "Line" / "Lines" / "Line Symbol"
    /// - "Area Symbol" / "Area Pattern"
    /// - "No data" (maps to Line as default)
    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim() {
            "Group 1" => Some(Self::Group1),
            "Area 1" => Some(Self::Area1),
            "Area 2" => Some(Self::Area2),
            "Point" | "Point Symbol" => Some(Self::Point),
            "Line" | "Lines" | "Line Symbol" | "No data" => Some(Self::Line),
            "Area Pattern" => Some(Self::AreaPattern),
            "Area Symbol" => Some(Self::AreaSymbol),
            "Routing" => Some(Self::Routing),
            "Hazards" => Some(Self::Hazards),
            "Mariners" | "Mariners Standard" | "Mariners Other" => Some(Self::Mariners),
            _ => None,
        }
    }

    /// Get numeric priority for sorting (higher = on top)
    pub fn as_u8(&self) -> u8 {
        match self {
            Self::Group1 => 0,
            Self::Area1 => 1,
            Self::Area2 => 2,
            Self::Point => 3,
            Self::Line => 4,
            Self::AreaPattern => 5,
            Self::AreaSymbol => 6,
            Self::Routing => 7,
            Self::Hazards => 8,
            Self::Mariners => 9,
        }
    }
}

/// Geometry type for lookup table
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GeometryType {
    Point,
    Line,
    Area,
}

impl GeometryType {
    /// Parse from XML string
    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim() {
            "Point" => Some(Self::Point),
            "Line" => Some(Self::Line),
            "Area" => Some(Self::Area),
            _ => None,
        }
    }
}

/// A single lookup table entry from chartsymbols.xml
#[derive(Debug, Clone)]
pub struct LookupEntry {
    /// S-57 object class name (e.g., "ACHARE", "SLCONS")
    pub object_class: String,
    /// Geometry type this entry applies to
    pub geometry_type: GeometryType,
    /// Display priority (draw order)
    pub display_priority: DisplayPriority,
    /// Display category (visibility filtering)
    pub display_category: DisplayCategory,
    /// Attribute codes for filtering (e.g., ["CATSLC1", "WATLEV3"]).
    /// Each code is 6-char S-57 attribute name + value digits.
    /// Empty vec means entry applies to all features of this class.
    pub attribute_codes: Vec<String>,
    /// Rendering instruction string (e.g., "LS(SOLD,2,CSTLN);SY(ACHARE02)")
    pub instruction: String,
    /// Comment from original file
    pub comment: Option<String>,
}

impl LookupEntry {
    /// Check if this entry matches a feature's attributes.
    /// All attribute codes must match for the entry to apply.
    /// Empty attribute_codes means "match everything".
    ///
    /// Attribute code format: 6-char S-57 name + value digits.
    /// E.g., "CATSLC1" means CATSLC attribute must equal 1.
    pub fn matches_attributes(&self, attributes: &HashMap<&str, Vec<i32>>) -> bool {
        if self.attribute_codes.is_empty() {
            return true;
        }

        for code in &self.attribute_codes {
            let code = code.trim();
            if code.is_empty() {
                continue;
            }

            // S-57 attribute names are 6 characters
            if code.len() < 6 {
                continue; // Invalid format, skip
            }

            let attr_name = &code[..6];
            let value_str = &code[6..];

            // If no value specified, just check attribute exists
            if value_str.is_empty() {
                if !attributes.contains_key(attr_name) {
                    return false;
                }
                continue;
            }

            // Parse expected value
            let Ok(expected_value) = value_str.parse::<i32>() else {
                continue; // Can't parse value, skip
            };

            // Check if attribute exists and matches any of its values
            match attributes.get(attr_name) {
                Some(values) => {
                    if !values.iter().any(|v| *v == expected_value) {
                        return false;
                    }
                }
                None => return false,
            }
        }

        true
    }
}

/// Complete lookup tables loaded from chartsymbols.xml
#[derive(Debug, Default)]
pub struct LookupTables {
    /// Lookup entries indexed by (object_class, geometry_type)
    /// Multiple entries can exist per key (for attribute filtering)
    entries: HashMap<(String, GeometryType), Vec<LookupEntry>>,
    /// Active color table (color_name -> [r, g, b]) — DAY_BRIGHT by default
    pub colors: HashMap<String, [u8; 3]>,
    /// All palettes: palette_name -> (token_index -> [r, g, b])
    /// Each inner Vec is indexed by color_index (same ordering as color_tokens)
    palettes: HashMap<String, Vec<[u8; 3]>>,
    /// Ordered list of color token names (index position = color_index)
    color_tokens: Vec<String>,
    /// Reverse map: color token name -> index
    color_index_map: HashMap<String, u16>,
    /// Name of the currently active palette
    active_palette: String,
}

impl LookupTables {
    /// Create empty lookup tables
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a lookup entry
    pub fn add_entry(&mut self, entry: LookupEntry) {
        let key = (entry.object_class.clone(), entry.geometry_type);
        self.entries.entry(key).or_default().push(entry);
    }

    /// Add a color definition to the active palette (legacy single-palette path)
    pub fn add_color(&mut self, name: String, r: u8, g: u8, b: u8) {
        self.colors.insert(name, [r, g, b]);
    }

    /// Add a color to a named palette. Builds the token index on first call.
    pub fn add_palette_color(&mut self, palette_name: &str, token: &str, r: u8, g: u8, b: u8) {
        // Assign index if this is a new token
        let idx = if let Some(&idx) = self.color_index_map.get(token) {
            idx as usize
        } else {
            let idx = self.color_tokens.len();
            self.color_tokens.push(token.to_string());
            self.color_index_map.insert(token.to_string(), idx as u16);
            idx
        };

        // Ensure palette vec is large enough
        let palette = self.palettes.entry(palette_name.to_string()).or_default();
        if palette.len() <= idx {
            palette.resize(idx + 1, [0, 0, 0]);
        }
        palette[idx] = [r, g, b];

        // Also populate the legacy `colors` map for DAY_BRIGHT (default active palette)
        if palette_name == "DAY_BRIGHT" {
            self.colors.insert(token.to_string(), [r, g, b]);
        }
    }

    /// Finalize palette setup after parsing (ensure all palettes have same length)
    pub fn finalize_palettes(&mut self) {
        let n = self.color_tokens.len();
        for palette in self.palettes.values_mut() {
            palette.resize(n, [0, 0, 0]);
        }
        if self.active_palette.is_empty() {
            self.active_palette = "DAY_BRIGHT".to_string();
        }
        log::info!("Palettes finalized: {} tokens, {} palettes", n, self.palettes.len());
    }

    /// Get the color index for a token name. Returns None if unknown.
    pub fn get_color_index(&self, token: &str) -> Option<u16> {
        self.color_index_map.get(token).copied()
    }

    /// Get the active palette as f32 RGBA array (for GPU upload).
    /// Returns Vec indexed by color_index.
    pub fn active_palette_f32(&self) -> Vec<[f32; 4]> {
        self.palette_f32(&self.active_palette)
    }

    /// Get a named palette as f32 RGBA array.
    pub fn palette_f32(&self, palette_name: &str) -> Vec<[f32; 4]> {
        if let Some(palette) = self.palettes.get(palette_name) {
            palette.iter().map(|[r, g, b]| {
                [*r as f32 / 255.0, *g as f32 / 255.0, *b as f32 / 255.0, 1.0]
            }).collect()
        } else {
            // Fallback: build from legacy colors map
            let mut result = vec![[0.0, 0.0, 0.0, 1.0]; self.color_tokens.len()];
            for (token, idx) in &self.color_index_map {
                if let Some([r, g, b]) = self.colors.get(token) {
                    result[*idx as usize] = [
                        *r as f32 / 255.0,
                        *g as f32 / 255.0,
                        *b as f32 / 255.0,
                        1.0,
                    ];
                }
            }
            result
        }
    }

    /// Set the active palette by name.
    pub fn set_active_palette(&mut self, name: &str) {
        if self.palettes.contains_key(name) {
            self.active_palette = name.to_string();
            // Update legacy colors map
            if let Some(palette) = self.palettes.get(name) {
                self.colors.clear();
                for (i, token) in self.color_tokens.iter().enumerate() {
                    if i < palette.len() {
                        self.colors.insert(token.clone(), palette[i]);
                    }
                }
            }
        }
    }

    /// Number of color tokens (palette entries)
    pub fn palette_color_count(&self) -> usize {
        self.color_tokens.len()
    }

    /// Available palette names
    pub fn palette_names(&self) -> Vec<&str> {
        self.palettes.keys().map(|s| s.as_str()).collect()
    }

    /// Look up entries for an object class and geometry type.
    /// Returns all matching entries (caller should filter by attributes).
    pub fn lookup(&self, object_class: &str, geom_type: GeometryType) -> Option<&[LookupEntry]> {
        self.entries
            .get(&(object_class.to_string(), geom_type))
            .map(|v| v.as_slice())
    }

    /// Find the best matching entry for a feature.
    /// Tries attribute-filtered entries first, falls back to generic.
    pub fn lookup_best(
        &self,
        object_class: &str,
        geom_type: GeometryType,
        attributes: &HashMap<&str, Vec<i32>>,
    ) -> Option<&LookupEntry> {
        let entries = self.lookup(object_class, geom_type)?;

        // First try to find an entry with matching attribute filter
        for entry in entries {
            if !entry.attribute_codes.is_empty() && entry.matches_attributes(attributes) {
                return Some(entry);
            }
        }

        // Fall back to entry without filter
        for entry in entries {
            if entry.attribute_codes.is_empty() {
                return Some(entry);
            }
        }

        // Return first entry as last resort
        entries.first()
    }

    /// Fast lookup that avoids building attributes_as_map() when not needed.
    ///
    /// Most object classes have only generic LUP entries (empty attribute_codes).
    /// For those, we can skip the expensive HashMap allocation entirely and
    /// return the first entry directly.
    ///
    /// Only falls back to `lookup_best()` (which needs the attr map) when there
    /// are attribute-filtered entries that actually require matching.
    pub fn lookup_best_fast<'a>(
        &'a self,
        object_class: &str,
        geom_type: GeometryType,
        feature: &crate::senc::Feature,
    ) -> Option<&'a LookupEntry> {
        let entries = self.lookup(object_class, geom_type)?;

        // Check if ANY entry has attribute filters
        let has_filtered = entries.iter().any(|e| !e.attribute_codes.is_empty());

        if !has_filtered {
            // All entries are generic — return first without allocating attrs map
            return entries.first();
        }

        // Some entries need attribute matching — build map and do full lookup
        let attrs = feature.attributes_as_map();
        self.lookup_best(object_class, geom_type, &attrs)
    }

    /// Get color RGB values by name
    pub fn get_color(&self, name: &str) -> Option<[u8; 3]> {
        self.colors.get(name).copied()
    }

    /// Get color as normalized floats [0.0-1.0]
    pub fn get_color_f32(&self, name: &str) -> Option<[f32; 4]> {
        self.colors.get(name).map(|[r, g, b]| {
            [
                *r as f32 / 255.0,
                *g as f32 / 255.0,
                *b as f32 / 255.0,
                1.0,
            ]
        })
    }

    /// Number of lookup entries
    pub fn entry_count(&self) -> usize {
        self.entries.values().map(|v| v.len()).sum()
    }

    /// Number of colors in active palette
    pub fn color_count(&self) -> usize {
        self.colors.len()
    }
}
