//! LC (Line Complex) Pattern Rendering
//!
//! Renders S-52 line patterns by stamping vector primitives along polylines.
//! Each LC symbol is parsed from HPGL and rendered as transformed line segments
//! at regular intervals (the symbol's advance distance).
//!
//! ## Design
//!
//! Instead of using a texture atlas (which requires rasterization at specific DPI),
//! we render LC symbols as vector line segments. Each stamp along a polyline:
//! 1. Takes the symbol's normalized HPGL segments
//! 2. Rotates them to align with the polyline direction
//! 3. Scales them to screen pixels
//! 4. Emits LineVertex data for GPU rendering
//!
//! This approach:
//! - Maintains vector precision at all zoom levels
//! - Uses the existing line rendering pipeline
//! - Avoids texture management complexity

use crate::s52::lc::{LineStyleSymbol, LineStyleTable};
use crate::render::line_vertices::build_line_vertices_multi_indexed;
use crate::render::state::LineVertex;

/// Configuration for LC pattern rendering
#[derive(Debug, Clone)]
pub struct LcRenderConfig {
    /// Pixels per millimeter (screen DPI / 25.4)
    pub ppmm: f32,
    /// Meters per pixel at current zoom
    pub meters_per_pixel: f32,
    /// Global Mercator position the polylines are measured from (a tile's
    /// centre), so the stamp phase can still be taken on the global grid.
    pub origin: [f64; 2],
}

impl LcRenderConfig {
    /// Create config from screen DPI and camera zoom
    pub fn new(dpi: f32, meters_per_pixel: f32) -> Self {
        Self {
            ppmm: dpi / 25.4,
            meters_per_pixel,
            origin: [0.0, 0.0],
        }
    }

    /// Get pixels per meter at current zoom
    pub fn px_per_meter(&self) -> f32 {
        if self.meters_per_pixel > 0.0 {
            1.0 / self.meters_per_pixel
        } else {
            1.0
        }
    }
}

/// An LC stamp instance to be rendered along a polyline
#[derive(Debug, Clone)]
pub struct LcStamp {
    /// Position in global Mercator coordinates
    pub position: [f32; 2],
    /// Rotation angle in radians (direction of polyline at this point)
    pub angle: f32,
    /// Scale factor (symbol size in pixels)
    pub scale: f32,
    /// Color as RGBA
    pub color: [f32; 4],
}

/// Computes a deterministic phase offset for LC pattern continuity across tiles.
///
/// The phase is derived from feature properties so that the same feature
/// always starts its pattern at the same phase, regardless of tile clipping.
///
/// # Arguments
/// * `lookup_id` - S-52 lookup table ID for this feature
/// * `first_point` - First point of the polyline in global Mercator coordinates
/// * `acronym` - Object class acronym (e.g., "COALNE", "SLCONS")
/// * `advance_meters` - Symbol advance distance in meters
///
/// # Returns
/// Phase offset in meters, in range [0, advance_meters)
pub fn compute_phase_offset(
    lookup_id: u32,
    first_point: [f32; 2],
    acronym: &str,
    advance_meters: f32,
) -> f32 {
    if advance_meters <= 0.0 {
        return 0.0;
    }

    // Quantize first point to 1-meter grid for stability
    let qx = first_point[0] as i64;
    let qy = first_point[1] as i64;

    // Simple FNV-1a style hash combining all inputs
    let mut hash: u64 = 0xcbf29ce484222325; // FNV offset basis
    const FNV_PRIME: u64 = 0x100000001b3;

    // Mix in lookup_id
    hash ^= lookup_id as u64;
    hash = hash.wrapping_mul(FNV_PRIME);

    // Mix in quantized coordinates
    hash ^= qx as u64;
    hash = hash.wrapping_mul(FNV_PRIME);
    hash ^= qy as u64;
    hash = hash.wrapping_mul(FNV_PRIME);

    // Mix in acronym bytes
    for byte in acronym.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }

    // Convert to [0, advance_meters) range
    let normalized = (hash % 10000) as f32 / 10000.0;
    normalized * advance_meters
}

