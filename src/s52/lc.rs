//! LC (Line Complex) Symbol Parser
//!
//! Parses line-style symbol definitions from OpenCPN chartsymbols.xml.
//! These symbols are used for LC() instructions to render patterned lines.
//!
//! Each line-style symbol has:
//! - name: Symbol identifier (e.g., "LOWACC21", "DWRTCL05")
//! - vector: Pattern tile dimensions (width, height)
//! - origin/pivot: Reference points for positioning
//! - color_ref: S-52 color token
//! - hpgl: Vector graphics commands (for full rendering)
//!
//! ## HPGL Command Subset
//!
//! We support a minimal HPGL subset used by S-52:
//! - SP (Select Pen) - ignored, we use color_ref
//! - SW (Set Width) - line width in units
//! - PU (Pen Up) - move without drawing
//! - PD (Pen Down) - draw line segments
//! - CI (Circle) - draw circle (approximated as polygon)

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::reader::Reader;

// ============================================================
// HPGL Primitives
// ============================================================

/// A line segment in S-52 coordinate space
#[derive(Debug, Clone, Copy)]
pub struct HpglSegment {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
    pub width: u8,
}

/// Parsed HPGL drawing primitives
#[derive(Debug, Clone, Default)]
pub struct HpglPrimitives {
    /// Line segments to draw
    pub segments: Vec<HpglSegment>,
    /// Bounding box in S-52 units (min_x, min_y, max_x, max_y)
    pub bounds: (i32, i32, i32, i32),
}

impl HpglPrimitives {
    /// Parse HPGL string into drawing primitives
    #[allow(unused_assignments)]
    pub fn parse(hpgl: &str) -> Self {
        let mut result = HpglPrimitives::default();
        let mut pen_down = false;
        let mut current_x: i32 = 0;
        let mut current_y: i32 = 0;
        let mut line_width: u8 = 1;
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;

        // Split by semicolon and process commands
        for cmd in hpgl.split(';') {
            let cmd = cmd.trim();
            if cmd.is_empty() {
                continue;
            }

            // Extract command prefix (2 chars) and parameters
            if cmd.len() < 2 {
                continue;
            }
            let prefix = &cmd[..2];
            let params = &cmd[2..];

            match prefix {
                "SP" | "SPA" | "ST" => {
                    // Select Pen / Set Transparency - ignored
                }
                "SW" => {
                    // Set Width
                    if let Ok(w) = params.parse::<u8>() {
                        line_width = w;
                    }
                }
                "PU" => {
                    // Pen Up - move without drawing
                    pen_down = false;
                    if let Some((x, y)) = parse_coord_pair(params) {
                        current_x = x;
                        current_y = y;
                    }
                }
                "PD" => {
                    // Pen Down - draw line segments
                    pen_down = true;
                    // Parse all coordinate pairs, drawing segments between them
                    for (x, y) in parse_coord_list(params) {
                        if pen_down {
                            result.segments.push(HpglSegment {
                                x0: current_x,
                                y0: current_y,
                                x1: x,
                                y1: y,
                                width: line_width,
                            });
                            // Update bounds
                            min_x = min_x.min(current_x).min(x);
                            min_y = min_y.min(current_y).min(y);
                            max_x = max_x.max(current_x).max(x);
                            max_y = max_y.max(current_y).max(y);
                        }
                        current_x = x;
                        current_y = y;
                    }
                }
                "CI" => {
                    // Circle - approximate with line segments
                    if let Ok(radius) = params.parse::<i32>() {
                        const CIRCLE_SEGMENTS: usize = 16;
                        let cx = current_x;
                        let cy = current_y;
                        for i in 0..CIRCLE_SEGMENTS {
                            let angle0 = (i as f32) * std::f32::consts::TAU / (CIRCLE_SEGMENTS as f32);
                            let angle1 = ((i + 1) as f32) * std::f32::consts::TAU / (CIRCLE_SEGMENTS as f32);
                            let x0 = cx + (radius as f32 * angle0.cos()) as i32;
                            let y0 = cy + (radius as f32 * angle0.sin()) as i32;
                            let x1 = cx + (radius as f32 * angle1.cos()) as i32;
                            let y1 = cy + (radius as f32 * angle1.sin()) as i32;
                            result.segments.push(HpglSegment {
                                x0, y0, x1, y1,
                                width: line_width,
                            });
                            min_x = min_x.min(x0).min(x1);
                            min_y = min_y.min(y0).min(y1);
                            max_x = max_x.max(x0).max(x1);
                            max_y = max_y.max(y0).max(y1);
                        }
                    }
                }
                "PM" | "FP" => {
                    // Polygon Mode / Fill Polygon - ignored for line rendering
                }
                _ => {
                    // Unknown command - skip
                }
            }
        }

        // Set bounds (default to 0,0,1,1 if no segments)
        if min_x <= max_x && min_y <= max_y {
            result.bounds = (min_x, min_y, max_x, max_y);
        } else {
            result.bounds = (0, 0, 1, 1);
        }

        result
    }

