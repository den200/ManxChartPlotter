//! WRECKS02 - Wreck Conditional Symbology Procedure
//!
//! Selects appropriate wreck symbols based on depth (VALSOU), water level (WATLEV),
//! category (CATWRK), and safety contour settings.
//!
//! OpenCPN reference: s52cnsy.cpp:3239-3500

use crate::s52::instruction::RenderInstruction;
use crate::s52::MarinerSettings;
use crate::senc::{Feature, FeatureType};

/// Symbol result from WRECKS02 procedure
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WrecksSymbol {
    /// Non-dangerous wreck
    Wrecks01,
    /// Dangerous wreck (covers, drying)
    Wrecks04,
    /// Mast showing
    Wrecks05,
    /// Wreck with QUASOU=7 (least depth unknown)
    Wrecks07,
    /// Isolated danger wreck
    Isodgr51,
    /// Foul ground symbol
    Foulgnd1,
}

impl WrecksSymbol {
    pub fn symbol_name(&self) -> &'static str {
        match self {
            Self::Wrecks01 => "WRECKS01",
            Self::Wrecks04 => "WRECKS04",
            Self::Wrecks05 => "WRECKS05",
            Self::Wrecks07 => "WRECKS07",
            Self::Isodgr51 => "ISODGR51",
            Self::Foulgnd1 => "FOULGND1",
        }
    }
}

/// Result from WRECKS02 procedure
pub struct Wrecks02Result {
    /// Primary symbol to display
    pub symbol: WrecksSymbol,
    /// Whether to show sounding value alongside symbol
    pub show_sounding: bool,
    /// Whether this is an isolated danger (should be DISPLAYBASE priority)
    pub is_isolated_danger: bool,
    /// Depth value for sounding display (if show_sounding is true)
    pub depth_value: Option<f64>,
}

/// Parse QUASOU attribute (comma-separated list)
fn parse_quasou(quasou_attr: Option<&str>) -> Vec<u8> {
    match quasou_attr {
        Some(s) => s
            .split(',')
            .filter_map(|c| c.trim().parse::<u8>().ok())
            .collect(),
        None => vec![],
    }
}

/// Check if feature is within safety contour (simplified UDWHAZ03 check)
fn is_isolated_danger(valsou: Option<f64>, settings: &MarinerSettings) -> bool {
    if let Some(depth) = valsou {
        depth < settings.safety_contour as f64 && depth >= 0.0
    } else {
        false
    }
}

/// WRECKS02 - Wreck symbol selection
///
/// Matches OpenCPN logic for selecting wreck symbols based on:
/// - VALSOU: Sounding depth value
/// - WATLEV: Water level (1=always dry, 2=always submerged, 3=covers/uncovers, 4=awash, 5=subject to inundation)
/// - CATWRK: Category of wreck (1=non-dangerous, 2=dangerous, 4=shows hull, 5=shows mast)
/// - QUASOU: Quality of sounding (7 = least depth unknown)
/// - EXPSOU: Exposition of sounding
pub fn wrecks02(feature: &Feature, settings: &MarinerSettings) -> Wrecks02Result {
    let valsou = feature.attribute_float("VALSOU");
    let watlev = feature.attribute_int("WATLEV").unwrap_or(0);
    let catwrk = feature.attribute_int("CATWRK").unwrap_or(0);
    let _expsou = feature.attribute_int("EXPSOU").unwrap_or(0);
    let quasou = parse_quasou(feature.attribute_str("QUASOU"));

    // Check for QUASOU=7 (least depth unknown, safe clearance at value shown)
    let has_quasou_7 = quasou.contains(&7);

    // Determine depth value for danger assessment
    let depth_value = if let Some(v) = valsou {
        Some(v)
    } else {
        // Infer depth from WATLEV when VALSOU is missing
        match watlev {
            5 => Some(0.0),           // Subject to inundation
            3 => Some(0.01),          // Covers and uncovers
            1 | 2 | 4 => Some(-15.0), // Above water
            _ => Some(-15.0),         // Default: above water
        }
    };

    // Check for isolated danger (UDWHAZ03) - skip if QUASOU=7
    if !has_quasou_7 && is_isolated_danger(valsou, settings) {
        // Only show isolated danger symbol if not always dry
        if watlev != 1 && watlev != 2 {
            return Wrecks02Result {
                symbol: WrecksSymbol::Isodgr51,
                show_sounding: false,
                is_isolated_danger: true,
                depth_value,
            };
        }
    }

    // QUASOU=7 gets special symbol
    if has_quasou_7 {
        return Wrecks02Result {
            symbol: WrecksSymbol::Wrecks07,
            show_sounding: valsou.is_some(),
            is_isolated_danger: false,
            depth_value,
        };
    }

    // Handle point features
    if feature.feature_type == FeatureType::Point {
        return wrecks02_point(valsou, watlev, catwrk, depth_value);
    }

    // Handle area features (similar logic to point)
    wrecks02_area(valsou, watlev, catwrk, depth_value)
}

