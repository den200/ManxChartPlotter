//! Mariner settings for S-52 display control.

use super::DisplayCategory;

/// Depth unit display modes for soundings
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum DepthUnit {
    #[default]
    Meters = 0,
    Feet = 1,
    Fathoms = 2,
}

/// Depth shade mode per S52_MAR_TWO_SHADES
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
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
    /// Use symbolized area boundaries (LC patterns) vs plain (LS lines).
    /// Corresponds to S52_MAR_SYMBOLIZED_BND. Default: false — matches OpenCPN's
    /// `m_nBoundaryStyle = PLAIN_BOUNDARIES` (s52plib.cpp:308). With symbolized
    /// boundaries, area classes like CTNARE resolve to a stamped LC() boundary
    /// (e.g. magenta CTNARE51 caution symbols along the whole coast); PLAIN
    /// resolves them to a single dashed LS() line, as OpenCPN shows by default.
    pub symbolized_boundaries: bool,
    /// Suppress non-critical text labels (OpenCPN m_bShowS57ImportantTextOnly).
    /// When true, text with display-group (dis) >= 20 is filtered out.
    pub show_important_text_only: bool,
    /// Draw the chart coverage (M_COVR) outline rectangle. OpenCPN does not draw
    /// this in STANDARD display; gate it behind a separate setting rather than
    /// bypassing the category filter.
    pub show_chart_boundaries: bool,
    /// Prefer the Simplified point-symbol table over Paper Chart.
    /// Corresponds to OpenCPN's "simplified symbols" toggle.
    pub simplified_points: bool,
}

impl Default for MarinerSettings {
    fn default() -> Self {
        // Defaults per S52-RENDERING-SPEC.md Appendix B
        Self {
            show_displaybase: true,  // Always on (safety)
            show_standard: true,     // Default on
            show_other: false,       // OpenCPN on-screen default is STANDARD, not SHOW_ALL
            safety_depth: 10.0,      // 10 meters (S52_MAR_SAFETY_DEPTH) - per spec
            safety_contour: 10.0,    // 10 meters (S52_MAR_SAFETY_CONTOUR) - per spec
            shallow_contour: 2.0,    // 2 meters (S52_MAR_SHALLOW_CONTOUR)
            deep_contour: 30.0,      // 30 meters (S52_MAR_DEEP_CONTOUR)
            show_text: true,
            show_soundings: true,
            depth_unit: DepthUnit::default(), // Meters
            depth_shade_mode: DepthShadeMode::default(), // FourShades per spec
            symbolized_boundaries: false, // PLAIN boundaries by default (matches OpenCPN)
            // Default ON: until navcore has proper LOD / per-feature SCAMIN for
            // labels, leaving every dis>=20 label visible at overview zooms
            // produces an unreadable text stampede. OpenCPN's default is off,
            // but its labels are also culled by a proper chart-scale filter we
            // don't yet match.
            show_important_text_only: true,
            show_chart_boundaries: false,
            simplified_points: false,
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
