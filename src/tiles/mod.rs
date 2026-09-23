//! WebMercator tile system for multi-chart rendering.
//!
//! Implements z/x/y slippy-map tiles (256px) with proper y-inversion
//! and x-wrap handling for dateline crossing.

pub mod clip;
pub mod builder;
pub mod scene;
pub mod cache;
pub mod worker;

use crate::s52::LineStyleKey;

/// Line style identifiers for multi-class rendering (DEPRECATED - use LineBatchKey).
/// Each ID maps to a specific S-52 LineStyle in s52_styles.rs.
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub enum LineStyleId {
    /// Coastline (COALNE) - dashed, gray
    Coastline,
    /// Shoreline construction default (SLCONS) - solid, width 2
    ShorelineConstruction,
    /// Shoreline construction wharf/pier (SLCONS_WHARF) - solid, width 4
    ShorelineConstructionWharf,
    /// Depth contour (DEPCNT) - solid, gray-blue
    DepthContour,
    /// Depth contour safety (DEPCNT_SAFETY) - solid, thicker
    DepthContourSafety,
    /// Cable overhead (CBLOHD) - dashed, gray
    CableOverhead,
    /// Cable submarine (CBLSUB) - dashed, magenta
    CableSubmarine,
    /// Traffic separation line (TSELNE) - solid, magenta
    TrafficSeparationLine,
    /// Road (ROADWY) - solid, brown
    Road,
    /// River bank (RIVBNK) - dotted
    RiverBank,
    /// Pipeline (PIPSOL) - solid, gray
    Pipeline,
}

/// Batch key for line grouping - combines render pass with S-52 style and priority.
///
/// The `pass` field supports multi-LS instructions for casings:
/// - `LS(SOLD,4,CHBLK);LS(SOLD,2,CHGRD)` → pass 0 = outline, pass 1 = fill
/// - Lower pass numbers draw first (underneath)
///
/// Batches are sorted by (disp_prio, pass, style, lookup_id) for correct draw order.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct LineBatchKey {
    /// S-52 display priority (controls draw order across feature classes)
    pub disp_prio: u8,
    /// Render pass order (0 = first LS in instruction, 1 = second, etc.)
    pub pass: u8,
    /// S-52 line style (pattern, width, color token)
    pub style: LineStyleKey,
    /// Lookup ID for debugging (0 if unknown)
    pub lookup_id: u32,
    /// Whether this batch comes from a background chart (covered by a more-detailed chart).
    /// Background batches are drawn with stencil test (pass when stencil==0).
    pub is_background: bool,
}

impl LineBatchKey {
    /// Create a new batch key with full metadata
    pub fn new_with_priority(disp_prio: u8, pass: u8, style: LineStyleKey, lookup_id: u32, is_background: bool) -> Self {
        Self { disp_prio, pass, style, lookup_id, is_background }
    }

    /// Create a new batch key with default priority (for backward compatibility)
    pub fn new(pass: u8, style: LineStyleKey) -> Self {
        Self {
            disp_prio: 4, // Default: Line priority
            pass,
            style,
            lookup_id: 0,
            is_background: false,
        }
    }

    /// Create a sortable key tuple for ordering batches.
    /// Returns (disp_prio, pass, style, lookup_id) for consistent ordering.
    pub fn sort_key(&self) -> (u8, u8, &LineStyleKey, u32) {
        (self.disp_prio, self.pass, &self.style, self.lookup_id)
    }
}

impl Ord for LineBatchKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.disp_prio
            .cmp(&other.disp_prio)
            // Background before foreground at same priority (bg drawn first, then fg on top)
            .then(self.is_background.cmp(&other.is_background).reverse())
            .then(self.pass.cmp(&other.pass))
            .then(self.style.cmp(&other.style))
            .then(self.lookup_id.cmp(&other.lookup_id))
    }
}

impl PartialOrd for LineBatchKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Earth radius (scaled, matches src/senc/symbol_lookup.rs)
const R: f64 = 6378137.0 * 0.9996;

/// WebMercator uses a SQUARE world: x ∈ [-πR, +πR], y ∈ [-πR, +πR]
/// (y is bounded by the ±85.0511° latitude clamp, not by geometry)
const ORIGIN_SHIFT: f64 = std::f64::consts::PI * R;  // πR ≈ 20M meters (half-width)
const WORLD_WIDTH: f64 = 2.0 * ORIGIN_SHIFT;         // 2πR ≈ 40M meters (full span)
const MAX_Y: f64 = ORIGIN_SHIFT;                     // Same as ORIGIN_SHIFT for square tiles

/// Web Mercator latitude bounds (avoid infinity at poles)
const MAX_LAT: f64 = 85.0511;

