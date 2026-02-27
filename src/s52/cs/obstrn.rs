//! OBSTRN04 - Obstruction Conditional Symbology Procedure
//!
//! Handles OBSTRN (obstruction) and UWTROC (underwater rock) features.
//! Selects appropriate danger symbols based on depth (VALSOU), water level (WATLEV),
//! and safety contour settings.
//!
//! OpenCPN reference: s52cnsy.cpp:1655-1970

use crate::s52::instruction::{LinePattern, RenderInstruction};
use crate::s52::MarinerSettings;
use crate::senc::{Feature, FeatureType, ObjectClass};

/// Symbol result from OBSTRN04 procedure
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObstrnSymbol {
    /// Standard obstruction symbol
    Obstrn01,
    /// Above water obstruction
    Obstrn03,
    /// Alternative obstruction (above water)
    Obstrn11,
    /// Rock awash or submerged
    Uwtroc03,
    /// Underwater rock, default
    Uwtroc04,
    /// Danger symbol (depth value shown, covers/uncovers)
    Danger51,
    /// Danger symbol (depth value shown, always submerged)
    Danger52,
    /// Danger symbol (awash or submerged, no depth)
    Danger53,
    /// Isolated danger symbol (within safety contour)
    Isodgr51,
    /// Land area symbol (always dry)
    Lndare01,
    /// Foul ground symbol
    Foulgnd1,
}

impl ObstrnSymbol {
    pub fn symbol_name(&self) -> &'static str {
        match self {
            Self::Obstrn01 => "OBSTRN01",
            Self::Obstrn03 => "OBSTRN03",
            Self::Obstrn11 => "OBSTRN11",
            Self::Uwtroc03 => "UWTROC03",
            Self::Uwtroc04 => "UWTROC04",
            Self::Danger51 => "DANGER51",
            Self::Danger52 => "DANGER52",
            Self::Danger53 => "DANGER53",
            Self::Isodgr51 => "ISODGR51",
            Self::Lndare01 => "LNDARE01",
            Self::Foulgnd1 => "FOULGND1",
        }
    }
}

/// Result from OBSTRN04 procedure
pub struct Obstrn04Result {
    /// Primary symbol to display
    pub symbol: ObstrnSymbol,
    /// Whether to show sounding value alongside symbol
    pub show_sounding: bool,
    /// Whether this is an isolated danger (should be DISPLAYBASE priority)
    pub is_isolated_danger: bool,
    /// Depth value for sounding display (if show_sounding is true)
    pub depth_value: Option<f64>,
}

/// Check if feature is within safety contour (simplified UDWHAZ03 check)
///
/// Full implementation would require spatial queries to find intersecting
/// DEPARE/DRGARE areas. This simplified version just compares depth to safety contour.
fn is_isolated_danger(valsou: Option<f64>, settings: &MarinerSettings) -> bool {
    if let Some(depth) = valsou {
        // Danger if depth is less than safety contour
        depth < settings.safety_contour as f64 && depth >= 0.0
    } else {
        false
    }
}