fn wrecks02_point(
    valsou: Option<f64>,
    watlev: i32,
    catwrk: i32,
    depth_value: Option<f64>,
) -> Wrecks02Result {
    let mut show_sounding = false;

    let symbol = if let Some(v) = valsou {
        show_sounding = true;

        if v <= 20.0 {
            // Select symbol based on WATLEV and CATWRK
            match watlev {
                1 | 2 => WrecksSymbol::Wrecks01, // Always dry/above water
                3 => WrecksSymbol::Wrecks04,     // Covers and uncovers - dangerous
                4 | 5 => {
                    // Awash or submerged
                    if catwrk == 5 {
                        WrecksSymbol::Wrecks05 // Shows mast
                    } else {
                        WrecksSymbol::Wrecks04 // Dangerous
                    }
                }
                _ => {
                    // Default selection based on CATWRK
                    match catwrk {
                        1 => WrecksSymbol::Wrecks01, // Non-dangerous
                        2 => WrecksSymbol::Wrecks04, // Dangerous
                        4 => WrecksSymbol::Wrecks04, // Shows hull
                        5 => WrecksSymbol::Wrecks05, // Shows mast
                        _ => WrecksSymbol::Wrecks05, // Default: mast showing
                    }
                }
            }
        } else {
            // Deep wreck (> 20m) - non-dangerous
            WrecksSymbol::Wrecks01
        }
    } else {
        // No VALSOU - use CATWRK and WATLEV
        match catwrk {
            1 => WrecksSymbol::Wrecks01, // Non-dangerous
            2 => WrecksSymbol::Wrecks04, // Dangerous
            4 => WrecksSymbol::Wrecks04, // Shows hull
            5 => WrecksSymbol::Wrecks05, // Shows mast
            _ => {
                // Default based on WATLEV
                match watlev {
                    1 | 2 => WrecksSymbol::Wrecks01,
                    3 => WrecksSymbol::Wrecks04,
                    _ => WrecksSymbol::Wrecks05, // Default
                }
            }
        }
    };

    Wrecks02Result {
        symbol,
        show_sounding,
        is_isolated_danger: false,
        depth_value,
    }
}

fn wrecks02_area(
    valsou: Option<f64>,
    watlev: i32,
    catwrk: i32,
    depth_value: Option<f64>,
) -> Wrecks02Result {
    // Area wrecks use similar logic to points
    wrecks02_point(valsou, watlev, catwrk, depth_value)
}

/// Generate render instructions for WRECKS02
pub fn wrecks02_instructions(
    feature: &Feature,
    settings: &MarinerSettings,
) -> Vec<RenderInstruction> {
    let result = wrecks02(feature, settings);

    vec![RenderInstruction::Symbol {
        name: result.symbol.symbol_name().to_string(),
    }]
}

