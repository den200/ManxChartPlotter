//! LIGHTS05/06 - Light Conditional Symbology Procedure
//!
//! Matches OpenCPN symbol selection for flare vs all-round lights.
//! Sector arcs and light-sector text are not rendered here.

use crate::s52::MarinerSettings;
use crate::senc::Feature;

/// Light symbol types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LightSymbol {
    RedFlare,
    GreenFlare,
    YellowFlare,
    DefaultFlare,
    RedAllRound,
    GreenAllRound,
    YellowAllRound,
    DefaultAllRound,
    Spotlight,
    Strobe,
    Unknown,
}

impl LightSymbol {
    pub fn symbol_name(&self) -> &'static str {
        match self {
            Self::RedFlare => "LIGHTS11",
            Self::GreenFlare => "LIGHTS12",
            Self::YellowFlare => "LIGHTS13",
            Self::DefaultFlare => "LITDEF11",
            Self::RedAllRound => "LIGHTS93",
            Self::GreenAllRound => "LIGHTS92",
            Self::YellowAllRound => "LIGHTS91",
            Self::DefaultAllRound => "LIGHTS91",
            Self::Spotlight => "LIGHTS81",
            Self::Strobe => "LIGHTS82",
            Self::Unknown => "QUESMRK1",
        }
    }
}

/// Light sector rendering parameters (for CA arcs + leg lines).
pub struct LightSectorInfo {
    pub sectr1: f64,
    pub sectr2: f64,
    pub arc_radius_mm: f64,
    pub sector_radius_mm: f64,
    pub arc_color_token: &'static str,
    pub faint: bool,
}

/// Parse the COLOUR attribute (comma-separated list of color codes)
fn parse_color_list(colour_attr: Option<&str>) -> Vec<u8> {
    match colour_attr {
        Some(s) => s
            .split(',')
            .filter_map(|c| c.trim().parse::<u8>().ok())
            .collect(),
        None => vec![],
    }
}

fn parse_litvis(litvis_attr: Option<&str>) -> Vec<u8> {
    match litvis_attr {
        Some(s) => s
            .split(',')
            .filter_map(|c| c.trim().parse::<u8>().ok())
            .collect(),
        None => vec![],
    }
}

fn select_flare_symbol(colors: &[u8]) -> LightSymbol {
    if colors.is_empty() {
        return LightSymbol::DefaultFlare;
    }

    if colors.len() == 1 {
        match colors[0] {
            3 => LightSymbol::RedFlare,
            4 => LightSymbol::GreenFlare,
            1 | 6 | 9 | 11 => LightSymbol::YellowFlare,
            _ => LightSymbol::DefaultFlare,
        }
    } else if colors.len() == 2 {
        let has_white = colors.contains(&1);
        let has_red = colors.contains(&3);
        let has_green = colors.contains(&4);

        if has_white && has_red {
            LightSymbol::RedFlare
        } else if has_white && has_green {
            LightSymbol::GreenFlare
        } else {
            LightSymbol::DefaultFlare
        }
    } else {
        LightSymbol::DefaultFlare
    }
}

fn select_all_round_symbol(colors: &[u8]) -> LightSymbol {
    if colors.is_empty() {
        return LightSymbol::DefaultAllRound;
    }

    if colors.len() == 1 {
        match colors[0] {
            3 => LightSymbol::RedAllRound,
            4 => LightSymbol::GreenAllRound,
            1 | 6 | 9 | 11 => LightSymbol::YellowAllRound,
            _ => LightSymbol::DefaultAllRound,
        }
    } else if colors.len() == 2 {
        let has_white = colors.contains(&1);
        let has_red = colors.contains(&3);
        let has_green = colors.contains(&4);

        if has_white && has_red {
            LightSymbol::RedAllRound
        } else if has_white && has_green {
            LightSymbol::GreenAllRound
        } else {
            LightSymbol::DefaultAllRound
        }
    } else {
        LightSymbol::DefaultAllRound
    }
}

fn select_sector_color_token(colors: &[u8]) -> &'static str {
    if colors.is_empty() {
        return "CHMGD";
    }

    if colors.len() == 1 {
        match colors[0] {
            3 => "LITRD",
            4 => "LITGN",
            1 | 6 | 9 | 11 => "LITYW",
            _ => "CHMGD",
        }
    } else if colors.len() == 2 {
        let has_white = colors.contains(&1);
        let has_red = colors.contains(&3);
        let has_green = colors.contains(&4);

        if has_white && has_red {
            "LITRD"
        } else if has_white && has_green {
            "LITGN"
        } else {
            "CHMGD"
        }
    } else {
        "CHMGD"
    }
}