/// WebMercator tile identifier (slippy-map convention)
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub struct TileId {
    /// Zoom level 0-18
    pub z: u8,
    /// Tile column (0 = west, wraps at dateline)
    pub x: u32,
    /// Tile row (0 = north in slippy-map convention)
    pub y: u32,
}

impl TileId {
    /// Create a new tile ID
    pub fn new(z: u8, x: u32, y: u32) -> Self {
        Self { z, x, y }
    }

    /// Convert tile ID to Mercator bounds
    ///
    /// KEY: Slippy-map y=0 is NORTH, but Mercator y increases NORTH
    /// So we invert: tile_y=0 → max Mercator y (ORIGIN_SHIFT)
    pub fn bounds(&self) -> TileBounds {
        let n = (1u64 << self.z) as f64;
        let tile_size = WORLD_WIDTH / n;

        // X: tile 0 starts at -ORIGIN_SHIFT (= -πR)
        let min_x = -ORIGIN_SHIFT + self.x as f64 * tile_size;
        let max_x = min_x + tile_size;

        // Y is INVERTED: tile y=0 is top (north = +MAX_Y), y=n-1 is bottom (south = -MAX_Y)
        let max_y = MAX_Y - self.y as f64 * tile_size;
        let min_y = max_y - tile_size;

        TileBounds { min_x, max_x, min_y, max_y }
    }

    /// The point this tile's geometry is measured from: its centre, in
    /// global Mercator metres. Every position in the tile's packet is
    /// relative to it (see `builder::tile_relative`), and the renderer draws
    /// the tile with the camera re-based on it.
    pub fn origin(&self) -> glam::DVec2 {
        let (x, y) = self.bounds().center();
        glam::DVec2::new(x, y)
    }

    /// Get tile containing a Mercator point
    pub fn from_mercator(mx: f64, my: f64, z: u8) -> Self {
        let n_i = 1i64 << z;
        let n = n_i as f64;
        let tile_size = WORLD_WIDTH / n;

        // X with wrapping for ±180° crossing (dateline)
        let x_raw = ((mx + ORIGIN_SHIFT) / tile_size).floor() as i64;
        let x = x_raw.rem_euclid(n_i) as u32;  // Wrap mod 2^z

        // Y inverted, clamped with explicit integer bounds
        let y_raw = ((MAX_Y - my) / tile_size).floor() as i64;
        let y = y_raw.clamp(0, n_i - 1) as u32;

        Self { z, x, y }
    }
}

/// Tile bounds in global Mercator meters
#[derive(Debug, Clone, Copy, Default)]
pub struct TileBounds {
    pub min_x: f64,
    pub max_x: f64,
    pub min_y: f64,
    pub max_y: f64,
}

impl TileBounds {
    /// Create bounds from min/max values
    pub fn new(min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> Self {
        Self { min_x, max_x, min_y, max_y }
    }

    /// Expand bounds by a factor (e.g., 1.2 = 20% larger)
    pub fn expand(&self, factor: f32) -> Self {
        let factor = factor as f64;
        let half_w = (self.max_x - self.min_x) / 2.0;
        let half_h = (self.max_y - self.min_y) / 2.0;
        let cx = (self.min_x + self.max_x) / 2.0;
        let cy = (self.min_y + self.max_y) / 2.0;

        Self {
            min_x: cx - half_w * factor,
            max_x: cx + half_w * factor,
            min_y: cy - half_h * factor,
            max_y: cy + half_h * factor,
        }
    }

    /// Check if this bounds intersects another
    pub fn intersects(&self, other: &TileBounds) -> bool {
        !(self.max_x < other.min_x || self.min_x > other.max_x ||
          self.max_y < other.min_y || self.min_y > other.max_y)
    }

    /// Union of two bounds
    pub fn union(&self, other: &TileBounds) -> Self {
        Self {
            min_x: self.min_x.min(other.min_x),
            max_x: self.max_x.max(other.max_x),
            min_y: self.min_y.min(other.min_y),
            max_y: self.max_y.max(other.max_y),
        }
    }

    /// Width in meters
    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }

    /// Height in meters
    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }

    /// Center point
    pub fn center(&self) -> (f64, f64) {
        ((self.min_x + self.max_x) / 2.0, (self.min_y + self.max_y) / 2.0)
    }
}

/// WGS84 to Mercator meters (with latitude + output clamp for safety)
pub fn latlon_to_mercator(lat: f64, lon: f64) -> (f64, f64) {
    let lat_clamped = lat.clamp(-MAX_LAT, MAX_LAT);  // ±85.0511° safe range
    let x = lon.to_radians() * R;
    let y = ((lat_clamped.to_radians() / 2.0 + std::f64::consts::FRAC_PI_4).tan()).ln() * R;
    // Also clamp output y to guarantee tile math never sees out-of-range values
    (x, y.clamp(-MAX_Y, MAX_Y))
}