/// OBSTRN04 - Obstruction symbol selection for point features
///
/// Matches OpenCPN logic for selecting obstruction symbols based on:
/// - VALSOU: Sounding depth value
/// - WATLEV: Water level (1=always dry, 2=always submerged, 3=covers/uncovers, 4=awash, 5=subject to inundation)
/// - EXPSOU: Exposition of sounding
/// - Object class (OBSTRN vs UWTROC)
pub fn obstrn04(feature: &Feature, settings: &MarinerSettings) -> Obstrn04Result {
    let valsou = feature.attribute_float("VALSOU");
    let watlev = feature.attribute_int("WATLEV").unwrap_or(0);
    let expsou = feature.attribute_int("EXPSOU").unwrap_or(0);
    let catobs = feature.attribute_int("CATOBS").unwrap_or(0);

    let is_uwtroc = feature.object_class == ObjectClass::UnderwaterRock;
    let is_isolated = is_isolated_danger(valsou, settings);

    // Determine depth value for danger assessment
    let depth_value = if let Some(v) = valsou {
        Some(v)
    } else {
        // Infer depth from WATLEV when VALSOU is missing
        match watlev {
            5 => Some(0.0),      // Subject to inundation
            3 => Some(0.01),     // Covers and uncovers
            1 | 2 | 4 => Some(-15.0), // Above water
            _ => {
                // Check CATOBS for foul ground
                if catobs == 6 && expsou != 1 {
                    Some(0.01)
                } else {
                    Some(-15.0)
                }
            }
        }
    };

    // Check for isolated danger (UDWHAZ03)
    if is_isolated {
        // Only show isolated danger symbol if not always dry
        if watlev != 1 && watlev != 2 {
            return Obstrn04Result {
                symbol: ObstrnSymbol::Isodgr51,
                show_sounding: false,
                is_isolated_danger: true,
                depth_value,
            };
        }
    }

    // Handle point features
    if feature.feature_type == FeatureType::Point {
        return obstrn04_point(feature, valsou, watlev, is_uwtroc, depth_value);
    }

    // Handle line features
    if feature.feature_type == FeatureType::Line {
        return obstrn04_line(valsou, depth_value);
    }

    // Handle area features
    obstrn04_area(valsou, watlev, is_isolated, depth_value)
}

fn obstrn04_point(
    _feature: &Feature,
    valsou: Option<f64>,
    watlev: i32,
    is_uwtroc: bool,
    depth_value: Option<f64>,
) -> Obstrn04Result {
    let mut show_sounding = false;

    let symbol = if let Some(v) = valsou {
        if v <= 20.0 {
            if is_uwtroc {
                // Underwater rock with sounding
                match watlev {
                    3 => {
                        show_sounding = true;
                        ObstrnSymbol::Danger51
                    }
                    4 | 5 => ObstrnSymbol::Uwtroc04,
                    _ => {
                        show_sounding = true;
                        ObstrnSymbol::Danger51
                    }
                }
            } else {
                // OBSTRN with sounding
                match watlev {
                    1 | 2 => ObstrnSymbol::Lndare01,
                    3 => {
                        show_sounding = true;
                        ObstrnSymbol::Danger52
                    }
                    4 | 5 => {
                        show_sounding = true;
                        ObstrnSymbol::Danger53
                    }
                    _ => {
                        show_sounding = true;
                        ObstrnSymbol::Danger51
                    }
                }
            }
        } else {
            // Deep (> 20m)
            show_sounding = true;
            ObstrnSymbol::Danger52
        }
    } else {
        // No VALSOU
        if is_uwtroc {
            match watlev {
                2 => ObstrnSymbol::Lndare01,
                3 => ObstrnSymbol::Uwtroc03,
                _ => ObstrnSymbol::Uwtroc04,
            }
        } else {
            match watlev {
                1 | 2 => ObstrnSymbol::Obstrn11,
                3 => ObstrnSymbol::Obstrn01,
                4 | 5 => ObstrnSymbol::Obstrn03,
                _ => ObstrnSymbol::Obstrn01,
            }
        }
    };

    Obstrn04Result {
        symbol,
        show_sounding,
        is_isolated_danger: false,
        depth_value,
    }
}

fn obstrn04_line(valsou: Option<f64>, depth_value: Option<f64>) -> Obstrn04Result {
    // Line features get line style, not symbol
    // This returns a placeholder - actual line style handled separately
    Obstrn04Result {
        symbol: ObstrnSymbol::Obstrn01,
        show_sounding: valsou.is_some() && valsou.unwrap() <= 20.0,
        is_isolated_danger: false,
        depth_value,
    }
}

