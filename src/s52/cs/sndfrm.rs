//! SNDFRM02 - Sounding Conditional Symbology Procedure
//!
//! Formats depth soundings with S-52 compliant coloring and formatting.
//! Based on OpenCPN s52cnsy.cpp lines 2661-2932.
//!
//! Key features:
//! - Safety depth coloring (SNDG2 black for shallow, SNDG1 gray for deep)
//! - Decimal formatting with subscript for fractional meters
//! - QUASOU/STATUS/QUAPOS uncertainty indicators
//! - TECSOU=6 swept depth handling
//! - Drying height (negative depth) underscore prefix

use crate::senc::{AttributeValue, Feature};

// Import MarinerSettings - DepthUnit is accessed via settings field
use super::super::{DepthUnit, MarinerSettings};

/// Sounding rendering result from SNDFRM02
#[derive(Debug, Clone)]
pub struct SoundingRenderInfo {
    /// Integer part of depth for display
    pub whole_part: u32,
    /// Decimal digit (0-9) if applicable, None for depths >= 31m
    pub decimal_digit: Option<u8>,
    /// Number of whole digits to display (1-5)
    pub digit_count: u8,
    /// Whether this is a shallow/danger sounding (depth <= safety_depth)
    /// Uses SNDG2 color (black) when true, SNDG1 (gray) when false
    pub is_shallow: bool,
    /// Whether this is a drying height (negative depth, needs underscore)
    pub is_drying: bool,
    /// Whether to show uncertainty indicator (question mark)
    /// Set when QUASOU 3,4,5,8,9 or STATUS 18 or QUAPOS 2-9
    pub show_uncertainty: bool,
    /// Whether this is a swept depth (TECSOU=6, found by diver)
    pub is_swept: bool,
}

impl SoundingRenderInfo {
    /// Pack flags into a u32 for GPU transfer
    /// Bit 0: is_shallow
    /// Bit 1: is_drying
    /// Bit 2: show_uncertainty
    /// Bit 3: is_swept
    /// Bit 4: has_decimal (decimal_digit.is_some())
    /// Bits 5-7: digit_count (1-5)
    /// Bits 8-11: decimal_digit value (0-9)
    /// Bits 12-28: whole_part value (0-99999)
    pub fn to_flags(&self) -> u32 {
        let mut flags = 0u32;
        if self.is_shallow {
            flags |= 1 << 0;
        }
        if self.is_drying {
            flags |= 1 << 1;
        }
        if self.show_uncertainty {
            flags |= 1 << 2;
        }
        if self.is_swept {
            flags |= 1 << 3;
        }
        if let Some(decimal) = self.decimal_digit {
            flags |= 1 << 4; // has_decimal flag
            flags |= (decimal as u32 & 0xF) << 8; // decimal value in bits 8-11
        }
        flags |= ((self.digit_count as u32) & 0x7) << 5; // digit count in bits 5-7
        flags |= (self.whole_part & 0x1_FFFF) << 12; // whole part in bits 12-28
        flags
    }

    /// Get the S-52 color token for this sounding
    pub fn color_token(&self) -> &'static str {
        if self.is_shallow {
            "SNDG2" // Black for shallow/danger soundings
        } else {
            "SNDG1" // Gray for safe/deep soundings
        }
    }
}

/// SNDFRM02 - Format a sounding for display per S-52 specification.
///
/// # Arguments
/// * `depth_meters` - Depth value in meters (negative for drying heights)
/// * `feature` - The SOUNDG feature (used for QUASOU, TECSOU, STATUS attributes)
/// * `settings` - Mariner settings (for safety_depth threshold)
///
/// # Returns
/// SoundingRenderInfo with formatting and color information
///
/// # OpenCPN Reference
/// s52cnsy.cpp lines 2661-2932
pub fn sndfrm02(
    depth_meters: f64,
    feature: &Feature,
    settings: &MarinerSettings,
) -> SoundingRenderInfo {
    // Convert depth based on unit preference (OpenCPN s52cnsy.cpp:2675-2700)
    let (depth_value, safety_depth) =
        convert_depth_units(depth_meters, settings.safety_depth as f64, settings);

    // Determine if shallow (danger) or deep (safe)
    // OpenCPN s52cnsy.cpp:2712-2715
    let is_shallow = depth_value <= safety_depth;

    // Drying heights are always considered shallow/dangerous
    let is_drying = depth_value < 0.0;

    // Check TECSOU for swept depth (code 6 = "found by diver")
    // OpenCPN s52cnsy.cpp:2719-2725
    let is_swept = check_tecsou_swept(feature);

    // Check QUASOU/STATUS/QUAPOS for uncertainty
    // OpenCPN s52cnsy.cpp:2727-2744
    let show_uncertainty = check_uncertainty(feature);

    // Format the depth value
    // OpenCPN s52cnsy.cpp:2747-2920
    let abs_depth = depth_value.abs();
    let leading_digit = abs_depth.floor() as u32;

    let (whole_part, decimal_digit, digit_count) =
        format_depth_parts(depth_value, leading_digit, settings);

    SoundingRenderInfo {
        whole_part,
        decimal_digit,
        digit_count,
        is_shallow: is_shallow || is_drying, // Drying always shallow
        is_drying,
        show_uncertainty,
        is_swept,
    }
}

