//! S-52 DAY_BRIGHT chart colors from OpenCPN chartsymbols.xml.
//!
//! These are the exact colors used by OpenCPN for the Day Bright scheme.

#![allow(dead_code)]

/// RGBA color type
pub type Color = [f32; 4];

/// Land area - S-52 LANDF RGB(139, 102, 31) - dark brown for maritime charts
/// LANDF is the correct land color for Day Bright scheme (matches OpenCPN)
pub const LAND_COLOR: Color = [0.545, 0.400, 0.122, 1.0];

/// Ocean background - S-52 DEPDW RGB(212, 234, 238) - deep water
pub const OCEAN_BACKGROUND: Color = [0.831, 0.918, 0.933, 1.0];

/// Very shallow water (< 2m) - S-52 DEPVS RGB(115, 182, 239)
/// Blue (NOT pink!) - S-52 uses blue gradients for all water depths
pub const WATER_SHALLOW: Color = [0.451, 0.714, 0.937, 1.0];

/// Intertidal/drying water - S-52 DEPIT RGB(131, 178, 149)
/// Greenish for areas that dry at low tide
pub const WATER_INTERTIDAL: Color = [0.514, 0.698, 0.584, 1.0];

/// Medium depth water (5-10m) - S-52 DEPMD RGB(186, 213, 225)
pub const WATER_MEDIUM: Color = [0.729, 0.835, 0.882, 1.0];

/// Deep water (> 20m) - S-52 DEPDW RGB(212, 234, 238)
pub const WATER_DEEP: Color = [0.831, 0.918, 0.933, 1.0];

/// Coastline color - S-52 CSTLN RGB(82, 90, 92)
pub const COASTLINE_COLOR: Color = [0.322, 0.353, 0.361, 1.0];

/// Depth contour color
pub const CONTOUR_COLOR: Color = [0.5, 0.6, 0.65, 1.0]; // Gray-blue

// Note: Line styles (COASTLINE_STYLE, CONTOUR_STYLE) have been moved to s52_styles.rs
// for S-52 compliance. Import from super::s52_styles instead.

/// Shoreline construction (piers, jetties, seawalls) - SLCONS
pub const SHORELINE_CONSTRUCTION_COLOR: Color = [0.55, 0.50, 0.45, 1.0]; // Brown-gray

/// Road color - ROADWY
pub const ROAD_COLOR: Color = [0.6, 0.55, 0.5, 1.0]; // Light brown

/// Cable (overhead) - CBLOHD
pub const CABLE_OVERHEAD_COLOR: Color = [0.5, 0.5, 0.5, 1.0]; // Gray

/// Cable (submarine) - CBLSUB
pub const CABLE_SUBMARINE_COLOR: Color = [0.6, 0.4, 0.55, 1.0]; // Purple-gray

/// Traffic separation line - TSELNE
pub const TRAFFIC_SEPARATION_COLOR: Color = [0.7, 0.3, 0.6, 1.0]; // Magenta

/// Recommended route centerline - RCRTCL
pub const RECOMMENDED_ROUTE_COLOR: Color = [0.6, 0.4, 0.7, 1.0]; // Purple

/// Pipeline (submarine/on land) - PIPSOL
pub const PIPELINE_COLOR: Color = [0.6, 0.5, 0.3, 1.0]; // Brown

/// Ferry route - FERYRT
pub const FERRY_ROUTE_COLOR: Color = [0.5, 0.3, 0.6, 1.0]; // Purple

/// River bank - RIVBNK
pub const RIVER_BANK_COLOR: Color = [0.4, 0.5, 0.4, 1.0]; // Green-gray

/// Anchorage area - ACHARE (semi-transparent)
pub const ANCHORAGE_AREA_COLOR: Color = [0.6, 0.5, 0.8, 0.3]; // Light purple, transparent

/// Traffic separation zone - TSEZNE (semi-transparent)
pub const TRAFFIC_ZONE_COLOR: Color = [0.8, 0.4, 0.7, 0.3]; // Magenta, transparent

/// Restricted area - RESARE (semi-transparent)
pub const RESTRICTED_AREA_COLOR: Color = [0.9, 0.6, 0.6, 0.3]; // Light red, transparent

/// Fairway - FAIRWY (semi-transparent)
pub const FAIRWAY_COLOR: Color = [0.7, 0.7, 0.9, 0.2]; // Light blue, very transparent

/// Built-up area - BUAARE
pub const BUILT_UP_AREA_COLOR: Color = [0.85, 0.8, 0.75, 1.0]; // Light gray-brown

/// Lake - LAKARE
pub const LAKE_COLOR: Color = [0.7, 0.82, 0.92, 1.0]; // Light blue (like medium water)

