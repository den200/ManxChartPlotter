//! OBSTRN04 - Obstruction Conditional Symbology Procedure
//!
//! Handles OBSTRN (obstruction) and UWTROC (underwater rock) features.
//! Selects appropriate danger symbols based on depth (VALSOU), water level (WATLEV),
//! and safety contour settings.
//!
//! OpenCPN reference: s52cnsy.cpp:1655-1970

use crate::s52::instruction::{LinePattern, RenderInstruction};
use crate::s52::cs::CsContext;
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

/// UDWHAZ03 — is this object an *isolated danger*?
///
/// Two conditions, both required (s52cnsy.cpp `_UDWHAZ03`):
///  1. the object is shallower than the safety contour (or its depth is
///     unknown and it is not an "exposed sounding"), and
///  2. the water it sits in is otherwise safe — i.e. an intersecting depth
///     area's DRVAL1 reaches the safety contour, or an intersecting depth line
///     is shallower than it.
///
/// Condition 2 is what needs the surrounding chart. Testing depth alone — as
/// navcore did before it had [`CsContext`] — marks every shallow rock inside
/// shallow water as an isolated danger, scattering ISODGR51 over ground that
/// OpenCPN symbolises as an ordinary rock with its sounding.
fn is_isolated_danger(
    depth_value: Option<f64>,
    expsou: i32,
    settings: &MarinerSettings,
    ctx: &CsContext,
) -> bool {
    let safety = settings.safety_contour as f64;
    match depth_value {
        // OpenCPN's UNKNOWN sentinel: dangerous unless it is an exposed
        // sounding, and this branch never consults the neighbourhood.
        None => expsou != 1,
        Some(depth) => {
            if expsou != 1 && depth > safety {
                return false;
            }
            ctx.indicates_danger(safety, expsou)
        }
    }
}