/// Format depth into whole and decimal parts per S-52 rules.
///
/// Formatting rules (from OpenCPN s52cnsy.cpp:2747-2920):
/// - < 10: X + optional subscript (feet: rounded, no subscript)
/// - < 31: XX + optional subscript (feet: rounded, no subscript)
/// - < 100: XX
/// - < 1000: XXX
/// - < 10000: XXXX
/// - >= 10000: XXXXX
fn format_depth_parts(
    depth_value_in: f64,
    leading_digit_in: u32,
    settings: &MarinerSettings,
) -> (u32, Option<u8>, u8) {
    let mut depth_value = depth_value_in;
    let mut leading_digit = leading_digit_in;

    let abs_depth = depth_value.abs();

    if abs_depth < 10.0 {
        // If showing as "feet", round to one digit only
        if settings.depth_unit == DepthUnit::Feet && depth_value > 0.0 {
            depth_value = depth_value.round();
            leading_digit = depth_value.abs().floor() as u32;
        }

        let fraction = ((depth_value.abs() - leading_digit as f64) * 10.0).abs() as u8;
        let decimal_digit = if fraction > 0 { Some(fraction) } else { None };
        return (leading_digit, decimal_digit, 1);
    }

    if abs_depth < 31.0 {
        let mut b_2digit = false;
        if settings.depth_unit == DepthUnit::Feet && depth_value > 0.0 {
            depth_value = depth_value.round();
            leading_digit = depth_value.abs().floor() as u32;
            b_2digit = true;
        }

        let fraction = (depth_value.abs() - leading_digit as f64).abs();
        let decimal_digit = if !b_2digit && fraction != 0.0 {
            let frac = (fraction * 10.0) as u8;
            if frac > 0 {
                Some(frac)
            } else {
                None
            }
        } else {
            None
        };

        return (leading_digit, decimal_digit, 2);
    }

    if abs_depth < 100.0 {
        return (leading_digit, None, 2);
    }

    if abs_depth < 1000.0 {
        return (leading_digit, None, 3);
    }

    if abs_depth < 10000.0 {
        return (leading_digit, None, 4);
    }

    (leading_digit, None, 5)
}

/// Check TECSOU attribute for swept depth indication.
/// TECSOU=6 means "found by diver" - indicates swept depth.
fn check_tecsou_swept(feature: &Feature) -> bool {
    // Try as string (comma-separated list)
    if let Some(AttributeValue::String(tecsou)) = feature.attributes.get("TECSOU") {
        return tecsou.split(',').any(|v| v.trim() == "6");
    }
    // Try as integer
    if let Some(AttributeValue::Integer(tecsou)) = feature.attributes.get("TECSOU") {
        return *tecsou == 6;
    }
    false
}

/// Check for uncertainty indicators from QUASOU, STATUS, and QUAPOS attributes.
///
/// OpenCPN s52cnsy.cpp:2727-2744:
/// - QUASOU 3 = depth known (from depth area)
/// - QUASOU 4 = depth unknown
/// - QUASOU 5 = doubtful
/// - QUASOU 8 = reported (not surveyed)
/// - QUASOU 9 = not regularly maintained
/// - STATUS 18 = doubtful
/// - QUAPOS 2-9 = low accuracy position
fn check_uncertainty(feature: &Feature) -> bool {
    // Check QUASOU
    if let Some(AttributeValue::String(quasou)) = feature.attributes.get("QUASOU") {
        let uncertain = ["3", "4", "5", "8", "9"];
        if quasou.split(',').any(|v| uncertain.contains(&v.trim())) {
            return true;
        }
    }
    if let Some(AttributeValue::Integer(quasou)) = feature.attributes.get("QUASOU") {
        if matches!(*quasou, 3 | 4 | 5 | 8 | 9) {
            return true;
        }
    }

    // Check STATUS
    if let Some(AttributeValue::Integer(status)) = feature.attributes.get("STATUS") {
        if *status == 18 {
            return true;
        }
    }

    // Check QUAPOS (position accuracy)
    if let Some(AttributeValue::Integer(quapos)) = feature.attributes.get("QUAPOS") {
        if *quapos >= 2 && *quapos < 10 {
            return true;
        }
    }

    false
}