/// Dredged area - DRGARE (semi-transparent)
pub const DREDGED_AREA_COLOR: Color = [0.6, 0.7, 0.8, 0.4]; // Blue-gray, transparent

/// Sea area / named water area - SEAARE (no fill, just for labeling)
pub const SEA_AREA_COLOR: Color = [0.0, 0.0, 0.0, 0.0]; // Transparent

/// Obstruction - OBSTRN
pub const OBSTRUCTION_COLOR: Color = [0.3, 0.3, 0.3, 0.5]; // Dark gray, semi-transparent

/// Preset depth color palette
pub struct DepthPalette {
    /// Depth breakpoints in meters
    pub breakpoints: [f64; 5],
    /// Colors for each depth range
    pub colors: [Color; 6],
}

impl Default for DepthPalette {
    fn default() -> Self {
        Self {
            // Standard depth breakpoints (meters)
            breakpoints: [0.0, 2.0, 5.0, 10.0, 20.0],
            colors: [
                // Drying/intertidal (negative depth) - S-52 DEPIT RGB(131, 178, 149)
                WATER_INTERTIDAL,
                // 0-2m: Very shallow - S-52 DEPVS RGB(115, 182, 239)
                WATER_SHALLOW,
                // 2-5m: Shallow - S-52 DEPMS RGB(152, 197, 242)
                [0.596, 0.773, 0.949, 1.0],
                // 5-10m: Medium - S-52 DEPMD RGB(186, 213, 225)
                WATER_MEDIUM,
                // 10-20m: Medium-deep - interpolated between DEPMD and DEPDW
                [0.780, 0.876, 0.908, 1.0],
                // >20m: Deep - S-52 DEPDW RGB(212, 234, 238)
                WATER_DEEP,
            ],
        }
    }
}

impl DepthPalette {
    /// Get color for a given depth in meters
    pub fn color_for_depth(&self, depth: f64) -> Color {
        // Find the appropriate color band
        for (i, &breakpoint) in self.breakpoints.iter().enumerate() {
            if depth < breakpoint {
                return self.colors[i];
            }
        }
        // Deeper than all breakpoints
        self.colors[5]
    }

    /// Get color with logarithmic interpolation for smoother gradients
    pub fn color_for_depth_smooth(&self, depth: f64) -> Color {
        if depth < 0.0 {
            return self.colors[0]; // Drying/intertidal (green)
        }

        // Use stepped bands matching S-52 depth classifications
        // Very shallow (0-2m): pinkish warning
        // Shallow (2-5m): light blue
        // Medium-deep (5-20m): lighter blue
        // Deep (>20m): background blue

        if depth < 2.0 {
            // Very shallow danger zone - pinkish
            let t = (depth / 2.0) as f32;
            return lerp_color(self.colors[1], self.colors[2], t);
        } else if depth < 5.0 {
            // Shallow - transition to medium
            let t = ((depth - 2.0) / 3.0) as f32;
            return lerp_color(self.colors[2], self.colors[3], t);
        } else if depth < 20.0 {
            // Medium to deep
            let t = ((depth - 5.0) / 15.0) as f32;
            return lerp_color(self.colors[3], self.colors[5], t);
        }

        // Deep water
        self.colors[5]
    }
}

/// Linear interpolation between two colors
pub fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3] + (b[3] - a[3]) * t,
    ]
}

/// Get color for a depth area feature
///
/// Uses average depth (DRVAL1 + DRVAL2) / 2 to determine color.
pub fn depth_color(drval1: Option<f64>, drval2: Option<f64>) -> Color {
    let palette = DepthPalette::default();

    let depth = match (drval1, drval2) {
        (Some(d1), Some(d2)) => (d1 + d2) / 2.0,
        (Some(d), None) | (None, Some(d)) => d,
        (None, None) => 10.0, // Default to medium depth if unknown
    };

    palette.color_for_depth_smooth(depth)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_colors_gradient() {
        let palette = DepthPalette::default();

        // S-52 depth colors: shallow is saturated blue, deep is pale/light
        let shallow = palette.color_for_depth(1.0);
        let deep = palette.color_for_depth(50.0);

        // Deep water is lighter (higher green component) in S-52 palette
        // DEPVS (shallow) = [0.451, 0.714, 0.937]
        // DEPDW (deep)    = [0.831, 0.918, 0.933]
        assert!(deep[1] >= shallow[1]); // Deep is lighter/paler
    }

    #[test]
    fn lerp_midpoint() {
        let mid = lerp_color([0.0, 0.0, 0.0, 1.0], [1.0, 1.0, 1.0, 1.0], 0.5);
        assert!((mid[0] - 0.5).abs() < 0.01);
        assert!((mid[1] - 0.5).abs() < 0.01);
        assert!((mid[2] - 0.5).abs() < 0.01);
    }
}