/// Generates LC stamps along a polyline with phase offset for tile continuity.
///
/// Returns a list of stamps positioned at `advance` intervals along the polyline.
/// The advance distance is computed from the symbol's width at the current zoom.
///
/// For tile continuity, stamps are placed at positions along a global grid that depends
/// on the polyline's direction. This ensures that when a polyline is clipped at tile
/// boundaries, the stamps from different tiles align properly.
///
/// # Arguments
/// * `polyline` - Points in global Mercator coordinates
/// * `symbol` - LC symbol definition
/// * `config` - Render configuration (DPI, scale)
/// * `color` - RGBA color for stamps
/// * `phase_offset_meters` - Feature-specific phase offset for deterministic alignment
pub fn generate_stamps_along_polyline_with_phase(
    polyline: &[[f32; 2]],
    symbol: &LineStyleSymbol,
    config: &LcRenderConfig,
    color: [f32; 4],
    phase_offset_meters: f32,
) -> Vec<LcStamp> {
    if polyline.len() < 2 {
        return vec![];
    }

    // Compute advance in pixels
    let advance_px = symbol.width_pixels(config.ppmm);
    if advance_px < 1.0 {
        return vec![]; // Symbol too small at this zoom
    }

    // Convert to global Mercator meters
    let advance_meters = advance_px * config.meters_per_pixel;
    if advance_meters < 0.001 {
        return vec![]; // Avoid excessive stamps
    }

    let scale = symbol.height_pixels(config.ppmm);
    let mut stamps = Vec::new();

    // Compute overall direction of the polyline
    let first_pt = polyline[0];
    let last_pt = polyline[polyline.len() - 1];
    let overall_dx = last_pt[0] - first_pt[0];
    let overall_dy = last_pt[1] - first_pt[1];
    let overall_len = (overall_dx * overall_dx + overall_dy * overall_dy).sqrt();

    if overall_len < 0.001 {
        return vec![];
    }

    // Normalize direction and canonicalize it so phase alignment does not depend
    // on whether the input polyline runs "forward" or "backward".
    // (The stamp *rotation* still uses per-segment angles below.)
    let mut dir_x = overall_dx / overall_len;
    let mut dir_y = overall_dy / overall_len;
    if dir_x < 0.0 || (dir_x == 0.0 && dir_y < 0.0) {
        dir_x = -dir_x;
        dir_y = -dir_y;
    }

    // Compute where the polyline's first point falls in the GLOBAL coordinate system
    // by projecting it onto the line's direction from the origin
    // This gives us the "global arc position" of where this polyline segment starts
    // Compute dot in f64 to reduce drift at large Mercator coordinates.
    let global_start_pos = (first_pt[0] as f64 + config.origin[0]) * (dir_x as f64)
        + (first_pt[1] as f64 + config.origin[1]) * (dir_y as f64);
    let global_start_pos = global_start_pos as f32;

    // Compute the global base phase (where stamp 0 would be for an infinite line through origin)
    // This includes centering offset (half-advance) plus feature-specific phase
    let base_phase = phase_offset_meters + advance_meters * 0.5;

    // Find where stamps fall relative to global_start_pos
    // Stamps are at global positions: base_phase + k*advance, for k = 0, 1, 2, ...
    // We need to find first k such that base_phase + k*advance >= global_start_pos
    let relative_start = global_start_pos - base_phase;
    let first_stamp_k = if relative_start <= 0.0 {
        0.0
    } else {
        (relative_start / advance_meters).ceil()
    };
    let first_stamp_global = base_phase + first_stamp_k * advance_meters;

    // Convert to arc-length offset from polyline start
    let first_stamp_arc = first_stamp_global - global_start_pos;

    // Accumulate arc length along polyline
    let mut arc_length = 0.0_f32;

    for i in 0..(polyline.len() - 1) {
        let p0 = polyline[i];
        let p1 = polyline[i + 1];

        let dx = p1[0] - p0[0];
        let dy = p1[1] - p0[1];
        let seg_len = (dx * dx + dy * dy).sqrt();

        if seg_len < 0.001 {
            continue;
        }

        let angle = dy.atan2(dx);
        let seg_start_arc = arc_length;
        let seg_end_arc = arc_length + seg_len;

        // Find stamps that fall within this segment
        // First stamp after seg_start_arc
        let mut stamp_arc = if first_stamp_arc < seg_start_arc {
            // Find next stamp >= seg_start_arc
            let n = ((seg_start_arc - first_stamp_arc) / advance_meters).ceil();
            first_stamp_arc + n * advance_meters
        } else {
            first_stamp_arc
        };

        while stamp_arc < seg_end_arc {
            // Convert arc position to segment-local t parameter
            let t = (stamp_arc - seg_start_arc) / seg_len;
            if t >= 0.0 && t <= 1.0 {
                let x = p0[0] + dx * t;
                let y = p0[1] + dy * t;

                stamps.push(LcStamp {
                    position: [x, y],
                    angle,
                    scale,
                    color,
                });
            }
            stamp_arc += advance_meters;
        }

        arc_length = seg_end_arc;
    }

    stamps
}