/// Get the symbol name for a wreck feature
pub fn wrecks02_symbol(feature: &Feature, settings: &MarinerSettings) -> &'static str {
    wrecks02(feature, settings).symbol.symbol_name()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::senc::{AttributeValue, FeatureType, ObjectClass};
    use std::collections::HashMap;

    fn make_wreck(valsou: Option<f64>, watlev: Option<i32>, catwrk: Option<i32>) -> Feature {
        let mut attributes = HashMap::new();
        if let Some(v) = valsou {
            attributes.insert("VALSOU".to_string(), AttributeValue::Float(v));
        }
        if let Some(w) = watlev {
            attributes.insert("WATLEV".to_string(), AttributeValue::Integer(w));
        }
        if let Some(c) = catwrk {
            attributes.insert("CATWRK".to_string(), AttributeValue::Integer(c));
        }
        Feature {
            type_code: 0,
            object_class: ObjectClass::Wreck,
            feature_type: FeatureType::Point,
            attributes,
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        }
    }

    #[test]
    fn test_wreck_non_dangerous() {
        // CATWRK=1 (non-dangerous)
        let feature = make_wreck(Some(25.0), None, Some(1));
        let settings = MarinerSettings::default();
        let result = wrecks02(&feature, &settings);
        assert_eq!(result.symbol, WrecksSymbol::Wrecks01);
    }

    #[test]
    fn test_wreck_dangerous() {
        // CATWRK=2 (dangerous) with shallow depth
        let feature = make_wreck(Some(5.0), Some(3), Some(2));
        let settings = MarinerSettings::default();
        let result = wrecks02(&feature, &settings);
        // Should be isolated danger since depth < safety_contour
        assert_eq!(result.symbol, WrecksSymbol::Isodgr51);
        assert!(result.is_isolated_danger);
    }

    #[test]
    fn test_wreck_mast_showing() {
        // CATWRK=5 (mast showing) with deep depth (> safety contour)
        let feature = make_wreck(Some(15.0), Some(4), Some(5));
        let settings = MarinerSettings::default(); // safety_contour = 10.0
        let result = wrecks02(&feature, &settings);
        assert_eq!(result.symbol, WrecksSymbol::Wrecks05);
    }

    #[test]
    fn test_wreck_quasou_7() {
        // QUASOU=7 (least depth unknown) gets special symbol
        let mut feature = make_wreck(Some(5.0), Some(3), None);
        feature.attributes.insert(
            "QUASOU".to_string(),
            AttributeValue::String("7".to_string()),
        );
        let settings = MarinerSettings::default();
        let result = wrecks02(&feature, &settings);
        assert_eq!(result.symbol, WrecksSymbol::Wrecks07);
    }

    #[test]
    fn test_wreck_covers_uncovers() {
        // WATLEV=3 (covers/uncovers) with depth > safety_contour
        let feature = make_wreck(Some(15.0), Some(3), None);
        let settings = MarinerSettings::default(); // safety_contour = 10.0
        let result = wrecks02(&feature, &settings);
        assert_eq!(result.symbol, WrecksSymbol::Wrecks04);
    }

    #[test]
    fn test_wreck_always_dry() {
        // WATLEV=1 (always dry)
        let feature = make_wreck(None, Some(1), None);
        let settings = MarinerSettings::default();
        let result = wrecks02(&feature, &settings);
        assert_eq!(result.symbol, WrecksSymbol::Wrecks01);
    }

    #[test]
    fn test_wreck_deep() {
        // Deep wreck (> 20m) is non-dangerous
        let feature = make_wreck(Some(30.0), Some(3), None);
        let settings = MarinerSettings::default();
        let result = wrecks02(&feature, &settings);
        assert_eq!(result.symbol, WrecksSymbol::Wrecks01);
        assert!(result.show_sounding);
    }

    #[test]
    fn test_wrecks02_symbol() {
        let feature = make_wreck(None, Some(3), Some(4));
        let settings = MarinerSettings::default();
        let symbol = wrecks02_symbol(&feature, &settings);
        assert_eq!(symbol, "WRECKS04");
    }
}