/// Parse CATLIT attribute to check for special light types
/// CATLIT codes: 8=flood, 9=spotlight, 11=strobe
fn parse_catlit(catlit_attr: Option<&str>) -> Vec<u8> {
    match catlit_attr {
        Some(s) => s
            .split(',')
            .filter_map(|c| c.trim().parse::<u8>().ok())
            .collect(),
        None => vec![],
    }
}

/// LIGHTS05/06 - Light symbol selection
pub fn lights06(feature: &Feature, _settings: &MarinerSettings) -> LightSymbol {
    let catlit = parse_catlit(feature.attribute_str("CATLIT"));
    if catlit.contains(&8) || catlit.contains(&11) {
        return LightSymbol::Strobe;
    }
    if catlit.contains(&9) {
        return LightSymbol::Spotlight;
    }

    let mut colours = parse_color_list(feature.attribute_str("COLOUR"));
    if colours.is_empty() {
        colours.push(12); // default magenta in OpenCPN
    }

    let valnmr = feature.attribute_float("VALNMR").unwrap_or(9.0);
    let sectr1 = feature.attribute_float("SECTR1");
    let sectr2 = feature.attribute_float("SECTR2");

    // Any light with sector attributes uses AllRound symbol (even if sweep is invalid)
    let has_sector_attrs = sectr1.is_some() && sectr2.is_some();

    if has_sector_attrs || valnmr >= 10.0 {
        select_all_round_symbol(&colours)
    } else {
        select_flare_symbol(&colours)
    }
}

fn is_valid_sector(sectr1: Option<f64>, sectr2: Option<f64>) -> bool {
    let (s1, s2) = match (sectr1, sectr2) {
        (Some(a), Some(b)) => (a, b),
        _ => return false,
    };
    let sweep = if s2 <= s1 { s2 - s1 + 360.0 } else { s2 - s1 };
    sweep >= 1.0 && sweep != 360.0
}

/// Sector geometry parameters for LIGHTS05/06.
pub fn light_sector_info(feature: &Feature) -> Option<LightSectorInfo> {
    let sectr1 = feature.attribute_float("SECTR1")?;
    let sectr2 = feature.attribute_float("SECTR2")?;
    if !is_valid_sector(Some(sectr1), Some(sectr2)) {
        return None;
    }

    let valnmr = feature.attribute_float("VALNMR").unwrap_or(9.0);

    // Arc radius in mm (OpenCPN)
    let arc_radius_mm = if valnmr > 0.0 {
        if valnmr < 15.0 {
            10.0
        } else if valnmr < 30.0 {
            15.0
        } else {
            20.0
        }
    } else {
        20.0
    };

    // OpenCPN emits 25mm sector legs, but that produces very cluttered results
    // in our current label/line pipeline for dense bridge light clusters.
    // Keep the same arc radius logic, but shorten the leg extent slightly.
    let sector_radius_mm = (arc_radius_mm + 4.0_f64).min(22.0_f64);

    let mut colors = parse_color_list(feature.attribute_str("COLOUR"));
    if colors.is_empty() {
        colors.push(12); // default magenta
    }

    let litvis = parse_litvis(feature.attribute_str("LITVIS"));
    let faint = litvis.iter().any(|v| matches!(v, 3 | 7 | 8));

    Some(LightSectorInfo {
        sectr1,
        sectr2,
        arc_radius_mm,
        sector_radius_mm,
        arc_color_token: select_sector_color_token(&colors),
        faint,
    })
}

/// Get symbol info for a light feature
pub fn lights06_symbol(feature: &Feature, settings: &MarinerSettings) -> &'static str {
    lights06(feature, settings).symbol_name()
}

/// Render metadata for LIGHTS symbols, including optional rotation and ORIENT text.
pub struct LightRenderInfo {
    pub symbol_name: &'static str,
    pub rotation_deg: Option<f64>,
    pub orient_text: Option<String>,
}

pub fn light_render_info(feature: &Feature, settings: &MarinerSettings) -> LightRenderInfo {
    let catlit = parse_catlit(feature.attribute_str("CATLIT"));
    let is_directional = catlit.contains(&1) || catlit.contains(&16);
    let orient = feature.attribute_float("ORIENT");

    if is_directional {
        if let Some(orient_deg) = orient {
            // OpenCPN adds 180° to LIGHTS rotation (s52plib.cpp:3295-3298)
            // ORIENT = direction FROM which light is visible (from seaward)
            // Rotation = direction light is POINTING (toward seaward)
            let rotation = orient_deg + 180.0;
            let rotation = if rotation >= 360.0 {
                rotation - 360.0
            } else {
                rotation
            };
            return LightRenderInfo {
                symbol_name: lights06_symbol(feature, settings),
                rotation_deg: Some(rotation),
                orient_text: Some(format!("{:03.0} deg", orient_deg)), // Text shows original ORIENT
            };
        }
        return LightRenderInfo {
            symbol_name: "QUESMRK1",
            rotation_deg: None,
            orient_text: None,
        };
    }

    LightRenderInfo {
        symbol_name: lights06_symbol(feature, settings),
        rotation_deg: None,
        orient_text: None,
    }
}