/// Generates LC stamps along a polyline (legacy API without phase offset).
///
/// Returns a list of stamps positioned at `advance` intervals along the polyline.
/// The advance distance is computed from the symbol's width at the current zoom.
pub fn generate_stamps_along_polyline(
    polyline: &[[f32; 2]],
    symbol: &LineStyleSymbol,
    config: &LcRenderConfig,
    color: [f32; 4],
) -> Vec<LcStamp> {
    generate_stamps_along_polyline_with_phase(polyline, symbol, config, color, 0.0)
}

/// Generates polylines from LC stamps using the symbol's HPGL primitives.
///
/// Each stamp's HPGL segments are transformed (rotated + scaled + translated)
/// and returned as polylines ready for the line rendering pipeline.
pub fn stamps_to_polylines(
    stamps: &[LcStamp],
    symbol: &LineStyleSymbol,
    config: &LcRenderConfig,
) -> Vec<Vec<[f32; 2]>> {
    stamps_to_polylines_by_width(stamps, symbol, config)
        .into_iter()
        .flat_map(|(_, polylines)| polylines)
        .collect()
}

/// Generates LC stamp polylines grouped by the HPGL `SW` primitive width.
pub fn stamps_to_polylines_by_width(
    stamps: &[LcStamp],
    symbol: &LineStyleSymbol,
    config: &LcRenderConfig,
) -> Vec<(u8, Vec<Vec<[f32; 2]>>)> {
    let normalized = symbol.normalized_segments();
    if normalized.is_empty() {
        return vec![];
    }

    let (width_px, height_px) = symbol.size_pixels(config.ppmm);
    let mut grouped: std::collections::BTreeMap<u8, Vec<Vec<[f32; 2]>>> =
        std::collections::BTreeMap::new();

    for stamp in stamps {
        let cos_a = stamp.angle.cos();
        let sin_a = stamp.angle.sin();

        // Transform each segment into a 2-point polyline
        for (nx0, ny0, nx1, ny1, width) in &normalized {
            // Scale to pixels, then to meters
            let sx0 = *nx0 * width_px * config.meters_per_pixel;
            let sy0 = *ny0 * height_px * config.meters_per_pixel;
            let sx1 = *nx1 * width_px * config.meters_per_pixel;
            let sy1 = *ny1 * height_px * config.meters_per_pixel;

            // Rotate
            let rx0 = sx0 * cos_a - sy0 * sin_a;
            let ry0 = sx0 * sin_a + sy0 * cos_a;
            let rx1 = sx1 * cos_a - sy1 * sin_a;
            let ry1 = sx1 * sin_a + sy1 * cos_a;

            // Translate
            let x0 = stamp.position[0] + rx0;
            let y0 = stamp.position[1] + ry0;
            let x1 = stamp.position[0] + rx1;
            let y1 = stamp.position[1] + ry1;

            grouped
                .entry((*width).max(1))
                .or_default()
                .push(vec![[x0, y0], [x1, y1]]);
        }
    }

    grouped.into_iter().collect()
}