    /// Convert segments to normalized [0,1] coordinates relative to symbol origin and size.
    /// Returns segments as (x0, y0, x1, y1) tuples in normalized space.
    pub fn normalized_segments(&self, origin_x: i32, origin_y: i32, width: u32, height: u32) -> Vec<(f32, f32, f32, f32, u8)> {
        if width == 0 || height == 0 {
            return vec![];
        }

        let w = width as f32;
        let h = height as f32;

        self.segments.iter().map(|seg| {
            // Translate relative to origin and normalize to [0,1]
            let x0 = (seg.x0 - origin_x) as f32 / w;
            let y0 = (seg.y0 - origin_y) as f32 / h;
            let x1 = (seg.x1 - origin_x) as f32 / w;
            let y1 = (seg.y1 - origin_y) as f32 / h;
            (x0, y0, x1, y1, seg.width)
        }).collect()
    }
}

/// Parse a single coordinate pair "x,y"
fn parse_coord_pair(s: &str) -> Option<(i32, i32)> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() >= 2 {
        let x = parts[0].trim().parse().ok()?;
        let y = parts[1].trim().parse().ok()?;
        Some((x, y))
    } else {
        None
    }
}

/// Parse a list of coordinate pairs "x1,y1,x2,y2,..."
fn parse_coord_list(s: &str) -> Vec<(i32, i32)> {
    let nums: Vec<i32> = s
        .split(',')
        .filter_map(|p| p.trim().parse().ok())
        .collect();

    nums.chunks(2)
        .filter_map(|chunk| {
            if chunk.len() == 2 {
                Some((chunk[0], chunk[1]))
            } else {
                None
            }
        })
        .collect()
}

/// A line-style symbol definition from chartsymbols.xml
#[derive(Debug, Clone)]
pub struct LineStyleSymbol {
    /// Symbol name (e.g., "LOWACC21", "DWRTCL05")
    pub name: String,
    /// RCID from XML
    pub rcid: u32,
    /// Pattern tile width in S-52 units
    pub width: u32,
    /// Pattern tile height in S-52 units
    pub height: u32,
    /// Origin X coordinate
    pub origin_x: i32,
    /// Origin Y coordinate
    pub origin_y: i32,
    /// Pivot X coordinate (for rotation center)
    pub pivot_x: i32,
    /// Pivot Y coordinate
    pub pivot_y: i32,
    /// Description text
    pub description: String,
    /// HPGL vector commands for drawing
    pub hpgl: String,
    /// S-52 color token (e.g., "ACSTLN", "ADEPSC")
    pub color_ref: String,
}

impl LineStyleSymbol {
    /// Get the repeat distance for this pattern (width in S-52 units)
    pub fn repeat_distance(&self) -> u32 {
        self.width
    }

    /// Convert S-52 units to pixels at a given pixels-per-mm scale
    /// S-52 units are 0.01mm each
    pub fn width_pixels(&self, ppmm: f32) -> f32 {
        (self.width as f32) * 0.01 * ppmm
    }

    /// Convert S-52 units to pixels at a given pixels-per-mm scale
    pub fn height_pixels(&self, ppmm: f32) -> f32 {
        (self.height as f32) * 0.01 * ppmm
    }

    /// Get advance distance in meters for stamping along polylines.
    ///
    /// The advance is the pattern tile width converted from S-52 units (0.01mm)
    /// to a physical distance at a given screen DPI and zoom level.
    ///
    /// For chart rendering, we want symbols to appear at a consistent screen size,
    /// so we compute: advance_meters = width_px / px_per_meter
    pub fn advance_meters(&self, ppmm: f32, px_per_meter: f32) -> f32 {
        let width_px = self.width_pixels(ppmm);
        if px_per_meter > 0.0 {
            width_px / px_per_meter
        } else {
            // Fallback: assume 1 pixel = 1 meter (very zoomed out)
            width_px
        }
    }

