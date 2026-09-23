//! WRECKS02 - Wreck Conditional Symbology Procedure
//!
//! Selects appropriate wreck symbols based on depth (VALSOU), water level (WATLEV),
//! category (CATWRK), and safety contour settings.
//!
//! OpenCPN reference: s52cnsy.cpp:3239-3500

use crate::s52::instruction::{LinePattern, RenderInstruction};
use crate::s52::cs::CsContext;
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
    /// Danger symbol for a sounded wreck shallower than the safety contour
    Danger51,
    /// Danger symbol for a sounded wreck at or beyond the safety contour
    Danger52,
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
            Self::Danger51 => "DANGER51",
            Self::Danger52 => "DANGER52",
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
/// UDWHAZ03, shared with OBSTRN04 — see the note in `obstrn.rs`. A wreck is an
/// isolated danger only when it is shallow *and* the water around it is deep
/// enough that a mariner would otherwise pass over it safely.
fn is_isolated_danger(
    depth_value: Option<f64>,
    expsou: i32,
    settings: &MarinerSettings,
    ctx: &CsContext,
) -> bool {
    let safety = settings.safety_contour as f64;
    match depth_value {
        None => expsou != 1,
        Some(depth) => {
            if expsou != 1 && depth > safety {
                return false;
            }
            ctx.indicates_danger(safety, expsou)
        }
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
pub fn wrecks02(
    feature: &Feature,
    settings: &MarinerSettings,
    ctx: &CsContext,
) -> Wrecks02Result {
    let valsou = feature.attribute_float("VALSOU");
    let watlev = feature.attribute_int("WATLEV").unwrap_or(0);
    let catwrk = feature.attribute_int("CATWRK").unwrap_or(0);
    let _expsou = feature.attribute_int("EXPSOU").unwrap_or(0);
    let quasou = parse_quasou(feature.attribute_str("QUASOU"));

    // Check for QUASOU=7 (least depth unknown, safe clearance at value shown)
    let has_quasou_7 = quasou.contains(&7);

    // Depth used for the danger assessment. s52cnsy.cpp WRECKS02 prefers
    // VALSOU, then infers from CATWRK — a wreck recorded as "non-dangerous"
    // stands in for 20 m of clearance — and only falls back to WATLEV when the
    // category is absent. Deriving from WATLEV alone made every covering wreck
    // look 1 cm deep, and therefore an isolated danger.
    let depth_value = if let Some(v) = valsou {
        Some(v)
    } else if catwrk != 0 {
        Some(match catwrk {
            1 => 20.0,       // non-dangerous
            2 => 0.0,        // dangerous
            4 | 5 => -15.0,  // hull / mast showing
            _ => -15.0,
        })
    } else {
        match watlev {
            3 => Some(0.01), // covers and uncovers
            5 => Some(0.0),  // subject to inundation
            _ => Some(-15.0),
        }
    };

    // Check for isolated danger (UDWHAZ03) - skip if QUASOU=7
    let expsou = feature.attribute_int("EXPSOU").unwrap_or(0);
    if !has_quasou_7 && is_isolated_danger(depth_value, expsou, settings, ctx) {
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

/// Generate render instructions for WRECKS02.
///
/// Follows the two continuations of s52cnsy.cpp WRECKS02. A wreck *with* a
/// sounding is drawn as a danger symbol plus the literal text "Wk" plus its
/// depth — not as one of the WRECKS0x pictorial symbols, which are only for
/// wrecks whose depth is unknown.
///
/// Deviation: OpenCPN appends SNDFRM02's digit symbols; navcore emits the
/// sounding as text, as it does for soundings everywhere else.
pub fn wrecks02_instructions(
    feature: &Feature,
    settings: &MarinerSettings,
    ctx: &CsContext,
) -> Vec<RenderInstruction> {
    let result = wrecks02(feature, settings, ctx);
    let valsou = feature.attribute_float("VALSOU");
    let watlev = feature.attribute_int("WATLEV");
    let catwrk = feature.attribute_int("CATWRK");
    let quasou = parse_quasou(feature.attribute_str("QUASOU"));
    let safety = settings.safety_contour as f64;
    let mut out = Vec::new();

    if feature.feature_type == FeatureType::Area {
        // Continuation B
        let quapos = feature.attribute_int("QUAPOS").unwrap_or(0);
        if (2..10).contains(&quapos) {
            out.push(RenderInstruction::LineComplex {
                name: "LOWACC41".to_string(),
            });
        } else if result.is_isolated_danger {
            out.push(line(LinePattern::Dotted, "CHBLK"));
        } else if let Some(v) = valsou {
            out.push(line(
                if v <= 20.0 {
                    LinePattern::Dotted
                } else {
                    LinePattern::Dashed
                },
                "CHBLK",
            ));
        } else {
            out.push(match watlev {
                Some(1) | Some(2) => line(LinePattern::Solid, "CSTLN"),
                Some(4) => line(LinePattern::Dashed, "CSTLN"),
                _ => line(LinePattern::Dotted, "CSTLN"),
            });
        }

        match valsou {
            Some(v) => {
                if result.is_isolated_danger {
                    out.push(RenderInstruction::Symbol {
                        name: WrecksSymbol::Isodgr51.symbol_name().to_string(),
                        rotation: None,
                    });
                }
                if v <= 20.0 {
                    out.push(sounding_text(v));
                }
            }
            None => {
                out.push(RenderInstruction::AreaColor {
                    color: match watlev {
                        Some(1) | Some(2) => "CHBRN",
                        Some(4) => "DEPIT",
                        _ => "DEPVS",
                    }
                    .to_string(),
                    transparency: None,
                });
                if result.is_isolated_danger {
                    out.push(RenderInstruction::Symbol {
                        name: WrecksSymbol::Isodgr51.symbol_name().to_string(),
                        rotation: None,
                    });
                }
            }
        }
        return out;
    }

    // Continuation A (point)
    if result.is_isolated_danger {
        out.push(RenderInstruction::Symbol {
            name: WrecksSymbol::Isodgr51.symbol_name().to_string(),
            rotation: None,
        });
        return out;
    }

    match valsou {
        Some(v) => {
            out.push(RenderInstruction::Symbol {
                name: if v < safety {
                    WrecksSymbol::Danger51
                } else {
                    WrecksSymbol::Danger52
                }
                .symbol_name()
                .to_string(),
                rotation: None,
            });
            out.push(RenderInstruction::Text {
                attribute: "Wk".to_string(),
                format: None,
                hjust: 3,
                vjust: 1,
                xoffs: 2,
                yoffs: 0,
                color: "CHBLK".to_string(),
                style: 1,
                weight: 5,
                width: 1,
                bsize: 10,
                // s52cnsy.cpp: "TX('Wk',3,1,2,'15110',2,0,CHBLK,21)".
                space: 2,
                dis: 21,
            });
            if quasou.contains(&7) {
                out.push(RenderInstruction::Symbol {
                    name: WrecksSymbol::Wrecks07.symbol_name().to_string(),
                    rotation: None,
                });
            }
            out.push(sounding_text(v));
        }
        None => {
            // Pictorial wreck symbols, and only when both attributes are
            // present — s52cnsy.cpp leaves the symbol empty otherwise.
            if let (Some(cw), Some(wl)) = (catwrk, watlev) {
                let sym = match (cw, wl) {
                    (1, 3) => WrecksSymbol::Wrecks04,
                    (2, 3) => WrecksSymbol::Wrecks05,
                    (4, _) | (5, _) => WrecksSymbol::Wrecks01,
                    (_, 1) | (_, 2) | (_, 4) | (_, 5) => WrecksSymbol::Wrecks01,
                    _ => WrecksSymbol::Wrecks05,
                };
                out.push(RenderInstruction::Symbol {
                    name: sym.symbol_name().to_string(),
                    rotation: None,
                });
            }
        }
    }

    out
}

fn line(pattern: LinePattern, color: &str) -> RenderInstruction {
    RenderInstruction::LineStyle {
        pattern,
        width: 2,
        color: color.to_string(),
    }
}

/// The sounding value drawn beside a wreck.
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

/// Get the symbol name for a wreck feature
pub fn wrecks02_symbol(feature: &Feature, settings: &MarinerSettings) -> &'static str {
    wrecks02(feature, settings, CsContext::EMPTY).symbol.symbol_name()
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

    fn make_wreck(valsou: Option<f64>, watlev: Option<i32>, catwrk: Option<i32>) -> Feature {
        let mut attributes = crate::senc::Attributes::new();
        if let Some(v) = valsou {
            attributes.insert("VALSOU", AttributeValue::Float(v));
        }
        if let Some(w) = watlev {
            attributes.insert("WATLEV", AttributeValue::Integer(w));
        }
        if let Some(c) = catwrk {
            attributes.insert("CATWRK", AttributeValue::Integer(c));
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
        let settings = test_contours();
        let result = wrecks02(&feature, &settings, CsContext::EMPTY);
        assert_eq!(result.symbol, WrecksSymbol::Wrecks01);
    }

    #[test]
    fn test_wreck_dangerous() {
        // CATWRK=2 (dangerous) with shallow depth
        let feature = make_wreck(Some(5.0), Some(3), Some(2));
        let settings = test_contours();
        // Shallower than the safety contour AND lying in otherwise safe water.
        let ctx = CsContext {
            area_drval1: vec![10.0],
            line_drval2: vec![],
            ..Default::default()
        };
        let result = wrecks02(&feature, &settings, &ctx);
        assert_eq!(result.symbol, WrecksSymbol::Isodgr51);
        assert!(result.is_isolated_danger);
    }

    #[test]
    fn test_wreck_mast_showing() {
        // CATWRK=5 (mast showing) with deep depth (> safety contour)
        let feature = make_wreck(Some(15.0), Some(4), Some(5));
        let settings = test_contours(); // safety_contour = 10.0
        let result = wrecks02(&feature, &settings, CsContext::EMPTY);
        assert_eq!(result.symbol, WrecksSymbol::Wrecks05);
    }

    #[test]
    fn test_wreck_quasou_7() {
        // QUASOU=7 (least depth unknown) gets special symbol
        let mut feature = make_wreck(Some(5.0), Some(3), None);
        feature.attributes.insert("QUASOU",
            AttributeValue::String("7".to_string()),
        );
        let settings = test_contours();
        let result = wrecks02(&feature, &settings, CsContext::EMPTY);
        assert_eq!(result.symbol, WrecksSymbol::Wrecks07);
    }

    #[test]
    fn test_wreck_covers_uncovers() {
        // WATLEV=3 (covers/uncovers) with depth > safety_contour
        let feature = make_wreck(Some(15.0), Some(3), None);
        let settings = test_contours(); // safety_contour = 10.0
        let result = wrecks02(&feature, &settings, CsContext::EMPTY);
        assert_eq!(result.symbol, WrecksSymbol::Wrecks04);
    }

    #[test]
    fn test_wreck_always_dry() {
        // WATLEV=1 (always dry)
        let feature = make_wreck(None, Some(1), None);
        let settings = test_contours();
        let result = wrecks02(&feature, &settings, CsContext::EMPTY);
        assert_eq!(result.symbol, WrecksSymbol::Wrecks01);
    }

    #[test]
    fn test_wreck_deep() {
        // Deep wreck (> 20m) is non-dangerous
        let feature = make_wreck(Some(30.0), Some(3), None);
        let settings = test_contours();
        let result = wrecks02(&feature, &settings, CsContext::EMPTY);
        assert_eq!(result.symbol, WrecksSymbol::Wrecks01);
        assert!(result.show_sounding);
    }

    #[test]
    fn test_wrecks02_symbol() {
        let feature = make_wreck(None, Some(3), Some(4));
        let settings = test_contours();
        let symbol = wrecks02_symbol(&feature, &settings);
        assert_eq!(symbol, "WRECKS04");
    }
}