/// Generates line vertices from LC stamps using the existing line building pipeline.
///
/// Returns (vertices, indices) suitable for the line shader.
pub fn stamps_to_line_vertices(
    stamps: &[LcStamp],
    symbol: &LineStyleSymbol,
    config: &LcRenderConfig,
) -> (Vec<LineVertex>, Vec<u32>) {
    let polylines = stamps_to_polylines(stamps, symbol, config);
    if polylines.is_empty() {
        return (vec![], vec![]);
    }

    build_line_vertices_multi_indexed(&polylines)
}

/// Precomputed LC pattern data for a specific symbol.
/// Stores the normalized segments and rendering metadata.
#[derive(Debug, Clone)]
pub struct LcPatternData {
    /// Symbol name for debugging
    pub name: String,
    /// Advance distance in S-52 units
    pub advance_s52: u32,
    /// Symbol width/height in S-52 units
    pub size_s52: (u32, u32),
    /// Normalized line segments (x0, y0, x1, y1, width)
    pub segments: Vec<(f32, f32, f32, f32, u8)>,
    /// S-52 color token
    pub color_ref: String,
}

impl LcPatternData {
    /// Create pattern data from a LineStyleSymbol
    pub fn from_symbol(symbol: &LineStyleSymbol) -> Self {
        Self {
            name: symbol.name.clone(),
            advance_s52: symbol.width,
            size_s52: (symbol.width, symbol.height),
            segments: symbol.normalized_segments(),
            color_ref: symbol.color_ref.clone(),
        }
    }
}

/// Table of precomputed LC pattern data for fast rendering
pub struct LcPatternTable {
    patterns: std::collections::HashMap<String, LcPatternData>,
}

impl LcPatternTable {
    /// Create pattern table from a LineStyleTable
    pub fn from_line_style_table(table: &LineStyleTable) -> Self {
        let mut patterns = std::collections::HashMap::new();

        for name in table.names() {
            if let Some(symbol) = table.get(name) {
                let pattern = LcPatternData::from_symbol(symbol);
                if !pattern.segments.is_empty() {
                    patterns.insert(name.clone(), pattern);
                }
            }
        }

        Self { patterns }
    }

    /// Get pattern data by symbol name
    pub fn get(&self, name: &str) -> Option<&LcPatternData> {
        self.patterns.get(name)
    }

    /// Number of patterns loaded
    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_stamps_simple() {
        // Simple horizontal line
        let polyline = vec![[0.0, 0.0], [100.0, 0.0]];

        // Mock symbol with 10 unit advance
        let symbol = LineStyleSymbol {
            name: "TEST".to_string(),
            rcid: 1,
            width: 1000, // 10mm in S-52 units
            height: 500,
            origin_x: 0,
            origin_y: 0,
            pivot_x: 0,
            pivot_y: 0,
            description: "Test".to_string(),
            hpgl: "PU0,0;PD100,0;".to_string(),
            color_ref: "CHBLK".to_string(),
        };

        let config = LcRenderConfig::new(96.0, 1.0); // 96 DPI, 1m/px
        let color = [1.0, 0.0, 0.0, 1.0];

        let stamps = generate_stamps_along_polyline(&polyline, &symbol, &config, color);
        assert!(!stamps.is_empty(), "Should generate stamps");

        // All stamps should have angle = 0 (horizontal line)
        for stamp in &stamps {
            assert!(stamp.angle.abs() < 0.01, "Stamps should be horizontal");
        }
    }

    #[test]
    fn test_lc_pattern_table() {
        let table = LineStyleTable::load_from_xml(
            "assets/s52/chartsymbols.xml"
        ).unwrap();

        let patterns = LcPatternTable::from_line_style_table(&table);
        assert!(!patterns.is_empty(), "Should have patterns");

        // Check LOWACC21 is available
        let lowacc = patterns.get("LOWACC21");
        assert!(lowacc.is_some(), "LOWACC21 should be in pattern table");

        let pattern = lowacc.unwrap();
        assert_eq!(pattern.advance_s52, 130);
        assert!(!pattern.segments.is_empty(), "Should have segments");
    }