fn obstrn04_area(
    valsou: Option<f64>,
    watlev: i32,
    is_isolated: bool,
    depth_value: Option<f64>,
) -> Obstrn04Result {
    // Area obstructions get fill pattern + line style
    if is_isolated {
        return Obstrn04Result {
            symbol: ObstrnSymbol::Isodgr51,
            show_sounding: false,
            is_isolated_danger: true,
            depth_value,
        };
    }

    let show_sounding = valsou.is_some() && valsou.unwrap() <= 20.0;

    // Default area obstruction symbol
    let symbol = match watlev {
        1 | 2 => ObstrnSymbol::Obstrn11,
        3 => ObstrnSymbol::Obstrn01,
        _ => ObstrnSymbol::Obstrn01,
    };

    Obstrn04Result {
        symbol,
        show_sounding,
        is_isolated_danger: false,
        depth_value,
    }
}

/// Generate render instructions for OBSTRN04
pub fn obstrn04_instructions(feature: &Feature, settings: &MarinerSettings) -> Vec<RenderInstruction> {
    let result = obstrn04(feature, settings);
    let mut instructions = Vec::new();

    // Add symbol instruction
    instructions.push(RenderInstruction::Symbol {
        name: result.symbol.symbol_name().to_string(),
    });

    // Add sounding text if needed
    if result.show_sounding {
        let depth = result.depth_value.unwrap_or(0.0);
        let color = if depth <= 20.0 { "CHBLK" } else { "CHGRD" };
        
        instructions.push(RenderInstruction::Text {
            attribute: "VALSOU".to_string(),
            format: Some("%4.1lf".to_string()),
            hjust: 3, // Left
            vjust: 1, // Bottom
            xoffs: 1,
            yoffs: 1,
            color: color.to_string(),
        });
    }

    // For area features, add fill pattern and line style
    if feature.feature_type == FeatureType::Area {
        instructions.push(RenderInstruction::AreaPattern {
            pattern: "FOULAR01".to_string(),
        });
        instructions.push(RenderInstruction::LineStyle {
            pattern: LinePattern::Dotted,
            width: 2,
            color: "CHBLK".to_string(),
        });
    }

    // For line features, add line style
    if feature.feature_type == FeatureType::Line {
        let valsou = feature.attribute_float("VALSOU");
        let pattern = if valsou.is_some() && valsou.unwrap() > 20.0 {
            LinePattern::Dashed
        } else {
            LinePattern::Dotted
        };
        instructions.push(RenderInstruction::LineStyle {
            pattern,
            width: 2,
            color: "CHBLK".to_string(),
        });
    }

    instructions
}

