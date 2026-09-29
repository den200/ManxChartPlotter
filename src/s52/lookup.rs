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
/// The ten S-52 priority levels, in spec order. The numbering is fixed by the
/// standard (and by OpenCPN's `DisPrio` enum): "No data" is the *bottom* level,
/// below Group 1 — not a mid-stack default.
pub enum DisplayPriority {
    /// No-data fill areas (unsurveyed water) — painted below everything
    NoData,
    /// Group 1 areas (background: land, depth areas, unsurveyed)
    Group1,
    /// Area 1 — superimposed areas
    Area1,
    /// Area 2 — superimposed areas, also water features
    Area2,
    /// Point symbols, also land features
    PointSymbol,
    /// Line symbols, also restricted areas
    LineSymbol,
    /// Area symbols, also traffic areas
    AreaSymbol,
    /// Routing (traffic lanes, etc.)
    Routing,
    /// Hazards (wrecks, obstructions)
    Hazards,
    /// Mariners (VRM, EBL, own ship)
    Mariners,
}

impl DisplayPriority {
    /// Parse from the `<disp-prio>` text in chartsymbols.xml.
    ///
    /// The ten strings below are exactly the ones the file uses; anything else
    /// falls back to `NoData`, matching `chartsymbols.cpp`'s final `else`.
    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim() {
            "Group 1" => Some(Self::Group1),
            "Area 1" => Some(Self::Area1),
            "Area 2" => Some(Self::Area2),
            "Point Symbol" => Some(Self::PointSymbol),
            "Line Symbol" => Some(Self::LineSymbol),
            "Area Symbol" => Some(Self::AreaSymbol),
            "Routing" => Some(Self::Routing),
            "Hazards" => Some(Self::Hazards),
            "Mariners" | "Mariners Standard" | "Mariners Other" => Some(Self::Mariners),
            _ => Some(Self::NoData),
        }
    }

    /// Get numeric priority for sorting (higher = on top)
    pub fn as_u8(&self) -> u8 {
        match self {
            Self::NoData => 0,
            Self::Group1 => 1,
            Self::Area1 => 2,
            Self::Area2 => 3,
            Self::PointSymbol => 4,
            Self::LineSymbol => 5,
            Self::AreaSymbol => 6,
            Self::Routing => 7,
            Self::Hazards => 8,
            Self::Mariners => 9,
        }
    }
}

/// S-52 lookup table type from `<table-name>` in chartsymbols.xml.
/// Controls which set of entries is used for a given geometry type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TableName {
    /// Point symbols — simplified mode
    Simplified,
    /// Point symbols — paper chart mode
    Paper,
    /// Line features
    Lines,
    /// Area boundaries — simple LS() lines
    Plain,
    /// Area boundaries — complex LC() patterns
    Symbolized,
}

impl TableName {
    /// Parse from XML `<table-name>` text
    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim() {
            "Simplified" => Some(Self::Simplified),
            "Paper" => Some(Self::Paper),
            "Lines" => Some(Self::Lines),
            "Plain" => Some(Self::Plain),
            "Symbolized" => Some(Self::Symbolized),
            _ => None,
        }
    }

    /// Select the preferred table name for a geometry type based on settings.
    pub fn preferred(geom_type: GeometryType, symbolized_boundaries: bool) -> Self {
        Self::preferred_ext(geom_type, symbolized_boundaries, false)
    }

    /// Extended variant that also honors the Simplified-points mariner setting.
    pub fn preferred_ext(
        geom_type: GeometryType,
        symbolized_boundaries: bool,
        simplified_points: bool,
    ) -> Self {
        match geom_type {
            GeometryType::Point => {
                if simplified_points {
                    TableName::Simplified
                } else {
                    TableName::Paper
                }
            }
            GeometryType::Line => TableName::Lines,
            GeometryType::Area => {
                if symbolized_boundaries {
                    TableName::Symbolized
                } else {
                    TableName::Plain
                }
            }
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
    /// Which lookup table this entry belongs to
    pub table_name: TableName,
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
    /// The `id` attribute of the `<lookup>` element — OpenCPN's `nSequence`,
    /// the third sort key of its LUP array (see [`LookupTables::finalize_lookups`]).
    pub seq: u32,
}

impl LookupEntry {
    /// Check if this entry matches a feature's attributes.
    /// All attribute codes must match for the entry to apply.
    /// Empty attribute_codes means "match everything".
    ///
    /// Attribute code format: 6-char S-57 name + value digits.
    /// E.g., "CATSLC1" means CATSLC attribute must equal 1.
    /// Does this entry's attribute filter match the feature?
    ///
    /// Mirrors the value comparison in `FindBestLUP` (s52plib.cpp), which
    /// switches on the *stored type* of the feature's attribute:
    ///
    /// | stored as | compared how |
    /// |---|---|
    /// | integer / enum | numeric equality |
    /// | float          | numeric equality |
    /// | string, incl. S-57 'L' lists | **whole-string** equality |
    ///
    /// The list case is the one that matters: a feature with `CATREA="4,5"`
    /// does *not* satisfy a `CATREA4` filter, because OpenCPN compares
    /// `strcmp("4,5", "4")`. Treating the value as a set of integers and
    /// testing membership — which Manx used to do — picks a more specific
    /// lookup row than OpenCPN ever would.
    pub fn matches_attributes(&self, feature: &crate::senc::Feature) -> bool {
        use crate::senc::AttributeValue;

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
            let value_str = code[6..].trim();
            let stored = feature.attributes.get(attr_name);

            // Blank value: any value of this attribute matches (S-52 8.3.3.4).
            if value_str.is_empty() {
                if stored.is_none() {
                    return false;
                }
                continue;
            }

            // "?" is the S-52 negative filter: the attribute must be absent.
            // E.g. the DEPARE LUP "DRVAL1? DRVAL2?" matches only unsurveyed
            // depth areas that carry no depth attributes. (OpenCPN never
            // matches a '?' row at all — it forgets to count the match — but
            // the spec reading below is what keeps Manx's water fills.)
            if value_str == "?" {
                if stored.is_some() {
                    return false;
                }
                continue;
            }

            let matched = match stored {
                Some(AttributeValue::Integer(v)) => {
                    value_str.parse::<i32>().map(|e| e == *v).unwrap_or(false)
                }
                Some(AttributeValue::Float(v)) => value_str
                    .parse::<f64>()
                    .map(|e| (e - *v).abs() < 1e-6)
                    .unwrap_or(false),
                Some(AttributeValue::String(s)) => s == value_str,
                None => false,
            };
            if !matched {
                return false;
            }
        }

        true
    }
}