    #[test]
    fn test_stamps_to_vertices() {
        let stamps = vec![
            LcStamp {
                position: [100.0, 200.0],
                angle: 0.0,
                scale: 10.0,
                color: [1.0, 0.0, 0.0, 1.0],
            },
        ];

        let symbol = LineStyleSymbol {
            name: "TEST".to_string(),
            rcid: 1,
            width: 100,
            height: 50,
            origin_x: 0,
            origin_y: 0,
            pivot_x: 0,
            pivot_y: 0,
            description: "Test".to_string(),
            hpgl: "PU0,0;PD100,0;".to_string(),
            color_ref: "CHBLK".to_string(),
        };

        let config = LcRenderConfig::new(96.0, 1.0);
        let (vertices, indices) = stamps_to_line_vertices(&stamps, &symbol, &config);

        // Should have vertices for the line segment
        assert!(!vertices.is_empty(), "Should have vertices");
        assert!(!indices.is_empty(), "Should have indices");
    }

    #[test]
    fn test_stamps_rotated_polyline() {
        // 45-degree diagonal line
        let polyline = vec![[0.0, 0.0], [100.0, 100.0]];

        let symbol = LineStyleSymbol {
            name: "TEST".to_string(),
            rcid: 1,
            width: 1000, // 10mm in S-52 units
            height: 500,
            origin_x: 0,
            origin_y: 0,
            pivot_x: 0,
            pivot_y: 0,
            description: "Test".to_string(),
            hpgl: "PU0,0;PD100,0;".to_string(),
            color_ref: "CHBLK".to_string(),
        };

        let config = LcRenderConfig::new(96.0, 1.0);
        let color = [1.0, 0.0, 0.0, 1.0];

        let stamps = generate_stamps_along_polyline(&polyline, &symbol, &config, color);
        assert!(!stamps.is_empty(), "Should generate stamps");

        // All stamps should have angle = π/4 (45 degrees)
        let expected_angle = std::f32::consts::FRAC_PI_4;
        for stamp in &stamps {
            assert!(
                (stamp.angle - expected_angle).abs() < 0.01,
                "Stamp angle {} should be ~{}", stamp.angle, expected_angle
            );
        }
    }

    #[test]
    fn test_lowacc21_end_to_end() {
        // Load actual LOWACC21 symbol
        let table = LineStyleTable::load_from_xml(
            "assets/s52/chartsymbols.xml"
        ).unwrap();

        let symbol = table.get("LOWACC21").expect("LOWACC21 should exist");

        // Simple polyline in Mercator meters
        let polyline = vec![[0.0, 0.0], [50.0, 0.0]];

        // Realistic config: 96 DPI, 10m per pixel (roughly zoom 13)
        let config = LcRenderConfig::new(96.0, 10.0);
        let color = [0.0, 0.0, 0.0, 1.0];

        let stamps = generate_stamps_along_polyline(&polyline, symbol, &config, color);
        println!("LOWACC21 stamps on 50m line at 10m/px: {}", stamps.len());

        // Should have reasonable number of stamps
        assert!(stamps.len() > 0 && stamps.len() < 100,
            "Should have reasonable stamp count, got {}", stamps.len());

        // Generate vertices
        let (vertices, indices) = stamps_to_line_vertices(&stamps, symbol, &config);
        println!("  -> {} vertices, {} indices", vertices.len(), indices.len());
        assert!(!vertices.is_empty(), "Should produce vertices");
        assert!(!indices.is_empty(), "Should produce indices");
    }