/// Get the symbol name for an obstruction feature
pub fn obstrn04_symbol(feature: &Feature, settings: &MarinerSettings) -> &'static str {
    obstrn04(feature, settings).symbol.symbol_name()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::senc::AttributeValue;
    use std::collections::HashMap;

    fn make_obstrn(valsou: Option<f64>, watlev: Option<i32>) -> Feature {
        let mut attributes = HashMap::new();
        if let Some(v) = valsou {
            attributes.insert("VALSOU".to_string(), AttributeValue::Float(v));
        }
        if let Some(w) = watlev {
            attributes.insert("WATLEV".to_string(), AttributeValue::Integer(w));
        }
        Feature {
            type_code: 0,
            object_class: ObjectClass::Obstruction,
            feature_type: FeatureType::Point,
            attributes,
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        }
    }

    fn make_uwtroc(valsou: Option<f64>, watlev: Option<i32>) -> Feature {
        let mut attributes = HashMap::new();
        if let Some(v) = valsou {
            attributes.insert("VALSOU".to_string(), AttributeValue::Float(v));
        }
        if let Some(w) = watlev {
            attributes.insert("WATLEV".to_string(), AttributeValue::Integer(w));
        }
        Feature {
            type_code: 0,
            object_class: ObjectClass::UnderwaterRock,
            feature_type: FeatureType::Point,
            attributes,
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        }
    }

    #[test]
    fn test_obstrn_no_valsou_default() {
        let feature = make_obstrn(None, None);
        let settings = MarinerSettings::default();
        let result = obstrn04(&feature, &settings);
        assert_eq!(result.symbol, ObstrnSymbol::Obstrn01);
        assert!(!result.show_sounding);
    }

    #[test]
    fn test_obstrn_always_dry() {
        // WATLEV=1 (always dry) -> OBSTRN11
        let feature = make_obstrn(None, Some(1));
        let settings = MarinerSettings::default();
        let result = obstrn04(&feature, &settings);
        assert_eq!(result.symbol, ObstrnSymbol::Obstrn11);
    }

    #[test]
    fn test_obstrn_covers_uncovers_with_depth() {
        // WATLEV=3 (covers/uncovers) with VALSOU > safety_contour -> DANGER52
        // (shallow depths trigger isolated danger instead)
        let feature = make_obstrn(Some(15.0), Some(3));
        let settings = MarinerSettings::default();
        let result = obstrn04(&feature, &settings);
        assert_eq!(result.symbol, ObstrnSymbol::Danger52);
        assert!(result.show_sounding);
    }

    #[test]
    fn test_obstrn_instructions_with_text() {
        let feature = make_obstrn(Some(15.0), Some(3));
        let settings = MarinerSettings::default();
        let instrs = obstrn04_instructions(&feature, &settings);
        
        // Should have Symbol and Text
        assert!(instrs.len() >= 2);
        assert!(matches!(instrs[0], RenderInstruction::Symbol { .. }));
        if let RenderInstruction::Text { attribute, .. } = &instrs[1] {
            assert_eq!(attribute, "VALSOU");
        } else {
            panic!("Expected Text instruction");
        }
    }

    #[test]
    fn test_obstrn_awash_with_depth() {
        // WATLEV=4 (awash) with VALSOU > safety_contour -> DANGER53
        let feature = make_obstrn(Some(12.0), Some(4));
        let settings = MarinerSettings::default();
        let result = obstrn04(&feature, &settings);
        assert_eq!(result.symbol, ObstrnSymbol::Danger53);
        assert!(result.show_sounding);
    }

    #[test]
    fn test_uwtroc_covers_uncovers_no_valsou() {
        // UWTROC with WATLEV=3 (covers/uncovers), no VALSOU -> UWTROC03
        let feature = make_uwtroc(None, Some(3));
        let settings = MarinerSettings::default();
        let result = obstrn04(&feature, &settings);
        assert_eq!(result.symbol, ObstrnSymbol::Uwtroc03);
    }

    #[test]
    fn test_uwtroc_with_depth() {
        // UWTROC with VALSOU > safety_contour, WATLEV=3 -> DANGER51
        let feature = make_uwtroc(Some(15.0), Some(3));
        let settings = MarinerSettings::default();
        let result = obstrn04(&feature, &settings);
        assert_eq!(result.symbol, ObstrnSymbol::Danger51);
        assert!(result.show_sounding);
    }

    #[test]
    fn test_isolated_danger() {
        // Shallow obstruction (VALSOU < safety_contour) -> ISODGR51
        let feature = make_obstrn(Some(5.0), Some(3));
        let settings = MarinerSettings::default(); // safety_contour = 10.0
        let result = obstrn04(&feature, &settings);
        assert_eq!(result.symbol, ObstrnSymbol::Isodgr51);
        assert!(result.is_isolated_danger);
    }

    #[test]
    fn test_deep_obstruction() {
        // Deep obstruction (VALSOU > 20m) -> DANGER52
        let feature = make_obstrn(Some(25.0), Some(3));
        let settings = MarinerSettings::default();
        let result = obstrn04(&feature, &settings);
        assert_eq!(result.symbol, ObstrnSymbol::Danger52);
        assert!(result.show_sounding);
    }

    #[test]
    fn test_obstrn04_symbol() {
        let feature = make_obstrn(None, Some(4));
        let settings = MarinerSettings::default();
        let symbol = obstrn04_symbol(&feature, &settings);
        assert_eq!(symbol, "OBSTRN03");
    }
}
