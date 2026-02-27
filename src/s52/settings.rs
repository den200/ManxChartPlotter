//! Mariner settings for S-52 display control.

use super::DisplayCategory;

/// Depth unit display modes for soundings
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DepthUnit {
    #[default]
    Meters = 0,
    Feet = 1,
    Fathoms = 2,
}

/// Depth shade mode per S52_MAR_TWO_SHADES
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DepthShadeMode {
    /// Two shades: safe (DEPMD) and unsafe (DEPVS)
    TwoShades,
    /// Four shades: DEPVS, DEPMS, DEPMD, DEPDW (default per spec)
    #[default]
    FourShades,
}

/// User-configurable display settings.
/// Controls which features are visible based on S-52 display categories.
#[derive(Debug, Clone)]
pub struct MarinerSettings {
    /// Show Displaybase features (always true for safety - ECDIS requirement)
    pub show_displaybase: bool,
    /// Show Standard features (default: true)
    pub show_standard: bool,
    /// Show Other features (default: false in OpenCPN)
    pub show_other: bool,
    /// Safety depth in meters (features shallower are highlighted)
    pub safety_depth: f32,
    /// Safety contour depth in meters (emphasized on chart)
    pub safety_contour: f32,
    /// Shallow contour depth in meters (OpenCPN S52_MAR_SHALLOW_CONTOUR, default 2m)
    pub shallow_contour: f32,
    /// Deep contour depth in meters (OpenCPN S52_MAR_DEEP_CONTOUR, default 30m)
    pub deep_contour: f32,
    /// Show text labels
    pub show_text: bool,
    /// Show soundings
    pub show_soundings: bool,
    /// Depth unit for display (meters, feet, or fathoms)
    pub depth_unit: DepthUnit,
    /// Depth shade mode: 2 or 4 shades (S52_MAR_TWO_SHADES)
    pub depth_shade_mode: DepthShadeMode,
}

impl Default for MarinerSettings {
    fn default() -> Self {
        // Defaults per S52-RENDERING-SPEC.md Appendix B
        Self {
            show_displaybase: true,  // Always on (safety)
            show_standard: true,     // Default on
            show_other: false,       // Default off (matches OpenCPN)
            safety_depth: 10.0,      // 10 meters (S52_MAR_SAFETY_DEPTH) - per spec
            safety_contour: 10.0,    // 10 meters (S52_MAR_SAFETY_CONTOUR) - per spec
            shallow_contour: 2.0,    // 2 meters (S52_MAR_SHALLOW_CONTOUR)
            deep_contour: 30.0,      // 30 meters (S52_MAR_DEEP_CONTOUR)
            show_text: true,
            show_soundings: true,
            depth_unit: DepthUnit::default(), // Meters
            depth_shade_mode: DepthShadeMode::default(), // FourShades per spec
        }
    }
}

impl MarinerSettings {
    /// Create settings with all categories visible
    pub fn show_all() -> Self {
        Self {
            show_displaybase: true,
            show_standard: true,
            show_other: true,
            ..Default::default()
        }
    }

    /// Check if a display category should be shown
    pub fn should_show(&self, category: DisplayCategory) -> bool {
        match category {
            DisplayCategory::Displaybase => self.show_displaybase,
            DisplayCategory::Standard => self.show_standard,
            DisplayCategory::Other => self.show_other,
            DisplayCategory::Mariners => self.show_standard, // Treat as standard
        }
    }
}