/// OBSTRN04 - Obstruction symbol selection for point features
///
/// Matches OpenCPN logic for selecting obstruction symbols based on:
/// - VALSOU: Sounding depth value
/// - WATLEV: Water level (1=always dry, 2=always submerged, 3=covers/uncovers, 4=awash, 5=subject to inundation)
/// - EXPSOU: Exposition of sounding
/// - Object class (OBSTRN vs UWTROC)
pub fn obstrn04(
    feature: &Feature,
    settings: &MarinerSettings,
    ctx: &CsContext,
) -> Obstrn04Result {
    let valsou = feature.attribute_float("VALSOU");
    let watlev = feature.attribute_int("WATLEV").unwrap_or(0);
    let expsou = feature.attribute_int("EXPSOU").unwrap_or(0);
    let catobs = feature.attribute_int("CATOBS").unwrap_or(0);

    let is_uwtroc = feature.object_class == ObjectClass::UnderwaterRock;

    // Depth used for the danger assessment. VALSOU when present, otherwise
    // inferred from CATOBS/WATLEV — but only for objects that are not exposed
    // soundings: with EXPSOU 1 and no VALSOU, s52cnsy.cpp leaves depth_value at
    // its UNKNOWN sentinel, which `None` stands for here.
    let depth_value = match valsou {
        Some(v) => Some(v),
        None if expsou == 1 => None,
        None => Some(if catobs == 6 {
            0.01
        } else {
            match watlev {
                5 => 0.0,  // subject to inundation
                3 => 0.01, // covers and uncovers
                _ => -15.0,
            }
        }),
    };

    let is_isolated = is_isolated_danger(depth_value, expsou, settings, ctx);

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

/// Generate render instructions for OBSTRN04.
///
/// Follows the three continuations of s52cnsy.cpp OBSTRN04 directly — point,
/// line and area are genuinely different symbolisations, not one symbol with
/// decorations. In particular an *area* obstruction is filled with a depth
/// shade (`AC`), never marked with a point symbol.
///
/// Deviation: where OpenCPN appends SNDFRM02's digit *symbols*
/// (`SY(SOUNDG21);SY(SOUNDG12)`), navcore emits the sounding as a text
/// instruction, as it does for soundings everywhere else.
pub fn obstrn04_instructions(
    feature: &Feature,
    settings: &MarinerSettings,
    ctx: &CsContext,
) -> Vec<RenderInstruction> {
    let result = obstrn04(feature, settings, ctx);
    let valsou = feature.attribute_float("VALSOU");
    // OpenCPN's "attribute absent" sentinel for WATLEV is -9, which is distinct
    // from every real enum value and selects the default branch.
    let watlev = feature.attribute_int("WATLEV");
    let catobs = feature.attribute_int("CATOBS");
    let mut out = Vec::new();

    // UDWHAZ03 only produces its symbol when the object is not permanently dry.
    let udwhaz = result.is_isolated_danger;

    match feature.feature_type {
        FeatureType::Point | FeatureType::Multipoint => {
            out.push(RenderInstruction::Symbol {
                name: result.symbol.symbol_name().to_string(),
                rotation: None,
            });
            if result.show_sounding {
                out.push(sounding_text(result.depth_value.unwrap_or(0.0)));
            }
        }

        FeatureType::Line => {
            // Continuation B
            let pattern = if udwhaz {
                LinePattern::Dotted
            } else {
                match valsou {
                    Some(v) if v > 20.0 => LinePattern::Dashed,
                    _ => LinePattern::Dotted,
                }
            };
            out.push(RenderInstruction::LineStyle {
                pattern,
                width: 2,
                color: "CHBLK".to_string(),
            });
            if !udwhaz {
                if let Some(v) = valsou {
                    if v <= 20.0 {
                        out.push(sounding_text(v));
                    }
                }
            }
        }

        FeatureType::Area => {
            // Continuation C
            if udwhaz {
                out.push(RenderInstruction::AreaColor {
                    color: "DEPVS".to_string(),
                    transparency: None,
                });
                out.push(RenderInstruction::AreaPattern {
                    pattern: "FOULAR01".to_string(),
                });
                out.push(RenderInstruction::LineStyle {
                    pattern: LinePattern::Dotted,
                    width: 2,
                    color: "CHBLK".to_string(),
                });
                out.push(RenderInstruction::Symbol {
                    name: ObstrnSymbol::Isodgr51.symbol_name().to_string(),
                    rotation: None,
                });
            } else if let Some(v) = valsou {
                out.push(RenderInstruction::LineStyle {
                    pattern: if v > 20.0 {
                        LinePattern::Dashed
                    } else {
                        LinePattern::Dotted
                    },
                    width: 2,
                    color: "CHBLK".to_string(),
                });
                out.push(sounding_text(v));
            } else {
                let (fill, line_pattern, line_color, foul) = match watlev {
                    Some(1) | Some(2) => ("CHBRN", LinePattern::Solid, "CSTLN", false),
                    Some(4) => ("DEPIT", LinePattern::Dashed, "CSTLN", false),
                    Some(3) | Some(5) => {
                        ("DEPVS", LinePattern::Dotted, "CHBLK", catobs == Some(6))
                    }
                    _ => ("DEPVS", LinePattern::Dotted, "CHBLK", false),
                };
                out.push(RenderInstruction::AreaColor {
                    color: fill.to_string(),
                    transparency: None,
                });
                if foul {
                    out.push(RenderInstruction::AreaPattern {
                        pattern: "FOULAR01".to_string(),
                    });
                }
                out.push(RenderInstruction::LineStyle {
                    pattern: line_pattern,
                    width: 2,
                    color: line_color.to_string(),
                });
            }
        }
    }

    // OpenCPN prints OBJNAM for a named obstruction ("Horn Rock" in the NZ
    // ENCs) regardless of primitive — s52cnsy.cpp appends this at `end:`.
    if feature.attribute_str("OBJNAM").is_some() {
        out.push(RenderInstruction::Text {
            attribute: "OBJNAM".to_string(),
            format: None,
            hjust: 1,
            vjust: 2,
            xoffs: -1,
            yoffs: -1,
            color: "CHBLK".to_string(),
            style: 1,
            weight: 5,
            width: 1,
            bsize: 18,
            // s52cnsy.cpp appends this verbatim as
            // "TX(OBJNAM,1,2,3,'15118',-1,-1,CHBLK,26)" — proportional.
            space: 3,
            dis: 26,
        });
    }

    out
}

/// The sounding value drawn beside an obstruction.
fn sounding_text(depth: f64) -> RenderInstruction {
    RenderInstruction::Text {
        attribute: "VALSOU".to_string(),
        format: Some("%4.1lf".to_string()),
        hjust: 3,
        vjust: 1,
        xoffs: 1,
        yoffs: 1,
        color: if depth <= 20.0 { "CHBLK" } else { "CHGRD" }.to_string(),
        style: 1,
        weight: 5,
        width: 1,
        bsize: 10,
        // Renderer-generated sounding text: standard pitch, as SNDFRM02's
        // digit symbols are evenly spaced.
        space: 2,
        dis: 11,
    }
}

/// Get the symbol name for an obstruction feature
pub fn obstrn04_symbol(feature: &Feature, settings: &MarinerSettings) -> &'static str {
    obstrn04(feature, settings, CsContext::EMPTY).symbol.symbol_name()
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

    use crate::senc::AttributeValue;

    fn make_obstrn(valsou: Option<f64>, watlev: Option<i32>) -> Feature {
        let mut attributes = crate::senc::Attributes::new();
        if let Some(v) = valsou {
            attributes.insert("VALSOU", AttributeValue::Float(v));
        }
        if let Some(w) = watlev {
            attributes.insert("WATLEV", AttributeValue::Integer(w));
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
        let mut attributes = crate::senc::Attributes::new();
        if let Some(v) = valsou {
            attributes.insert("VALSOU", AttributeValue::Float(v));
        }
        if let Some(w) = watlev {
            attributes.insert("WATLEV", AttributeValue::Integer(w));
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
        let settings = test_contours();
        let result = obstrn04(&feature, &settings, CsContext::EMPTY);
        assert_eq!(result.symbol, ObstrnSymbol::Obstrn01);
        assert!(!result.show_sounding);
    }

    #[test]
    fn test_obstrn_always_dry() {
        // WATLEV=1 (always dry) -> OBSTRN11
        let feature = make_obstrn(None, Some(1));
        let settings = test_contours();
        let result = obstrn04(&feature, &settings, CsContext::EMPTY);
        assert_eq!(result.symbol, ObstrnSymbol::Obstrn11);
    }

    #[test]
    fn test_obstrn_covers_uncovers_with_depth() {
        // WATLEV=3 (covers/uncovers) with VALSOU > safety_contour -> DANGER52
        // (shallow depths trigger isolated danger instead)
        let feature = make_obstrn(Some(15.0), Some(3));
        let settings = test_contours();
        let result = obstrn04(&feature, &settings, CsContext::EMPTY);
        assert_eq!(result.symbol, ObstrnSymbol::Danger52);
        assert!(result.show_sounding);
    }

    #[test]
    fn test_obstrn_instructions_with_text() {
        let feature = make_obstrn(Some(15.0), Some(3));
        let settings = test_contours();
        let instrs = obstrn04_instructions(&feature, &settings, CsContext::EMPTY);

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
        let settings = test_contours();
        let result = obstrn04(&feature, &settings, CsContext::EMPTY);
        assert_eq!(result.symbol, ObstrnSymbol::Danger53);
        assert!(result.show_sounding);
    }

    #[test]
    fn test_uwtroc_covers_uncovers_no_valsou() {
        // UWTROC with WATLEV=3 (covers/uncovers), no VALSOU -> UWTROC03
        let feature = make_uwtroc(None, Some(3));
        let settings = test_contours();
        let result = obstrn04(&feature, &settings, CsContext::EMPTY);
        assert_eq!(result.symbol, ObstrnSymbol::Uwtroc03);
    }

    #[test]
    fn test_uwtroc_with_depth() {
        // UWTROC with VALSOU > safety_contour, WATLEV=3 -> DANGER51
        let feature = make_uwtroc(Some(15.0), Some(3));
        let settings = test_contours();
        let result = obstrn04(&feature, &settings, CsContext::EMPTY);
        assert_eq!(result.symbol, ObstrnSymbol::Danger51);
        assert!(result.show_sounding);
    }

    #[test]
    fn test_isolated_danger() {
        // A 5 m obstruction inside water whose shallow limit is 10 m: shallower
        // than the safety contour, in otherwise safe water -> ISODGR51.
        let feature = make_obstrn(Some(5.0), Some(3));
        let settings = test_contours(); // safety_contour = 10.0
        let ctx = CsContext {
            area_drval1: vec![10.0],
            line_drval2: vec![],
        };
        let result = obstrn04(&feature, &settings, &ctx);
        assert_eq!(result.symbol, ObstrnSymbol::Isodgr51);
        assert!(result.is_isolated_danger);
    }

    #[test]
    fn test_shallow_obstruction_in_shallow_water_is_not_isolated() {
        // The same obstruction inside a 0-2 m area is part of that shallow
        // water, not an isolated danger — this is the case navcore used to get
        // wrong, before UDWHAZ03 could see its surroundings.
        let feature = make_obstrn(Some(5.0), Some(3));
        let settings = test_contours();
        let ctx = CsContext {
            area_drval1: vec![0.0],
            line_drval2: vec![],
        };
        let result = obstrn04(&feature, &settings, &ctx);
        assert_ne!(result.symbol, ObstrnSymbol::Isodgr51);
        assert!(!result.is_isolated_danger);
    }

    #[test]
    fn test_deep_obstruction() {
        // Deep obstruction (VALSOU > 20m) -> DANGER52
        let feature = make_obstrn(Some(25.0), Some(3));
        let settings = test_contours();
        let result = obstrn04(&feature, &settings, CsContext::EMPTY);
        assert_eq!(result.symbol, ObstrnSymbol::Danger52);
        assert!(result.show_sounding);
    }

    #[test]
    fn test_obstrn04_symbol() {
        let feature = make_obstrn(None, Some(4));
        let settings = test_contours();
        let symbol = obstrn04_symbol(&feature, &settings);
        assert_eq!(symbol, "OBSTRN03");
    }
}