/// Get S-52 color token for sounding based on depth vs safety depth.
///
/// - SNDG2 (black/dark): shallow/danger soundings (depth <= safety_depth)
/// - SNDG1 (gray): safe/deep soundings (depth > safety_depth)
pub fn sounding_color(is_shallow: bool) -> &'static str {
    if is_shallow {
        "SNDG2"
    } else {
        "SNDG1"
    }
}

fn convert_depth_units(
    depth_value_in: f64,
    safety_depth_in: f64,
    settings: &MarinerSettings,
) -> (f64, f64) {
    let mut depth_value = depth_value_in;
    let mut safety_depth = safety_depth_in;

    // If the sounding value from the ENC is bogus, clamp like OpenCPN.
    if depth_value_in > 40000.0 {
        depth_value = 99999.0;
    }
    if depth_value_in < -1000.0 {
        depth_value = 0.0;
    }

    match settings.depth_unit {
        DepthUnit::Feet => {
            let factor = 3.0 * 39.37 / 36.0; // OpenCPN conversion
            depth_value *= factor;
            safety_depth *= factor;
        }
        DepthUnit::Fathoms => {
            let factor = (3.0 * 39.37 / 36.0) / 6.0; // feet / 6
            depth_value *= factor;
            safety_depth *= factor;
        }
        DepthUnit::Meters => {}
    }

    // OpenCPN rounding bias
    depth_value += if depth_value > 0.0 { 0.01 } else { -0.01 };

    (depth_value, safety_depth)
}