/// Mercator meters to WGS84 (lat, lon) with latitude clamp.
pub fn mercator_to_latlon(x: f64, y: f64) -> (f64, f64) {
    let lon = (x / R).to_degrees();
    let lat = (2.0 * (y / R).exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees();
    (lat.clamp(-MAX_LAT, MAX_LAT), lon)
}

/// Convert local SM coordinates to global Mercator
///
/// SM coords are relative to (ref_lat, ref_lon) - just add offset
pub fn sm_to_global(sm_x: f32, sm_y: f32, ref_lat: f64, ref_lon: f64) -> (f64, f64) {
    let (ref_mx, ref_my) = latlon_to_mercator(ref_lat, ref_lon);
    (ref_mx + sm_x as f64, ref_my + sm_y as f64)
}

/// Compute tile zoom level from camera's meters-per-pixel
///
/// Clamps to z >= 6 to prevent world-scale tiles that exceed GPU buffer limits.
/// At z=0, all charts would be packed into one tile (~300MB+ vertices).
pub fn zoom_from_camera(meters_per_pixel: f32) -> u8 {
    let z = (WORLD_WIDTH / (256.0 * meters_per_pixel as f64)).log2();
    z.round().clamp(6.0, 18.0) as u8  // Min z=6 to avoid buffer overflow
}

/// Raw (fractional) zoom level from camera's meters-per-pixel.
/// Used for hysteresis logic — callers compare against current_z.
pub fn zoom_from_camera_raw(meters_per_pixel: f32) -> f64 {
    let z = (WORLD_WIDTH / (256.0 * meters_per_pixel as f64)).log2();
    z.clamp(6.0, 18.0)
}

/// Meters per pixel at a given zoom level
pub fn meters_per_pixel(z: u8) -> f64 {
    WORLD_WIDTH / (256.0 * (1u64 << z) as f64)
}

/// Get all tiles visible in viewport (with margin for hysteresis)
///
/// Handles x-wrap when bounds cross ±ORIGIN_SHIFT (dateline)
pub fn visible_tiles(bounds: &TileBounds, z: u8, margin_factor: f32) -> Vec<TileId> {
    let n_i = 1i64 << z;
    let tile_size = WORLD_WIDTH / n_i as f64;

    // Expand bounds by margin factor
    let expanded = bounds.expand(margin_factor);

    // X tile range (may wrap around dateline)
    // Use floor for min, and subtract epsilon for max to handle exact boundaries
    let x_min_raw = ((expanded.min_x + ORIGIN_SHIFT) / tile_size).floor() as i64;
    // Subtract tiny epsilon to avoid including tile when max_x is exactly on boundary
    let x_max_raw = ((expanded.max_x + ORIGIN_SHIFT - 1e-9) / tile_size).floor() as i64;

    // GUARD: If bounds span full world width, just iterate all x tiles
    let x_span = x_max_raw - x_min_raw + 1;
    let (x_start, x_count) = if x_span >= n_i {
        (0i64, n_i)  // All tiles in row
    } else {
        (x_min_raw, x_span)
    };

    // Y tile range (clamped, no wrap)
    // Same epsilon handling for y
    let y_min = ((MAX_Y - expanded.max_y + 1e-9) / tile_size).floor() as i64;
    let y_max = ((MAX_Y - expanded.min_y) / tile_size).floor() as i64;
    let y_min = y_min.clamp(0, n_i - 1);
    let y_max = y_max.clamp(0, n_i - 1);

    let mut tiles = Vec::new();
    for y in y_min..=y_max {
        // Handle x wrap: iterate from x_start for x_count tiles
        for i in 0..x_count {
            let x = (x_start + i).rem_euclid(n_i) as u32;
            tiles.push(TileId { z, x, y: y as u32 });
        }
    }
    tiles
}

/// Visible tiles for a tilted view, refined by distance.
///
/// A plan view wants one zoom level: every pixel is the same number of metres,
/// so every tile should be built at the same chart scale. Tilted, that stops
/// being true — the top of the screen can be three or four screen-heights away,
/// where a tile covers a fraction of the pixels it covers in the foreground.
/// Building those at the foreground's scale is what would make the tilted view
/// cost several times the plan view for detail no one can see.
///
/// So instead of one level this walks the quadtree from `z_min` and stops
/// subdividing a tile once its own zoom matches the zoom the perspective wants
/// where it sits — `mpp_at` gives the metres per pixel on the ground at a
/// point, and `zoom_from_camera` turns that back into a level.
///
/// `contains` decides whether a tile is in view at all; it is given the tile's
/// bounds and must account for the trapezium (and for anything behind the eye,
/// which projects back onto the screen mirrored if it is not excluded here).
pub fn visible_tiles_lod(
    bounds: &TileBounds,
    z_min: u8,
    z_max: u8,
    mpp_at: &dyn Fn(f64, f64) -> f32,
    contains: &dyn Fn(&TileBounds) -> bool,
) -> Vec<TileId> {
    let mut out = Vec::new();
    let mut stack: Vec<TileId> = visible_tiles(bounds, z_min, 1.0);
    // A runaway refinement would be a hang, not a glitch: 4^(z_max - z_min)
    // tiles is millions if the stop rule is ever wrong about a tile.
    const MAX_TILES: usize = 4096;
    while let Some(t) = stack.pop() {
        if out.len() + stack.len() >= MAX_TILES {
            out.push(t);
            continue;
        }
        let b = t.bounds();
        if !contains(&b) {
            continue;
        }
        if t.z >= z_max {
            out.push(t);
            continue;
        }
        // The near corner decides: a tile is too coarse as soon as any part of
        // it is close enough to deserve more detail.
        let want = [
            (b.min_x, b.min_y),
            (b.max_x, b.min_y),
            (b.min_x, b.max_y),
            (b.max_x, b.max_y),
            ((b.min_x + b.max_x) * 0.5, (b.min_y + b.max_y) * 0.5),
        ]
        .iter()
        .map(|&(x, y)| zoom_from_camera(mpp_at(x, y)))
        .max()
        .unwrap_or(t.z);
        if want <= t.z {
            out.push(t);
            continue;
        }
        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            stack.push(TileId {
                z: t.z + 1,
                x: t.x * 2 + dx,
                y: t.y * 2 + dy,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_bounds_roundtrip() {
        // Test that from_mercator and bounds are consistent
        let z = 10u8;
        let tile = TileId::new(z, 512, 512);
        let bounds = tile.bounds();
        let center = bounds.center();
        let recovered = TileId::from_mercator(center.0, center.1, z);
        assert_eq!(tile, recovered);
    }

    #[test]
    fn tile_bounds_at_origin() {
        // Tile at zoom 0 should cover entire world
        let tile = TileId::new(0, 0, 0);
        let bounds = tile.bounds();
        assert!((bounds.min_x + ORIGIN_SHIFT).abs() < 1.0);
        assert!((bounds.max_x - ORIGIN_SHIFT).abs() < 1.0);
        assert!((bounds.min_y + MAX_Y).abs() < 1.0);
        assert!((bounds.max_y - MAX_Y).abs() < 1.0);
    }

    #[test]
    fn latlon_to_mercator_equator() {
        let (x, y) = latlon_to_mercator(0.0, 0.0);
        assert!(x.abs() < 1.0);
        assert!(y.abs() < 1.0);
    }

    #[test]
    fn latlon_to_mercator_poles_clamped() {
        // Extreme latitudes should be clamped
        let (_, y_north) = latlon_to_mercator(90.0, 0.0);
        let (_, y_south) = latlon_to_mercator(-90.0, 0.0);
        assert!(y_north <= MAX_Y);
        assert!(y_south >= -MAX_Y);
    }

    #[test]
    fn zoom_from_camera_at_world() {
        // At zoom 0, meters_per_pixel is WORLD_WIDTH / 256
        // But we clamp to minimum z=6 to avoid buffer overflow
        let mpp = WORLD_WIDTH as f32 / 256.0;
        let z = zoom_from_camera(mpp);
        assert_eq!(z, 6);  // Clamped to minimum
    }

    #[test]
    fn visible_tiles_at_zoom_0() {
        // Bounds slightly inside world edges to avoid exact boundary issues
        let bounds = TileBounds::new(-ORIGIN_SHIFT + 1.0, ORIGIN_SHIFT - 1.0, -MAX_Y + 1.0, MAX_Y - 1.0);
        let tiles = visible_tiles(&bounds, 0, 1.0);
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0], TileId::new(0, 0, 0));
    }

    #[test]
    fn visible_tiles_at_zoom_1() {
        // At zoom 1, 4 tiles cover the world
        // Test a bounds that should intersect 2 tiles
        let bounds = TileBounds::new(-1000.0, 1000.0, -1000.0, 1000.0);  // Small area at origin
        let tiles = visible_tiles(&bounds, 1, 1.0);
        // Should get 4 tiles (2x2 grid around origin)
        assert_eq!(tiles.len(), 4);
    }

    #[test]
    fn sm_to_global_identity_at_origin() {
        // At equator/prime meridian, SM coords should equal global coords
        let (gx, gy) = sm_to_global(0.0, 0.0, 0.0, 0.0);
        assert!(gx.abs() < 1.0);
        assert!(gy.abs() < 1.0);
    }
}