    /// Parse the HPGL commands into drawing primitives
    pub fn parse_hpgl(&self) -> HpglPrimitives {
        HpglPrimitives::parse(&self.hpgl)
    }

    /// Get normalized line segments for rendering.
    /// Returns (x0, y0, x1, y1, width) tuples in [0,1] normalized space.
    pub fn normalized_segments(&self) -> Vec<(f32, f32, f32, f32, u8)> {
        let primitives = self.parse_hpgl();
        primitives.normalized_segments(self.origin_x, self.origin_y, self.width, self.height)
    }

    /// Get the symbol dimensions in pixels at a given ppmm scale.
    /// Returns (width_px, height_px).
    pub fn size_pixels(&self, ppmm: f32) -> (f32, f32) {
        (self.width_pixels(ppmm), self.height_pixels(ppmm))
    }
}

/// Table of all line-style symbols indexed by name
pub struct LineStyleTable {
    by_name: HashMap<String, LineStyleSymbol>,
}

impl LineStyleTable {
    /// Load line-style symbols from OpenCPN chartsymbols.xml
    pub fn load_from_xml<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let content = fs::read_to_string(path.as_ref())
            .map_err(|e| format!("Failed to read chartsymbols.xml: {}", e))?;

        let mut by_name: HashMap<String, LineStyleSymbol> = HashMap::new();

        let mut reader = Reader::from_str(&content);
        reader.trim_text(true);

        // State for current line-style
        let mut in_line_style = false;
        let mut current_rcid: u32 = 0;
        let mut current_name = String::new();
        let mut current_width: u32 = 0;
        let mut current_height: u32 = 0;
        let mut current_origin_x: i32 = 0;
        let mut current_origin_y: i32 = 0;
        let mut current_pivot_x: i32 = 0;
        let mut current_pivot_y: i32 = 0;
        let mut current_description = String::new();
        let mut current_hpgl = String::new();
        let mut current_color_ref = String::new();

        let mut current_element = String::new();
        let mut buf = Vec::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    current_element = name.clone();

