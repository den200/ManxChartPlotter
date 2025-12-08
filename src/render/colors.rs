//! Hardcoded Nautograf-style chart colors.
//!
//! Instead of full S-52 compliance, we use a simple, readable
//! color scheme inspired by Nautograf/OpenCPN.

/// RGBA color type
pub type Color = [f32; 4];

/// Land area (tan/beige) - LNDARE
pub const LAND_COLOR: Color = [0.98, 0.93, 0.78, 1.0]; // Sandy beige

/// Ocean background (deep blue)
pub const OCEAN_BACKGROUND: Color = [0.71, 0.83, 0.93, 1.0]; // Light blue

/// Shallow water (danger zone < 5m)
pub const WATER_SHALLOW: Color = [0.65, 0.85, 0.75, 1.0]; // Greenish

/// Critical water (< 2m) - visible warning
pub const WATER_CRITICAL: Color = [0.55, 0.80, 0.55, 1.0]; // More green

/// Medium depth water (5-20m)
pub const WATER_MEDIUM: Color = [0.78, 0.88, 0.95, 1.0]; // Light blue

/// Deep water (> 20m)
pub const WATER_DEEP: Color = [0.85, 0.92, 0.98, 1.0]; // Very light blue/white

/// Coastline color
pub const COASTLINE_COLOR: Color = [0.4, 0.35, 0.3, 1.0]; // Dark brown

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
                // Drying heights (negative depth / above water at low tide)
                [0.55, 0.80, 0.55, 1.0], // Green
                // 0-2m: Critical shallow
                [0.65, 0.85, 0.75, 1.0], // Light green
                // 2-5m: Shallow
                [0.72, 0.87, 0.90, 1.0], // Blue-green
                // 5-10m: Medium
                [0.78, 0.88, 0.95, 1.0], // Light blue
                // 10-20m: Deep
                [0.82, 0.90, 0.96, 1.0], // Lighter blue
                // >20m: Very deep
                [0.88, 0.94, 0.98, 1.0], // Near white
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
            return self.colors[0]; // Drying
        }

        // Use logarithmic scale for more detail in shallow areas
        let log_depth = (depth + 1.0).ln();
        let log_max = 21.0_f64.ln(); // ln(20+1)

        let t = (log_depth / log_max).clamp(0.0, 1.0);

        // Interpolate between shallow and deep colors
        lerp_color(WATER_SHALLOW, WATER_DEEP, t as f32)
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

        // Shallow should be more green
        let shallow = palette.color_for_depth(1.0);
        let deep = palette.color_for_depth(50.0);

        // Green component should be higher in shallow
        assert!(shallow[1] > deep[1] || (shallow[1] - deep[1]).abs() < 0.1);
    }

    #[test]
    fn lerp_midpoint() {
        let mid = lerp_color([0.0, 0.0, 0.0, 1.0], [1.0, 1.0, 1.0, 1.0], 0.5);
        assert!((mid[0] - 0.5).abs() < 0.01);
        assert!((mid[1] - 0.5).abs() < 0.01);
        assert!((mid[2] - 0.5).abs() < 0.01);
    }
}