/// Get RGB color for sounding (Day Bright palette).
///
/// From OpenCPN chartsymbols.xml:
/// - SNDG1: RGB(125, 137, 140) - gray for safe/deep
/// - SNDG2: RGB(7, 7, 7) - black for shallow/danger
pub fn sounding_color_rgb(is_shallow: bool) -> [f32; 4] {
    if is_shallow {
        // SNDG2 - black for danger
        [0.027, 0.027, 0.027, 1.0]
    } else {
        // SNDG1 - gray for safe
        [0.490, 0.537, 0.549, 1.0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::s52::DepthUnit;
    use crate::senc::{FeatureType, ObjectClass};
    use std::collections::HashMap;

    fn make_sounding() -> Feature {
        Feature {
            type_code: 129, // SOUNDG
            object_class: ObjectClass::Sounding,
            feature_type: FeatureType::Point,
            attributes: HashMap::new(),
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        }
    }

    fn make_sounding_with_attr(name: &str, value: AttributeValue) -> Feature {
        let mut feature = make_sounding();
        feature.attributes.insert(name.to_string(), value);
        feature
    }

    #[test]
    fn test_shallow_depth_is_black() {
        let feature = make_sounding();
        let settings = MarinerSettings {
            safety_depth: 10.0,
            ..Default::default()
        };
        let info = sndfrm02(5.0, &feature, &settings);
        assert!(info.is_shallow); // 5m < 10m safety
        assert_eq!(info.color_token(), "SNDG2");
    }

    #[test]
    fn test_deep_depth_is_gray() {
        let feature = make_sounding();
        let settings = MarinerSettings {
            safety_depth: 10.0,
            ..Default::default()
        };
        let info = sndfrm02(15.0, &feature, &settings);
        assert!(!info.is_shallow); // 15m > 10m safety
        assert_eq!(info.color_token(), "SNDG1");
    }

    #[test]
    fn test_format_single_digit_with_decimal() {
        let feature = make_sounding();
        let settings = MarinerSettings::default();
        let info = sndfrm02(8.5, &feature, &settings);
        assert_eq!(info.whole_part, 8);
        assert_eq!(info.decimal_digit, Some(5));
        assert_eq!(info.digit_count, 1);
    }

    #[test]
    fn test_format_two_digits_with_decimal() {
        let feature = make_sounding();
        let settings = MarinerSettings::default();
        let info = sndfrm02(15.3, &feature, &settings);
        assert_eq!(info.whole_part, 15);
        assert_eq!(info.decimal_digit, Some(3));
        assert_eq!(info.digit_count, 2);
    }

    #[test]
    fn test_format_no_decimal_above_31() {
        let feature = make_sounding();
        let settings = MarinerSettings::default();
        let info = sndfrm02(45.7, &feature, &settings);
        assert_eq!(info.whole_part, 45); // Truncated after rounding bias
        assert_eq!(info.decimal_digit, None);
        assert_eq!(info.digit_count, 2);
    }

    #[test]
    fn test_format_large_depth() {
        let feature = make_sounding();
        let settings = MarinerSettings::default();
        let info = sndfrm02(156.0, &feature, &settings);
        assert_eq!(info.whole_part, 156);
        assert_eq!(info.decimal_digit, None);
        assert_eq!(info.digit_count, 3);
    }

    #[test]
    fn test_drying_height() {
        let feature = make_sounding();
        let settings = MarinerSettings::default();
        let info = sndfrm02(-1.2, &feature, &settings);
        assert!(info.is_drying);
        assert!(info.is_shallow); // Drying always shallow/dangerous
        assert_eq!(info.whole_part, 1);
        assert_eq!(info.decimal_digit, Some(2));
        assert_eq!(info.digit_count, 1);
    }

    #[test]
    fn test_quasou_uncertainty_int() {
        let feature = make_sounding_with_attr("QUASOU", AttributeValue::Integer(4));
        let settings = MarinerSettings::default();
        let info = sndfrm02(10.0, &feature, &settings);
        assert!(info.show_uncertainty);
    }

    #[test]
    fn test_quasou_uncertainty_string() {
        let feature = make_sounding_with_attr("QUASOU", AttributeValue::String("4".to_string()));
        let settings = MarinerSettings::default();
        let info = sndfrm02(10.0, &feature, &settings);
        assert!(info.show_uncertainty);
    }

    #[test]
    fn test_status_uncertainty() {
        let feature = make_sounding_with_attr("STATUS", AttributeValue::Integer(18));
        let settings = MarinerSettings::default();
        let info = sndfrm02(10.0, &feature, &settings);
        assert!(info.show_uncertainty);
    }

    #[test]
    fn test_quapos_uncertainty() {
        let feature = make_sounding_with_attr("QUAPOS", AttributeValue::Integer(5));
        let settings = MarinerSettings::default();
        let info = sndfrm02(10.0, &feature, &settings);
        assert!(info.show_uncertainty);
    }

    #[test]
    fn test_tecsou_swept() {
        let feature = make_sounding_with_attr("TECSOU", AttributeValue::Integer(6));
        let settings = MarinerSettings::default();
        let info = sndfrm02(10.0, &feature, &settings);
        assert!(info.is_swept);
    }

    #[test]
    fn test_tecsou_not_swept() {
        let feature = make_sounding_with_attr("TECSOU", AttributeValue::Integer(3));
        let settings = MarinerSettings::default();
        let info = sndfrm02(10.0, &feature, &settings);
        assert!(!info.is_swept);
    }

    #[test]
    fn test_flags_packing() {
        let info = SoundingRenderInfo {
            whole_part: 8,
            decimal_digit: Some(5),
            digit_count: 1,
            is_shallow: true,
            is_drying: false,
            show_uncertainty: true,
            is_swept: false,
        };
        let flags = info.to_flags();
        assert_eq!(flags & 1, 1); // is_shallow
        assert_eq!((flags >> 1) & 1, 0); // is_drying
        assert_eq!((flags >> 2) & 1, 1); // show_uncertainty
        assert_eq!((flags >> 3) & 1, 0); // is_swept
        assert_eq!((flags >> 4) & 1, 1); // has_decimal
        assert_eq!((flags >> 5) & 0x7, 1); // digit count
        assert_eq!((flags >> 8) & 0xF, 5); // decimal value
        assert_eq!((flags >> 12) & 0x1_FFFF, 8); // whole part
    }

    #[test]
    fn test_sounding_colors() {
        assert_eq!(sounding_color(true), "SNDG2"); // Shallow = black
        assert_eq!(sounding_color(false), "SNDG1"); // Deep = gray

        let shallow_rgb = sounding_color_rgb(true);
        assert!(shallow_rgb[0] < 0.1); // Black

        let deep_rgb = sounding_color_rgb(false);
        assert!(deep_rgb[0] > 0.4); // Gray
    }

    #[test]
    fn test_exact_safety_boundary() {
        let feature = make_sounding();
        let settings = MarinerSettings {
            safety_depth: 10.0,
            ..Default::default()
        };

        // OpenCPN adds a +0.01 rounding bias, so exact safety depth becomes deep.
        let info = sndfrm02(10.0, &feature, &settings);
        assert!(!info.is_shallow);

        // Just above safety depth should be deep (>)
        let info = sndfrm02(10.1, &feature, &settings);
        assert!(!info.is_shallow);
    }

    #[test]
    fn test_feet_rounding_rules() {
        let feature = make_sounding();
        let settings = MarinerSettings {
            depth_unit: DepthUnit::Feet,
            ..Default::default()
        };

        // 5.4m -> ~17.7ft, rounded to 18, no decimal in feet < 31ft
        let info = sndfrm02(5.4, &feature, &settings);
        assert_eq!(info.whole_part, 18);
        assert_eq!(info.decimal_digit, None);
        assert_eq!(info.digit_count, 2);
    }
}