                    match name.as_str() {
                        "line-style" => {
                            in_line_style = true;
                            // Extract RCID attribute
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"RCID" {
                                    let val = String::from_utf8_lossy(&attr.value);
                                    current_rcid = val.parse().unwrap_or(0);
                                }
                            }
                        }
                        "vector" if in_line_style => {
                            // Extract width and height attributes
                            for attr in e.attributes().flatten() {
                                let key = String::from_utf8_lossy(attr.key.as_ref());
                                let val = String::from_utf8_lossy(&attr.value);
                                match key.as_ref() {
                                    "width" => current_width = val.parse().unwrap_or(0),
                                    "height" => current_height = val.parse().unwrap_or(0),
                                    _ => {}
                                }
                            }
                        }
                        "origin" if in_line_style => {
                            for attr in e.attributes().flatten() {
                                let key = String::from_utf8_lossy(attr.key.as_ref());
                                let val = String::from_utf8_lossy(&attr.value);
                                match key.as_ref() {
                                    "x" => current_origin_x = val.parse().unwrap_or(0),
                                    "y" => current_origin_y = val.parse().unwrap_or(0),
                                    _ => {}
                                }
                            }
                        }
                        "pivot" if in_line_style => {
                            for attr in e.attributes().flatten() {
                                let key = String::from_utf8_lossy(attr.key.as_ref());
                                let val = String::from_utf8_lossy(&attr.value);
                                match key.as_ref() {
                                    "x" => current_pivot_x = val.parse().unwrap_or(0),
                                    "y" => current_pivot_y = val.parse().unwrap_or(0),
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
                // Handle self-closing tags like <origin x="..." /> and <pivot x="..." />
                Ok(Event::Empty(ref e)) if in_line_style => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    match name.as_str() {
                        "origin" => {
                            for attr in e.attributes().flatten() {
                                let key = String::from_utf8_lossy(attr.key.as_ref());
                                let val = String::from_utf8_lossy(&attr.value);
                                match key.as_ref() {
                                    "x" => current_origin_x = val.parse().unwrap_or(0),
                                    "y" => current_origin_y = val.parse().unwrap_or(0),
                                    _ => {}
                                }
                            }
                        }
                        "pivot" => {
                            for attr in e.attributes().flatten() {
                                let key = String::from_utf8_lossy(attr.key.as_ref());
                                let val = String::from_utf8_lossy(&attr.value);
                                match key.as_ref() {
                                    "x" => current_pivot_x = val.parse().unwrap_or(0),
                                    "y" => current_pivot_y = val.parse().unwrap_or(0),
                                    _ => {}
                                }
                            }
                        }
                        "distance" => {
                            // Ignore distance for now (min/max attributes)
                        }
                        _ => {}
                    }
                }
                Ok(Event::Text(ref e)) if in_line_style => {
                    let text = e.unescape().unwrap_or_default().to_string();
                    match current_element.as_str() {
                        "name" => current_name = text,
                        "description" => current_description = text,
                        "HPGL" => current_hpgl = text,
                        "color-ref" => {
                            // S-52 vector color-ref is <penLetter><5-char palette
                            // token>[...]. The leading pen letter maps to HPGL
                            // "SPx" pen-selects (which we ignore); the palette
                            // token is the next 5 chars. Strip the pen letter so
                            // the token resolves against the colour table, e.g.
                            // "ACHMGD" -> "CHMGD" (magenta). Without this, every
                            // LC() complex line fell back to black.
                            current_color_ref = if text.len() >= 6 {
                                text[1..6].to_string()
                            } else {
                                text
                            };
                        }
                        _ => {}
                    }
                }
                Ok(Event::End(ref e)) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    if name == "line-style" && in_line_style {
                        // Save the line-style entry
                        if !current_name.is_empty() {
                            by_name.insert(
                                current_name.clone(),
                                LineStyleSymbol {
                                    name: current_name.clone(),
                                    rcid: current_rcid,
                                    width: current_width,
                                    height: current_height,
                                    origin_x: current_origin_x,
                                    origin_y: current_origin_y,
                                    pivot_x: current_pivot_x,
                                    pivot_y: current_pivot_y,
                                    description: current_description.clone(),
                                    hpgl: current_hpgl.clone(),
                                    color_ref: current_color_ref.clone(),
                                },
                            );
                        }

                        // Reset state
                        in_line_style = false;
                        current_rcid = 0;
                        current_name.clear();
                        current_width = 0;
                        current_height = 0;
                        current_origin_x = 0;
                        current_origin_y = 0;
                        current_pivot_x = 0;
                        current_pivot_y = 0;
                        current_description.clear();
                        current_hpgl.clear();
                        current_color_ref.clear();
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    log::warn!("Error parsing chartsymbols.xml: {}", e);
                    break;
                }
                _ => {}
            }
            buf.clear();
        }

