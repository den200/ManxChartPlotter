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

    /// Rebuild a [`SoundingRenderInfo`] from its packed flags.
    ///
    /// The renderer decides which soundings survive decluttering after the
    /// builder has already packed them for the GPU, so the symbol composition
    /// has to be able to work from the flags alone.
    pub fn from_flags(flags: u32) -> Self {
        Self {
            whole_part: (flags >> 12) & 0x1_FFFF,
            decimal_digit: ((flags & (1 << 4)) != 0).then(|| ((flags >> 8) & 0xF) as u8),
            digit_count: ((flags >> 5) & 0x7) as u8,
            is_shallow: flags & 1 != 0,
            is_drying: flags & (1 << 1) != 0,
            show_uncertainty: flags & (1 << 2) != 0,
            is_swept: flags & (1 << 3) != 0,
        }
    }

    /// The `SY()` symbols S-52 composes this sounding from.
    ///
    /// Soundings are not text. The presentation library ships a set of digit
    /// symbols whose *pivots* encode the placement — position 2 sits at +12px,
    /// position 1 at +5, position 0 at -2, and the fraction digit is dropped
    /// below the baseline — and SNDFRM02 picks one symbol per digit position.
    /// That is what makes a sounding readable at any orientation and what makes
    /// "9₂" render as the chart convention rather than as "9.2".
    ///
    /// The branch structure is s52cnsy.cpp's, which is the procedure's: the
    /// choice of *positions* depends on the magnitude, so 25.0 draws as
    /// positions 1 and 0 while 25.5 draws as 2, 1 and the fraction.
    pub fn symbols(&self) -> Vec<String> {
        // "SOUNDS" is the shallow (danger) set, "SOUNDG" the safe one.
        let p = if self.is_shallow { "SOUNDS" } else { "SOUNDG" };
        let mut out = Vec::with_capacity(4);
        if self.is_swept {
            out.push(format!("{p}B1"));
        }
        if self.show_uncertainty {
            out.push(format!("{p}C2"));
        }

        let whole = self.whole_part;
        let fraction = self.decimal_digit.filter(|d| *d > 0);
        let drying = self.is_drying;

        // Continuation A: a single leading digit, optional fraction.
        if whole < 10 {
            out.push(format!("{p}1{whole}"));
            if let Some(f) = fraction {
                out.push(format!("{p}5{f}"));
            }
            if drying {
                out.push(format!("{p}A1"));
            }
            return out;
        }

        // Two digits with a fraction keep the fraction's position free by
        // shifting the whole part left one place.
        if whole < 31 {
            if let Some(f) = fraction {
                out.push(format!("{p}2{}", whole / 10));
                out.push(format!("{p}1{}", whole % 10));
                out.push(format!("{p}5{f}"));
                if drying {
                    out.push(format!("{p}A1"));
                }
                return out;
            }
        }

        // Continuation B and beyond: whole digits only.
        let digits: Vec<u32> = {
            let mut d = Vec::new();
            let mut n = whole;
            while n > 0 {
                d.push(n % 10);
                n /= 10;
            }
            d.reverse();
            d
        };
        // Position codes per digit count, from s52cnsy.cpp. A drying two-digit
        // value shifts left to leave room for the A1 bar.
        let positions: &[u8] = match (digits.len(), drying) {
            (2, true) => &[2, 1],
            (2, false) => &[1, 0],
            (3, _) => &[2, 1, 0],
            (4, _) => &[2, 1, 0, 4],
            _ => &[3, 2, 1, 0, 4],
        };
        for (digit, pos) in digits.iter().zip(positions.iter()) {
            out.push(format!("{p}{pos}{digit}"));
        }
        if drying {
            out.push(format!("{p}A1"));
        }
        out
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
    // QUASOU and STATUS are both list attributes: on a real chart they
    // arrive as text ("1,18"), and in hand-made features as a single integer.
    if super::list_values(feature, "QUASOU")
        .iter()
        .any(|q| matches!(q, 3 | 4 | 5 | 8 | 9))
    {
        return true;
    }
    // STATUS 18 is "existence doubtful".
    if super::list_values(feature, "STATUS").contains(&18) {
        return true;
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

    fn make_sounding() -> Feature {
        Feature {
            type_code: 129, // SOUNDG
            object_class: ObjectClass::Sounding,
            feature_type: FeatureType::Point,
            attributes: crate::senc::Attributes::new(),
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        }
    }

    fn make_sounding_with_attr(name: &str, value: AttributeValue) -> Feature {
        let mut feature = make_sounding();
        feature.attributes.insert(name, value);
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

        // 9.99 m lands exactly on the 10 m safety depth once the bias is
        // added, and the test is `<=`: it is a shallow sounding. A strict `<`
        // would print it grey.
        let info = sndfrm02(9.99, &feature, &settings);
        assert!(info.is_shallow, "depth equal to the safety depth is shallow");
        assert_eq!(info.color_token(), "SNDG2");
    }

    /// The safety depth is entered in metres but compared in the display
    /// unit, so it has to be converted with the sounding. Comparing 16.4 ft
    /// against an unconverted "10" would print a 5 m sounding as safe water
    /// for a vessel that needs 10 m.
    #[test]
    fn safety_depth_is_converted_with_the_sounding_in_feet_and_fathoms() {
        let feature = make_sounding();
        for unit in [DepthUnit::Feet, DepthUnit::Fathoms] {
            let settings = MarinerSettings {
                safety_depth: 10.0,
                depth_unit: unit,
                ..Default::default()
            };
            assert!(
                sndfrm02(5.0, &feature, &settings).is_shallow,
                "5 m is shallower than a 10 m safety depth in {unit:?}"
            );
            assert!(
                !sndfrm02(15.0, &feature, &settings).is_shallow,
                "15 m is deeper than a 10 m safety depth in {unit:?}"
            );
        }
    }

    #[test]
    fn fathoms_keep_the_fraction_digit() {
        // 10 m is 5.47 fathoms (OpenCPN's factor). Unlike feet, fathoms are
        // not rounded to whole units below 31.
        let settings = MarinerSettings {
            depth_unit: DepthUnit::Fathoms,
            ..Default::default()
        };
        let info = sndfrm02(10.0, &make_sounding(), &settings);
        assert_eq!(info.whole_part, 5);
        assert_eq!(info.decimal_digit, Some(4));
        assert_eq!(info.digit_count, 1);
    }

    #[test]
    fn rounding_bias_keeps_binary_fractions_from_losing_a_tenth() {
        // 2.3 is 2.2999... in binary; truncating its fraction without
        // OpenCPN's +0.01 bias would print 2 with subscript 2.
        let info = sndfrm02(2.3, &make_sounding(), &MarinerSettings::default());
        assert_eq!(info.whole_part, 2);
        assert_eq!(info.decimal_digit, Some(3));

        // A whole metre has no fraction digit at all, bias or not.
        let info = sndfrm02(3.0, &make_sounding(), &MarinerSettings::default());
        assert_eq!(info.whole_part, 3);
        assert_eq!(info.decimal_digit, None);
    }

    #[test]
    fn reliable_soundings_carry_no_uncertainty_mark() {
        let settings = MarinerSettings::default();
        // QUASOU 1 (depth known), 6 (least depth known), QUAPOS 1 (surveyed)
        // and 10/11 (precisely known / calculated) are all reliable, and so is
        // a STATUS other than 18.
        for (attr, value) in [
            ("QUASOU", AttributeValue::Integer(1)),
            ("QUASOU", AttributeValue::String("1,6".to_string())),
            ("QUAPOS", AttributeValue::Integer(1)),
            ("QUAPOS", AttributeValue::Integer(10)),
            ("QUAPOS", AttributeValue::Integer(11)),
            ("STATUS", AttributeValue::Integer(1)),
        ] {
            let feature = make_sounding_with_attr(attr, value.clone());
            assert!(
                !sndfrm02(10.0, &feature, &settings).show_uncertainty,
                "{attr}={value:?} is not an uncertain sounding"
            );
        }
        assert!(!sndfrm02(10.0, &make_sounding(), &settings).show_uncertainty);
    }

    #[test]
    fn uncertainty_is_found_anywhere_in_a_quasou_list() {
        // QUASOU is a list attribute; the SENC stores it as a string.
        let feature =
            make_sounding_with_attr("QUASOU", AttributeValue::String("1,8".to_string()));
        assert!(sndfrm02(10.0, &feature, &MarinerSettings::default()).show_uncertainty);
    }

    /// STATUS is an S-57 *list* attribute, so the SENC delivers it as a
    /// string like QUASOU — a doubtful sounding (STATUS 18) on a real chart
    /// arrives as `String("18")`, which `check_uncertainty` only reads as an
    /// integer.
    #[test]
    fn status_doubtful_is_recognised_when_encoded_as_a_list() {
        let feature = make_sounding_with_attr("STATUS", AttributeValue::String("18".to_string()));
        assert!(sndfrm02(10.0, &feature, &MarinerSettings::default()).show_uncertainty);
    }

    #[test]
    fn tecsou_six_anywhere_in_the_list_marks_a_swept_depth() {
        let feature = make_sounding_with_attr("TECSOU", AttributeValue::String("1,6".to_string()));
        assert!(sndfrm02(10.0, &feature, &MarinerSettings::default()).is_swept);
        let feature = make_sounding_with_attr("TECSOU", AttributeValue::String("1,3".to_string()));
        assert!(!sndfrm02(10.0, &feature, &MarinerSettings::default()).is_swept);
    }

    #[test]
    fn bogus_depths_are_clamped_like_opencpn() {
        let settings = MarinerSettings::default();
        // Above 40 000 m the value is a placeholder: shown as 99999.
        let info = sndfrm02(50_000.0, &make_sounding(), &settings);
        assert_eq!(info.whole_part, 99_999);
        assert_eq!(info.digit_count, 5);
        assert!(!info.is_drying);
        // Below -1000 m it is nonsense and becomes 0. (The -0.01 bias then
        // makes it a "drying" 0, exactly as in s52cnsy.cpp.)
        let info = sndfrm02(-2_000.0, &make_sounding(), &settings);
        assert_eq!(info.whole_part, 0);
        assert_eq!(info.digit_count, 1);
    }

    #[test]
    fn flags_round_trip_through_the_gpu_packing() {
        // The renderer rebuilds the symbols from the packed flags alone, so
        // every field must survive, including the widest whole part.
        for info in [
            SoundingRenderInfo {
                whole_part: 99_999,
                decimal_digit: None,
                digit_count: 5,
                is_shallow: false,
                is_drying: false,
                show_uncertainty: false,
                is_swept: true,
            },
            SoundingRenderInfo {
                whole_part: 12,
                decimal_digit: Some(9),
                digit_count: 2,
                is_shallow: true,
                is_drying: true,
                show_uncertainty: true,
                is_swept: false,
            },
            SoundingRenderInfo {
                whole_part: 0,
                decimal_digit: Some(0),
                digit_count: 1,
                is_shallow: true,
                is_drying: false,
                show_uncertainty: false,
                is_swept: false,
            },
        ] {
            let back = SoundingRenderInfo::from_flags(info.to_flags());
            assert_eq!(back.whole_part, info.whole_part);
            assert_eq!(back.decimal_digit, info.decimal_digit);
            assert_eq!(back.digit_count, info.digit_count);
            assert_eq!(back.is_shallow, info.is_shallow);
            assert_eq!(back.is_drying, info.is_drying);
            assert_eq!(back.show_uncertainty, info.show_uncertainty);
            assert_eq!(back.is_swept, info.is_swept);
            assert_eq!(back.symbols(), info.symbols());
        }
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

#[cfg(test)]
mod symbol_tests {
    use super::*;

    fn info(whole: u32, decimal: Option<u8>, shallow: bool) -> SoundingRenderInfo {
        SoundingRenderInfo {
            whole_part: whole,
            decimal_digit: decimal,
            digit_count: if whole >= 10 { 2 } else { 1 },
            is_shallow: shallow,
            is_drying: false,
            show_uncertainty: false,
            is_swept: false,
        }
    }

    /// The digit *positions* depend on the magnitude — that is the whole point
    /// of the symbol set, and getting it wrong stacks digits on top of each
    /// other rather than merely misplacing them.
    #[test]
    fn sndfrm02_composes_the_standard_symbol_positions() {
        // One digit, with and without a fraction.
        assert_eq!(info(2, Some(5), true).symbols(), ["SOUNDS12", "SOUNDS55"]);
        assert_eq!(info(9, None, false).symbols(), ["SOUNDG19"]);
        // Two digits with a fraction shift left to free the fraction slot.
        assert_eq!(
            info(25, Some(5), false).symbols(),
            ["SOUNDG22", "SOUNDG15", "SOUNDG55"]
        );
        // The same value without a fraction uses positions 1 and 0 instead.
        assert_eq!(info(25, None, false).symbols(), ["SOUNDG12", "SOUNDG05"]);
        // Three and four digits.
        assert_eq!(
            info(125, None, false).symbols(),
            ["SOUNDG21", "SOUNDG12", "SOUNDG05"]
        );
        assert_eq!(
            info(1250, None, false).symbols(),
            ["SOUNDG21", "SOUNDG12", "SOUNDG05", "SOUNDG40"]
        );
    }

    /// Five-digit depths use positions 3, 2, 1, 0 and 4 (s52cnsy.cpp
    /// "Continuation C").
    #[test]
    fn sndfrm02_composes_five_digit_depths() {
        let mut i = info(12345, None, false);
        i.digit_count = 5;
        assert_eq!(
            i.symbols(),
            ["SOUNDG31", "SOUNDG22", "SOUNDG13", "SOUNDG04", "SOUNDG45"]
        );
    }

    /// A drying two-digit height moves left one place to make room for the
    /// A1 bar — positions 2 and 1, not 1 and 0 — end to end from the depth.
    #[test]
    fn two_digit_drying_height_shifts_left_for_the_bar() {
        let feature = crate::senc::Feature {
            type_code: 129,
            object_class: crate::senc::ObjectClass::Sounding,
            feature_type: crate::senc::FeatureType::Point,
            attributes: crate::senc::Attributes::new(),
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        };
        let s = sndfrm02(-12.0, &feature, &MarinerSettings::default());
        assert_eq!(s.symbols(), ["SOUNDS21", "SOUNDS12", "SOUNDSA1"]);
        // The same depth below water is positions 1 and 0, safe set.
        let s = sndfrm02(12.0, &feature, &MarinerSettings::default());
        assert_eq!(s.symbols(), ["SOUNDG11", "SOUNDG02"]);
    }

    #[test]
    fn sndfrm02_marks_swept_uncertain_and_drying() {
        let mut i = info(3, None, true);
        i.is_swept = true;
        i.show_uncertainty = true;
        i.is_drying = true;
        assert_eq!(
            i.symbols(),
            ["SOUNDSB1", "SOUNDSC2", "SOUNDS13", "SOUNDSA1"]
        );
    }

    /// The shallow set is chosen against the mariner's safety *depth*, which is
    /// what makes a sounding print black rather than grey.
    #[test]
    fn safety_depth_selects_the_shallow_symbol_set() {
        let feature = crate::senc::Feature {
            type_code: 129,
            object_class: crate::senc::ObjectClass::Other,
            feature_type: crate::senc::FeatureType::Point,
            attributes: crate::senc::Attributes::new(),
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        };
        let mut settings = MarinerSettings::default();
        settings.safety_depth = 3.0;
        let shallow = sndfrm02(2.5, &feature, &settings);
        assert!(shallow.is_shallow, "2.5m is shallower than a 3m safety depth");
        assert!(shallow.symbols()[0].starts_with("SOUNDS"));

        let deep = sndfrm02(7.5, &feature, &settings);
        assert!(!deep.is_shallow);
        assert!(deep.symbols()[0].starts_with("SOUNDG"));
    }
}