/// Complete lookup tables loaded from chartsymbols.xml
#[derive(Debug, Default)]
pub struct LookupTables {
    /// Lookup entries indexed by (object_class, geometry_type, table_name)
    /// Multiple entries can exist per key (for attribute filtering)
    entries: HashMap<(String, GeometryType, TableName), Vec<LookupEntry>>,
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
        let key = (entry.object_class.clone(), entry.geometry_type, entry.table_name);
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

    /// Look up entries for an object class, geometry type, and table name.
    /// Returns all matching entries (caller should filter by attributes).
    pub fn lookup(&self, object_class: &str, geom_type: GeometryType, table_name: TableName) -> Option<&[LookupEntry]> {
        self.entries
            .get(&(object_class.to_string(), geom_type, table_name))
            .map(|v| v.as_slice())
    }

    /// Find the lookup row OpenCPN would pick for this feature.
    ///
    /// `FindBestLUP` (s52plib.cpp) walks the object's rows **in array order**
    /// and takes the *first* whose attribute filter matches completely — "the
    /// first 100% match is selected". Specificity does not win on its own; it
    /// wins because [`finalize_lookups`] has already sorted the rows
    /// most-attributes-first, exactly as OpenCPN's `CompareLUPObjects` does.
    /// With no full match, the first row carrying no attributes at all is the
    /// default.
    ///
    /// [`finalize_lookups`]: LookupTables::finalize_lookups
    pub fn lookup_best(
        &self,
        object_class: &str,
        geom_type: GeometryType,
        table_name: TableName,
        feature: &crate::senc::Feature,
    ) -> Option<&LookupEntry> {
        let entries = self.lookup(object_class, geom_type, table_name)?;

        for entry in entries {
            if entry.attribute_codes.is_empty() {
                continue;
            }
            if entry.matches_attributes(feature) {
                return Some(entry);
            }
        }

        entries
            .iter()
            .find(|e| e.attribute_codes.is_empty())
            .or_else(|| entries.first())
    }

    /// Fast path around [`lookup_best`] for classes whose rows carry no
    /// attribute filters at all — the common case.
    ///
    /// [`lookup_best`]: LookupTables::lookup_best
    pub fn lookup_best_fast<'a>(
        &'a self,
        object_class: &str,
        geom_type: GeometryType,
        table_name: TableName,
        feature: &crate::senc::Feature,
    ) -> Option<&'a LookupEntry> {
        let entries = self.lookup(object_class, geom_type, table_name)?;

        if entries.iter().all(|e| e.attribute_codes.is_empty()) {
            return entries.first();
        }

        self.lookup_best(object_class, geom_type, table_name, feature)
    }

    /// Order every object's rows the way OpenCPN's sorted LUP array is ordered,
    /// so that "first full match wins" picks the same row.
    ///
    /// `CompareLUPObjects` sorts by object name, then by **attribute count
    /// descending**, then by `nSequence` (the `<lookup id>` from the XML).
    /// Where two rows tie on all three — chartsymbols.xml has ACHARE rows that
    /// share `id="2"` — OpenCPN's sorted-array insert leaves the later-parsed
    /// row first, so file order is reversed for exact ties.
    pub fn finalize_lookups(&mut self) {
        for entries in self.entries.values_mut() {
            let order: Vec<usize> = (0..entries.len()).collect();
            let mut idx = order;
            idx.sort_by(|&a, &b| {
                let (ea, eb) = (&entries[a], &entries[b]);
                eb.attribute_codes
                    .len()
                    .cmp(&ea.attribute_codes.len())
                    .then(ea.seq.cmp(&eb.seq))
                    .then(b.cmp(&a))
            });
            let mut sorted: Vec<LookupEntry> = Vec::with_capacity(entries.len());
            for i in idx {
                sorted.push(entries[i].clone());
            }
            *entries = sorted;
        }
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
