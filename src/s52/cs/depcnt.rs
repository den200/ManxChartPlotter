//! DEPCNT02 - Depth Contour Conditional Symbology Procedure
//!
//! Determines depth contour styling based on:
//! - VALDCO: contour value relative to mariner's safety contour
//! - QUAPOS: position accuracy (low accuracy = dashed line)

use crate::s52::instruction::LinePattern;
use crate::s52::MarinerSettings;
use crate::senc::Feature;

/// S-52 depth contour style variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepthContourStyle {
    /// Safety contour - emphasized (thick line, distinct color)
    SafetyContour,
    /// Safety contour with low accuracy position - thick dashed
    SafetyContourLowAccuracy,
    /// Shallow contour (shallower than safety)
    ShallowContour,
    /// Shallow contour with low accuracy position
    ShallowContourLowAccuracy,
    /// Deep contour (deeper than safety)
    DeepContour,
    /// Deep contour with low accuracy position
    DeepContourLowAccuracy,
}

impl DepthContourStyle {
    /// Get the S-52 line pattern for this style
    pub fn pattern(&self) -> LinePattern {
        match self {
            Self::SafetyContour | Self::ShallowContour | Self::DeepContour => LinePattern::Solid,
            Self::SafetyContourLowAccuracy
            | Self::ShallowContourLowAccuracy
            | Self::DeepContourLowAccuracy => LinePattern::Dashed,
        }
    }

    /// Get the S-52 color token for this style
    pub fn color_token(&self) -> &'static str {
        match self {
            Self::SafetyContour | Self::SafetyContourLowAccuracy => "DEPSC",
            Self::ShallowContour
            | Self::ShallowContourLowAccuracy
            | Self::DeepContour
            | Self::DeepContourLowAccuracy => "DEPCN",
        }
    }

    /// Get the line width (S-52 units) for this style
    pub fn width(&self) -> u8 {
        match self {
            Self::SafetyContour | Self::SafetyContourLowAccuracy => 2,
            Self::ShallowContour
            | Self::ShallowContourLowAccuracy
            | Self::DeepContour
            | Self::DeepContourLowAccuracy => 1,
        }
    }

    /// Get the line width multiplier for this style (legacy compatibility)
    pub fn width_multiplier(&self) -> f32 {
        self.width() as f32
    }
}

/// DEPCNT02 - Depth contour styling per S-52
///
/// Returns style information based on:
/// 1. Position accuracy (QUAPOS >= 4 = low accuracy → dashed)
/// 2. Contour value (VALDCO) vs safety contour → color/width
///
/// # Arguments
/// * `feature` - DEPCNT feature with VALDCO and optional QUAPOS attributes
/// * `settings` - Mariner settings containing safety_contour
///
/// # Returns
/// Style enum with pattern, width, and color information
pub fn depcnt02(feature: &Feature, settings: &MarinerSettings) -> DepthContourStyle {
    let valdco = feature.valdco().unwrap_or(0.0) as f32;
    let safety = settings.safety_contour;
    let quapos = feature.attribute_int("QUAPOS").unwrap_or(1);
    // Per S52-RENDERING-SPEC.md Appendix M.2: quapos > 1 && quapos < 10 = uncertain
    // QUAPOS values 2-9 indicate position uncertainty
    let low_accuracy = quapos > 1 && quapos < 10;

    // Check if this is the safety contour (within 0.1m tolerance)
    let is_safety = (valdco - safety).abs() < 0.1;
    let is_shallow = valdco < safety;

    match (is_safety, is_shallow, low_accuracy) {
        (true, _, false) => DepthContourStyle::SafetyContour,
        (true, _, true) => DepthContourStyle::SafetyContourLowAccuracy,
        (false, true, false) => DepthContourStyle::ShallowContour,
        (false, true, true) => DepthContourStyle::ShallowContourLowAccuracy,
        (false, false, false) => DepthContourStyle::DeepContour,
        (false, false, true) => DepthContourStyle::DeepContourLowAccuracy,
    }
}