    #[test]
    fn test_lc_scale_at_zoom_levels() {
        let table = LineStyleTable::load_from_xml(
            "assets/s52/chartsymbols.xml"
        ).unwrap();

        println!("\n=== LC Pattern Scale Analysis ===\n");

        for sym_name in &["LOWACC21", "NAVARE51"] {
            if let Some(symbol) = table.get(sym_name) {
                println!("{}:", sym_name);
                println!("  S-52 units: width={}, height={}", symbol.width, symbol.height);
                println!("  Physical: width={}mm, height={}mm",
                    symbol.width as f32 * 0.01,
                    symbol.height as f32 * 0.01);

                let ppmm = 96.0 / 25.4; // 3.78 ppmm at 96 DPI
                let width_px = symbol.width_pixels(ppmm);
                let height_px = symbol.height_pixels(ppmm);
                println!("  At 96 DPI: width={}px, height={}px", width_px, height_px);

                // Test at different zoom levels
                for (zoom, mpp) in [(7, 1200.0), (10, 150.0), (13, 19.0), (16, 2.4)] {
                    let advance_m = width_px * mpp;
                    let stamp_size_m = width_px * mpp; // in world coords

                    println!("  Zoom ~{}: mpp={:.1}, advance={:.0}m, stamp_size={:.0}m (=> {:.1}px on screen)",
                        zoom, mpp, advance_m, stamp_size_m, stamp_size_m / mpp);
                }
                println!();
            }
        }
    }

    #[test]
    fn test_lc_render_config() {
        let config = LcRenderConfig::new(96.0, 10.0);

        // 96 DPI = 96/25.4 ≈ 3.78 ppmm
        assert!((config.ppmm - 3.78).abs() < 0.1, "ppmm should be ~3.78");

        // 10m per pixel => 0.1 px per meter
        assert!((config.px_per_meter() - 0.1).abs() < 0.01, "px_per_meter should be 0.1");

        // Test zero meters_per_pixel edge case
        let zero_config = LcRenderConfig::new(96.0, 0.0);
        assert_eq!(zero_config.px_per_meter(), 1.0, "Should default to 1.0");
    }

    #[test]
    fn test_stamps_short_polyline() {
        // Very short polyline - should still generate at least one stamp
        let polyline = vec![[0.0, 0.0], [1.0, 0.0]];

        let symbol = LineStyleSymbol {
            name: "TEST".to_string(),
            rcid: 1,
            width: 100, // 1mm in S-52 units
            height: 50,
            origin_x: 0,
            origin_y: 0,
            pivot_x: 0,
            pivot_y: 0,
            description: "Test".to_string(),
            hpgl: "PU0,0;PD100,0;".to_string(),
            color_ref: "CHBLK".to_string(),
        };

        let config = LcRenderConfig::new(96.0, 0.01); // 1cm per pixel - high zoom
        let color = [1.0, 0.0, 0.0, 1.0];

        let stamps = generate_stamps_along_polyline(&polyline, &symbol, &config, color);
        // May or may not have stamps depending on advance vs line length
        println!("Short polyline stamps: {}", stamps.len());
    }

    #[test]
    fn test_compute_phase_offset_deterministic() {
        // Same inputs should always produce the same phase offset
        let phase1 = compute_phase_offset(123, [1000.0, 2000.0], "COALNE", 10.0);
        let phase2 = compute_phase_offset(123, [1000.0, 2000.0], "COALNE", 10.0);
        assert_eq!(phase1, phase2, "Phase offset should be deterministic");

        // Different inputs should produce different offsets (with high probability)
        let phase3 = compute_phase_offset(124, [1000.0, 2000.0], "COALNE", 10.0);
        assert_ne!(phase1, phase3, "Different lookup_id should give different phase");

        let phase4 = compute_phase_offset(123, [1001.0, 2000.0], "COALNE", 10.0);
        assert_ne!(phase1, phase4, "Different first_point should give different phase");

        let phase5 = compute_phase_offset(123, [1000.0, 2000.0], "SLCONS", 10.0);
        assert_ne!(phase1, phase5, "Different acronym should give different phase");
    }

    #[test]
    fn test_compute_phase_offset_in_range() {
        // Phase offset should always be in [0, advance_meters)
        for advance in [0.1, 1.0, 10.0, 100.0, 1000.0] {
            for i in 0..100 {
                let phase = compute_phase_offset(
                    i as u32,
                    [i as f32 * 100.0, i as f32 * 200.0],
                    "TEST",
                    advance,
                );
                assert!(phase >= 0.0, "Phase should be >= 0");
                assert!(phase < advance, "Phase {} should be < advance {}", phase, advance);
            }
        }
    }