// ============================================================
// LITDSN01 - Light Description Text
// ============================================================

struct LitchrInfo {
    text: String,
    grp2: bool,
    spost: Option<&'static str>,
}

/// LITCHR (Light Character) to abbreviation mapping.
/// Matches OpenCPN s52cnsy.cpp:3579-3699.
fn litchr_info(litchr: i32) -> Option<LitchrInfo> {
    let (text, grp2, spost) = match litchr {
        1 => ("F", false, None),
        2 => ("Fl", false, None),
        3 => ("LFl", false, None),
        4 => ("Q", false, None),
        5 => ("VQ", false, None),
        6 => ("UQ", false, None),
        7 => ("Iso", false, None),
        8 => ("Occ", false, None),
        9 => ("IQ", false, None),
        10 => ("IVQ", false, None),
        11 => ("IUQ", false, None),
        12 => ("Mo", false, None),
        13 => ("F + Fl", true, None),
        14 => ("Fl + LFl", true, None),
        15 => ("Occ + Fl", true, None),
        16 => ("F + LFl", true, None),
        17 => ("Al Occ", false, None),
        18 => ("Al LFl", false, None),
        19 => ("Al Fl", false, None),
        20 => ("Al Grp", false, None),
        21 => ("F", false, Some(" (vert)")),
        22 => ("F", false, Some(" (horz)")),
        23 => ("F", false, Some(" (vert)")),
        24 => ("F", false, Some(" (horz)")),
        25 => ("Q + LFl", true, None),
        26 => ("VQ + LFl", true, None),
        27 => ("UQ + LFl", true, None),
        28 => ("Alt", false, None),
        29 => ("F + Alt", true, None),
        _ => return None,
    };

    Some(LitchrInfo {
        text: text.to_string(),
        grp2,
        spost,
    })
}

/// COLOUR code to single-letter abbreviation.
/// From S-57 spec: 1=W, 3=R, 4=G, 6=Y, etc.
fn colour_to_letter(colour: u8) -> Option<char> {
    match colour {
        1 => Some('W'), // White
        3 => Some('R'), // Red
        4 => Some('G'), // Green
        6 => Some('Y'), // Yellow
        _ => None,
    }
}