/// Get line style parameters for a depth contour feature
///
/// # Returns
/// (pattern, width, color_token) tuple for line styling
pub fn depcnt02_params(
    feature: &Feature,
    settings: &MarinerSettings,
) -> (LinePattern, u8, &'static str) {
    let style = depcnt02(feature, settings);
    (style.pattern(), style.width(), style.color_token())
}

/// Check if a depth contour is the safety contour (regardless of accuracy)
pub fn is_safety_contour(feature: &Feature, settings: &MarinerSettings) -> bool {
    matches!(
        depcnt02(feature, settings),
        DepthContourStyle::SafetyContour | DepthContourStyle::SafetyContourLowAccuracy
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contours these depth cases were written against. Pinned here so they
    /// test the procedure rather than `MarinerSettings::default()`, which
    /// tracks s52plib's own defaults and has changed under them once already.
    fn test_contours() -> MarinerSettings {
        MarinerSettings {
            safety_depth: 10.0,
            safety_contour: 10.0,
            shallow_contour: 2.0,
            deep_contour: 30.0,
            ..Default::default()
        }
    }

    use crate::senc::{AttributeValue, FeatureType, ObjectClass};

    fn make_depcnt(valdco: f64) -> Feature {
        make_depcnt_with_quapos(valdco, None)
    }

    /// DEPCNT02 promotes the selected safety contour to DISPLAYBASE and clears
    /// its SCAMIN — "The contour selected is highlighted as the safety contour
    /// and put in DISPLAYBASE", and s52plib sets `Scamin = 1e8+1` to match. It
    /// is the one line on the chart that must survive every filter, and it
    /// moves with the mariner's setting, so the promotion has to be evaluated
    /// per view rather than baked into the lookup table.
    #[test]
    fn safety_contour_is_promoted_and_follows_the_setting() {
        let mut settings = test_contours();
        settings.safety_contour = 5.0;
        assert!(is_safety_contour(&make_depcnt(5.0), &settings));
        assert!(!is_safety_contour(&make_depcnt(2.0), &settings));
        assert!(!is_safety_contour(&make_depcnt(10.0), &settings));

        // Change the mariner's setting and a different contour is promoted.
        settings.safety_contour = 2.0;
        assert!(is_safety_contour(&make_depcnt(2.0), &settings));
        assert!(!is_safety_contour(&make_depcnt(5.0), &settings));

        // Low positional accuracy changes the style, not the promotion: a
        // dashed safety contour is still the safety contour.
        assert!(is_safety_contour(
            &make_depcnt_with_quapos(2.0, Some(4)),
            &settings
        ));
    }

    fn make_depcnt_with_quapos(valdco: f64, quapos: Option<i32>) -> Feature {
        let mut attributes = crate::senc::Attributes::new();
        attributes.insert("VALDCO", AttributeValue::Float(valdco));
        if let Some(q) = quapos {
            attributes.insert("QUAPOS", AttributeValue::Integer(q));
        }
        Feature {
            type_code: 0,
            object_class: ObjectClass::DepthContour,
            feature_type: FeatureType::Line,
            attributes,
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        }
    }

    #[test]
    fn test_safety_contour() {
        // VALDCO = 10m = safety_contour (default)
        let feature = make_depcnt(10.0);
        let settings = test_contours();
        let style = depcnt02(&feature, &settings);
        assert_eq!(style, DepthContourStyle::SafetyContour);
        assert_eq!(style.width_multiplier(), 2.0);
        assert_eq!(style.color_token(), "DEPSC");
    }

    #[test]
    fn test_shallow_contour() {
        // VALDCO = 5m < 10m safety
        let feature = make_depcnt(5.0);
        let settings = test_contours();
        let style = depcnt02(&feature, &settings);
        assert_eq!(style, DepthContourStyle::ShallowContour);
        assert_eq!(style.width_multiplier(), 1.0);
    }

    #[test]
    fn test_deep_contour() {
        // VALDCO = 20m > 10m safety
        let feature = make_depcnt(20.0);
        let settings = test_contours();
        let style = depcnt02(&feature, &settings);
        assert_eq!(style, DepthContourStyle::DeepContour);
        assert_eq!(style.width_multiplier(), 1.0);
    }

    #[test]
    fn test_custom_safety_contour() {
        // With safety_contour = 5m
        let feature = make_depcnt(5.0);
        let mut settings = test_contours();
        settings.safety_contour = 5.0;
        assert!(is_safety_contour(&feature, &settings));
    }

    #[test]
    fn test_safety_tolerance() {
        // VALDCO = 10.05 should still match safety_contour = 10.0
        let feature = make_depcnt(10.05);
        let settings = test_contours();
        assert!(is_safety_contour(&feature, &settings));
    }

    // QUAPOS low-accuracy tests

    #[test]
    fn test_safety_contour_low_accuracy() {
        // Safety contour with QUAPOS=4 (approximate) should be dashed
        let feature = make_depcnt_with_quapos(10.0, Some(4));
        let settings = test_contours();
        let style = depcnt02(&feature, &settings);
        assert_eq!(style, DepthContourStyle::SafetyContourLowAccuracy);
        assert_eq!(style.pattern(), LinePattern::Dashed);
        assert_eq!(style.width(), 2); // Still thick
        assert_eq!(style.color_token(), "DEPSC"); // Still safety color
    }

    #[test]
    fn test_shallow_contour_low_accuracy() {
        // Shallow contour with QUAPOS=5 should be dashed
        let feature = make_depcnt_with_quapos(5.0, Some(5));
        let settings = test_contours();
        let style = depcnt02(&feature, &settings);
        assert_eq!(style, DepthContourStyle::ShallowContourLowAccuracy);
        assert_eq!(style.pattern(), LinePattern::Dashed);
        assert_eq!(style.width(), 1);
    }

    #[test]
    fn test_deep_contour_low_accuracy() {
        // Deep contour with QUAPOS=4 should be dashed
        let feature = make_depcnt_with_quapos(20.0, Some(4));
        let settings = test_contours();
        let style = depcnt02(&feature, &settings);
        assert_eq!(style, DepthContourStyle::DeepContourLowAccuracy);
        assert_eq!(style.pattern(), LinePattern::Dashed);
    }

    #[test]
    fn test_accurate_quapos_stays_solid() {
        // QUAPOS=1 (surveyed) is accurate - solid line
        // Per spec Appendix M.2: only quapos > 1 && quapos < 10 is uncertain
        let feature = make_depcnt_with_quapos(10.0, Some(1));
        let settings = test_contours();
        let style = depcnt02(&feature, &settings);
        assert_eq!(style, DepthContourStyle::SafetyContour);
        assert_eq!(style.pattern(), LinePattern::Solid);
    }

    #[test]
    fn test_quapos_2_is_low_accuracy() {
        // QUAPOS=2 is low accuracy (per spec: quapos > 1 && quapos < 10)
        let feature = make_depcnt_with_quapos(10.0, Some(2));
        let settings = test_contours();
        let style = depcnt02(&feature, &settings);
        assert_eq!(style, DepthContourStyle::SafetyContourLowAccuracy);
        assert_eq!(style.pattern(), LinePattern::Dashed);
    }

    #[test]
    fn test_quapos_3_is_low_accuracy() {
        // QUAPOS=3 is low accuracy (per spec: quapos > 1 && quapos < 10)
        let feature = make_depcnt_with_quapos(10.0, Some(3));
        let settings = test_contours();
        let style = depcnt02(&feature, &settings);
        assert_eq!(style, DepthContourStyle::SafetyContourLowAccuracy);
        assert_eq!(style.pattern(), LinePattern::Dashed);
    }

    #[test]
    fn test_is_safety_contour_with_low_accuracy() {
        // is_safety_contour should return true even for low-accuracy safety contours
        let feature = make_depcnt_with_quapos(10.0, Some(4));
        let settings = test_contours();
        assert!(is_safety_contour(&feature, &settings));
    }

    #[test]
    fn test_depcnt02_params_returns_tuple() {
        let feature = make_depcnt_with_quapos(10.0, Some(4));
        let settings = test_contours();
        let (pattern, width, color) = depcnt02_params(&feature, &settings);
        assert_eq!(pattern, LinePattern::Dashed);
        assert_eq!(width, 2);
        assert_eq!(color, "DEPSC");
    }
}