    #[test]
    fn test_phase_continuity_across_tile_boundary() {
        // Test that stamps align when a polyline is clipped at a tile boundary
        // Simulates a feature crossing from tile A into tile B

        let symbol = LineStyleSymbol {
            name: "TEST".to_string(),
            rcid: 1,
            width: 1000, // 10mm in S-52 units -> advance
            height: 500,
            origin_x: 0,
            origin_y: 0,
            pivot_x: 0,
            pivot_y: 0,
            description: "Test".to_string(),
            hpgl: "PU0,0;PD100,0;".to_string(),
            color_ref: "CHBLK".to_string(),
        };

        let config = LcRenderConfig::new(96.0, 1.0); // 1m per pixel
        let color = [1.0, 0.0, 0.0, 1.0];

        // Original full polyline (100m horizontal)
        let full_polyline = vec![[0.0, 0.0], [100.0, 0.0]];

        // Compute advance in meters
        let advance_px = symbol.width_pixels(config.ppmm);
        let advance_meters = advance_px * config.meters_per_pixel;

        // Compute deterministic phase offset (same for both tiles)
        let first_point = full_polyline[0];
        let phase_offset = compute_phase_offset(42, first_point, "TEST", advance_meters);

        // "Tile A" gets first half: [0, 50]
        let tile_a_polyline = vec![[0.0, 0.0], [50.0, 0.0]];
        let stamps_a = generate_stamps_along_polyline_with_phase(
            &tile_a_polyline, &symbol, &config, color, phase_offset
        );

        // "Tile B" gets second half: [50, 100]
        // Same phase offset because same feature identity
        let tile_b_polyline = vec![[50.0, 0.0], [100.0, 0.0]];
        let stamps_b = generate_stamps_along_polyline_with_phase(
            &tile_b_polyline, &symbol, &config, color, phase_offset
        );

        // Full polyline stamps
        let stamps_full = generate_stamps_along_polyline_with_phase(
            &full_polyline, &symbol, &config, color, phase_offset
        );

        println!("Advance: {} meters", advance_meters);
        println!("Phase offset: {}", phase_offset);
        println!("Stamps in tile A: {} (x: {:?})",
            stamps_a.len(),
            stamps_a.iter().map(|s| s.position[0]).collect::<Vec<_>>()
        );
        println!("Stamps in tile B: {} (x: {:?})",
            stamps_b.len(),
            stamps_b.iter().map(|s| s.position[0]).collect::<Vec<_>>()
        );
        println!("Stamps in full: {} (x: {:?})",
            stamps_full.len(),
            stamps_full.iter().map(|s| s.position[0]).collect::<Vec<_>>()
        );

        // Combined stamps from tiles should have same count as full polyline
        // (accounting for tile boundary where stamp might be in either tile)
        let combined_count = stamps_a.len() + stamps_b.len();
        let full_count = stamps_full.len();

        // Allow for up to 1 stamp difference due to boundary handling
        assert!(
            (combined_count as i32 - full_count as i32).abs() <= 1,
            "Combined tiles ({}) should have ~same stamps as full polyline ({})",
            combined_count, full_count
        );

        // If we have stamps in both tiles, verify spacing is maintained
        if !stamps_a.is_empty() && !stamps_b.is_empty() {
            // Last stamp in tile A
            let last_a = stamps_a.last().unwrap().position[0];
            // First stamp in tile B
            let first_b = stamps_b[0].position[0];

            // Distance between should be approximately advance_meters
            let gap = first_b - last_a;
            let epsilon = advance_meters * 0.1; // 10% tolerance

            // Gap should be close to advance_meters (or 0 if stamp is at boundary)
            assert!(
                (gap - advance_meters).abs() < epsilon || gap < epsilon,
                "Gap between tiles ({}) should be ~advance ({}) or ~0",
                gap, advance_meters
            );
        }
    }
}