/// LITDSN01 - Generate light description text.
///
/// Produces strings like "Fl(3)G 10s 15m 10Nm" matching OpenCPN's _LITDSN01.
/// Reference: s52cnsy.cpp:3532-3879
pub fn litdsn01(feature: &Feature) -> Option<String> {
    let mut result = String::new();
    let mut spost: Option<&'static str> = None;
    let mut grp2 = false;
    let mut grp1_idx: Option<usize> = None;

    // Phase 1: LITCHR (Light Character)
    if let Some(litchr) = feature.attribute_int("LITCHR") {
        if let Some(info) = litchr_info(litchr) {
            result.push_str(&info.text);
            spost = info.spost;
            grp2 = info.grp2;

            if grp2 {
                if let Some(space_idx) = result.find(' ') {
                    result.insert_str(space_idx, "(?)");
                    grp1_idx = Some(space_idx + 1);
                }
            }
        }
    }

    // Phase 2: SIGGRP (Signal Group)
    if let Some(siggrp) = feature.attribute_str("SIGGRP") {
        let trimmed = siggrp.trim();
        if grp2 {
            let mut tokens: Vec<String> = Vec::new();
            let mut current = String::new();
            let mut in_paren = false;
            for ch in trimmed.chars() {
                if ch == '(' {
                    in_paren = true;
                    current.clear();
                } else if ch == ')' {
                    if in_paren && !current.is_empty() {
                        tokens.push(current.clone());
                    }
                    in_paren = false;
                } else if in_paren {
                    current.push(ch);
                }
            }

            if let Some(idx) = grp1_idx {
                if let Some(first) = tokens.get(0).and_then(|s| s.chars().next()) {
                    let mut chars: Vec<char> = result.chars().collect();
                    if idx < chars.len() {
                        chars[idx] = first;
                        result = chars.into_iter().collect();
                    }
                }
            }

            if let Some(second) = tokens.get(1) {
                if second != "1" {
                    result.push('(');
                    result.push_str(second);
                    result.push(')');
                }
            }
        } else if !trimmed.is_empty() && trimmed != "(1)" {
            result.push_str(trimmed);
        }
    }

    // Check if this is a sectored light (skip COLOUR and VALNMR in text)
    let is_sectored = feature.attribute_float("SECTR1").is_some();

    // Phase 3: COLOUR (only for non-sectored lights)
    if !is_sectored {
        let colors = parse_color_list(feature.attribute_str("COLOUR"));
        if !colors.is_empty() {
            result.push(' ');
            for c in &colors {
                if let Some(letter) = colour_to_letter(*c) {
                    result.push(letter);
                }
            }
        }
    }

    // Phase 4: SIGPER (Signal Period)
    if let Some(sigper) = feature.attribute_float("SIGPER") {
        if sigper > 0.0 {
            result.push(' ');
            // Use decimal only if needed
            if (sigper.round() - sigper).abs() > 0.01 {
                result.push_str(&format!("{:.1}s", sigper));
            } else {
                result.push_str(&format!("{:.0}s", sigper));
            }
        }
    }

    // Phase 5: HEIGHT
    if let Some(height) = feature.attribute_float("HEIGHT") {
        if height > 0.0 {
            result.push(' ');
            result.push_str(&format!("{:.0}m", height));
        }
    }

    // Phase 6: VALNMR (Nominal Range) - only for non-sectored lights
    if !is_sectored {
        if let Some(valnmr) = feature.attribute_float("VALNMR") {
            if valnmr > 0.0 {
                result.push(' ');
                result.push_str(&format!("{:.0}Nm", valnmr));
            }
        }
    }

    if let Some(suffix) = spost {
        result.push_str(suffix);
    }

    // Return None if empty
    let trimmed = result.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::senc::{AttributeValue, FeatureType, ObjectClass};
    use std::collections::HashMap;

    fn make_light(colour: Option<&str>, valnmr: Option<f64>, catlit: Option<&str>) -> Feature {
        let mut attributes = HashMap::new();
        if let Some(c) = colour {
            attributes.insert("COLOUR".to_string(), AttributeValue::String(c.to_string()));
        }
        if let Some(v) = valnmr {
            attributes.insert("VALNMR".to_string(), AttributeValue::Float(v));
        }
        if let Some(cat) = catlit {
            attributes.insert(
                "CATLIT".to_string(),
                AttributeValue::String(cat.to_string()),
            );
        }
        Feature {
            type_code: 0,
            object_class: ObjectClass::Light,
            feature_type: FeatureType::Point,
            attributes,
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        }
    }

    fn make_sector_light(sectr1: f64, sectr2: f64) -> Feature {
        let mut attributes = HashMap::new();
        attributes.insert("SECTR1".to_string(), AttributeValue::Float(sectr1));
        attributes.insert("SECTR2".to_string(), AttributeValue::Float(sectr2));
        Feature {
            type_code: 0,
            object_class: ObjectClass::Light,
            feature_type: FeatureType::Point,
            attributes,
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        }
    }

    #[test]
    fn test_red_flare() {
        let feature = make_light(Some("3"), Some(5.0), None);
        let settings = MarinerSettings::default();
        assert_eq!(lights06(&feature, &settings), LightSymbol::RedFlare);
        assert_eq!(lights06_symbol(&feature, &settings), "LIGHTS11");
    }

    #[test]
    fn test_green_all_round() {
        let feature = make_light(Some("4"), Some(15.0), None);
        let settings = MarinerSettings::default();
        assert_eq!(lights06(&feature, &settings), LightSymbol::GreenAllRound);
        assert_eq!(lights06_symbol(&feature, &settings), "LIGHTS92");
    }

    #[test]
    fn test_strobe_light() {
        let feature = make_light(Some("3"), Some(5.0), Some("11"));
        let settings = MarinerSettings::default();
        assert_eq!(lights06(&feature, &settings), LightSymbol::Strobe);
        assert_eq!(lights06_symbol(&feature, &settings), "LIGHTS82");
    }

    #[test]
    fn test_sector_sweep_too_small_uses_all_round() {
        let mut feature = make_sector_light(10.0, 10.2);
        feature.attributes.insert(
            "COLOUR".to_string(),
            AttributeValue::String("3".to_string()),
        );
        let settings = MarinerSettings::default();
        assert_eq!(lights06(&feature, &settings), LightSymbol::RedAllRound);
        assert!(light_sector_info(&feature).is_none());
    }

    #[test]
    fn test_sector_sweep_full_circle_uses_all_round() {
        let mut feature = make_sector_light(10.0, 370.0);
        feature.attributes.insert(
            "COLOUR".to_string(),
            AttributeValue::String("4".to_string()),
        );
        let settings = MarinerSettings::default();
        assert_eq!(lights06(&feature, &settings), LightSymbol::GreenAllRound);
        assert!(light_sector_info(&feature).is_none());
    }

    // ============================================================
    // LITDSN01 Tests
    // ============================================================

    fn make_full_light(
        litchr: Option<i32>,
        siggrp: Option<&str>,
        colour: Option<&str>,
        sigper: Option<f64>,
        height: Option<f64>,
        valnmr: Option<f64>,
    ) -> Feature {
        let mut attributes = HashMap::new();
        if let Some(v) = litchr {
            attributes.insert("LITCHR".to_string(), AttributeValue::Integer(v));
        }
        if let Some(v) = siggrp {
            attributes.insert("SIGGRP".to_string(), AttributeValue::String(v.to_string()));
        }
        if let Some(v) = colour {
            attributes.insert("COLOUR".to_string(), AttributeValue::String(v.to_string()));
        }
        if let Some(v) = sigper {
            attributes.insert("SIGPER".to_string(), AttributeValue::Float(v));
        }
        if let Some(v) = height {
            attributes.insert("HEIGHT".to_string(), AttributeValue::Float(v));
        }
        if let Some(v) = valnmr {
            attributes.insert("VALNMR".to_string(), AttributeValue::Float(v));
        }
        Feature {
            type_code: 0,
            object_class: ObjectClass::Light,
            feature_type: FeatureType::Point,
            attributes,
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        }
    }

    #[test]
    fn test_litdsn01_basic() {
        // Fl(3)G 10s 15m 10M
        let feature = make_full_light(
            Some(2),     // LITCHR = Flashing
            Some("(3)"), // SIGGRP
            Some("4"),   // COLOUR = Green
            Some(10.0),  // SIGPER
            Some(15.0),  // HEIGHT
            Some(10.0),  // VALNMR
        );
        let desc = litdsn01(&feature).unwrap();
        assert_eq!(desc, "Fl(3) G 10s 15m 10Nm");
    }

    #[test]
    fn test_litdsn01_skip_siggrp_one() {
        // Single flash - "(1)" should be omitted
        let feature = make_full_light(
            Some(2),     // LITCHR = Flashing
            Some("(1)"), // SIGGRP - should be skipped
            Some("1"),   // COLOUR = White
            Some(5.0),   // SIGPER
            None,
            None,
        );
        let desc = litdsn01(&feature).unwrap();
        assert_eq!(desc, "Fl W 5s");
    }

    #[test]
    fn test_litdsn01_decimal_period() {
        let feature = make_full_light(
            Some(7), // LITCHR = Isophased
            None,
            Some("3"), // COLOUR = Red
            Some(4.5), // SIGPER - decimal
            None,
            None,
        );
        let desc = litdsn01(&feature).unwrap();
        assert_eq!(desc, "Iso R 4.5s");
    }

    #[test]
    fn test_litdsn01_sector_skips_colour_and_range() {
        // Sectored light - should skip COLOUR and VALNMR
        let mut feature = make_full_light(
            Some(2), // LITCHR = Flashing
            None,
            Some("4"),  // COLOUR = Green (should be skipped)
            Some(10.0), // SIGPER
            Some(20.0), // HEIGHT
            Some(15.0), // VALNMR (should be skipped)
        );
        feature
            .attributes
            .insert("SECTR1".to_string(), AttributeValue::Float(90.0));
        feature
            .attributes
            .insert("SECTR2".to_string(), AttributeValue::Float(180.0));

        let desc = litdsn01(&feature).unwrap();
        assert_eq!(desc, "Fl 10s 20m"); // No G, no 15Nm
    }

    #[test]
    fn test_litdsn01_empty() {
        // No attributes - should return None
        let feature = make_full_light(None, None, None, None, None, None);
        assert!(litdsn01(&feature).is_none());
    }

    #[test]
    fn test_litdsn01_morse() {
        let feature = make_full_light(
            Some(12),    // LITCHR = Morse
            Some("(A)"), // SIGGRP
            Some("1"),   // COLOUR = White
            Some(30.0),  // SIGPER
            None,
            None,
        );
        let desc = litdsn01(&feature).unwrap();
        assert_eq!(desc, "Mo(A) W 30s");
    }
}