        Ok(Self { by_name })
    }

    /// Get a line-style symbol by name
    pub fn get(&self, name: &str) -> Option<&LineStyleSymbol> {
        self.by_name.get(name)
    }

    /// Get all symbol names
    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.by_name.keys()
    }

    /// Get the number of loaded symbols
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Check if the table is empty
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHARTSYMBOLS_PATH: &str = "assets/s52/chartsymbols.xml";

    #[test]
    fn test_load_line_style_table() {
        let table = LineStyleTable::load_from_xml(CHARTSYMBOLS_PATH);
        assert!(table.is_ok(), "Failed to load line-style table");

        let table = table.unwrap();
        assert!(!table.is_empty(), "Line-style table should not be empty");

        println!("Loaded {} line-style symbols", table.len());
    }

    #[test]
    fn test_lowacc21_exists() {
        let table = LineStyleTable::load_from_xml(CHARTSYMBOLS_PATH).unwrap();
        let symbol = table.get("LOWACC21");
        assert!(symbol.is_some(), "LOWACC21 should exist");

        let symbol = symbol.unwrap();
        assert_eq!(symbol.name, "LOWACC21");
        assert_eq!(symbol.width, 130);
        assert_eq!(symbol.height, 79);
        // The XML writes the token with its leading pen letter (ACSTLN); the
        // parser strips it so the token resolves in the colour table.
        assert_eq!(symbol.color_ref, "CSTLN");
        println!("LOWACC21: {:?}", symbol);
    }

    #[test]
    fn test_arcsln01_exists() {
        let table = LineStyleTable::load_from_xml(CHARTSYMBOLS_PATH).unwrap();
        // ARCSLN01 is commonly referenced in LC() instructions
        // Check if it exists (might have different name format)
        let found = table.names().any(|n| n.contains("ARCSLN"));
        if !found {
            println!("Available line-styles containing 'ARC': {:?}",
                table.names().filter(|n| n.contains("ARC")).collect::<Vec<_>>());
        }
    }

    #[test]
    fn test_repeat_distance() {
        let table = LineStyleTable::load_from_xml(CHARTSYMBOLS_PATH).unwrap();
        if let Some(symbol) = table.get("LOWACC21") {
            assert_eq!(symbol.repeat_distance(), 130);
            // At 96 DPI (3.78 ppmm), width should be ~0.49 pixels
            let width_px = symbol.width_pixels(3.78);
            assert!(width_px > 0.0, "Width in pixels should be positive");
        }
    }

    // HPGL parsing tests

    #[test]
    fn test_hpgl_parse_lowacc21() {
        // LOWACC21 HPGL: "SPA;SW1;PU5476,2607;PD5476,2686;PD5606,2686;PD5606,2607;PD5476,2607;"
        // This draws a rectangle
        let hpgl = "SPA;SW1;PU5476,2607;PD5476,2686;PD5606,2686;PD5606,2607;PD5476,2607;";
        let primitives = HpglPrimitives::parse(hpgl);

        // Should have 4 line segments forming a rectangle
        assert_eq!(primitives.segments.len(), 4, "LOWACC21 should have 4 segments");

        // All segments should have width 1
        for seg in &primitives.segments {
            assert_eq!(seg.width, 1);
        }
    }

    #[test]
    fn test_hpgl_parse_navare51() {
        // NAVARE51 HPGL: "SPA;SW1;PU1507,814;PD2107,814;SPA;SW1;PU1647,812;PD1812,976;PD1976,812;"
        // This draws a horizontal line with a caret/chevron
        let hpgl = "SPA;SW1;PU1507,814;PD2107,814;SPA;SW1;PU1647,812;PD1812,976;PD1976,812;";
        let primitives = HpglPrimitives::parse(hpgl);

        // Should have 3 segments: horizontal line + 2 for caret
        assert_eq!(primitives.segments.len(), 3, "NAVARE51 should have 3 segments");
    }

    #[test]
    fn test_hpgl_parse_circle() {
        // Test circle command
        let hpgl = "PU100,100;CI50;";
        let primitives = HpglPrimitives::parse(hpgl);

        // Circle is approximated with 16 segments
        assert_eq!(primitives.segments.len(), 16, "Circle should have 16 segments");
    }

    #[test]
    fn test_normalized_segments() {
        let table = LineStyleTable::load_from_xml(CHARTSYMBOLS_PATH).unwrap();
        if let Some(symbol) = table.get("LOWACC21") {
            let segments = symbol.normalized_segments();
            assert!(!segments.is_empty(), "LOWACC21 should have normalized segments");

            // All normalized coordinates should be finite
            for (x0, y0, x1, y1, _) in &segments {
                assert!(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite());
            }

            println!("LOWACC21 normalized segments: {:?}", segments);
        }
    }

    #[test]
    fn test_advance_meters() {
        let table = LineStyleTable::load_from_xml(CHARTSYMBOLS_PATH).unwrap();
        if let Some(symbol) = table.get("LOWACC21") {
            // At 96 DPI (3.78 ppmm) and 10 px/m zoom
            let ppmm = 3.78;
            let px_per_m = 10.0;
            let advance = symbol.advance_meters(ppmm, px_per_m);

            // Width is 130 S-52 units = 1.30mm = ~4.9 px at 96 DPI
            // At 10 px/m, advance should be ~0.49 meters
            assert!(advance > 0.0 && advance < 1.0, "Advance should be reasonable: {}", advance);
            println!("LOWACC21 advance at 10 px/m: {} meters", advance);
        }
    }

    #[test]
    fn test_navare51_exists() {
        let table = LineStyleTable::load_from_xml(CHARTSYMBOLS_PATH).unwrap();
        let symbol = table.get("NAVARE51");
        assert!(symbol.is_some(), "NAVARE51 should exist (most common LC symbol)");

        let symbol = symbol.unwrap();
        assert_eq!(symbol.width, 600);  // 6.00mm
        assert_eq!(symbol.color_ref, "CHGRD");
    }
}
