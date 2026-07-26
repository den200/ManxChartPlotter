//! RESARE02 - Restricted Area Conditional Symbology Procedure
//!
//! A RESARE may carry several categories at once (an inshore traffic zone that
//! is also a bird sanctuary and a mine field), so the procedure ranks the
//! restrictions and symbolises only the most significant one, indicating the
//! rest with a "!" subscript variant of the symbol.
//!
//! Reference: S-52 Annex A / OpenCPN s52cnsy.cpp RESARE02.

use crate::s52::instruction::{LinePattern, RenderInstruction};
use crate::s52::MarinerSettings;
use crate::senc::Feature;

/// RESTRN: entry prohibited/restricted, and IMO "area to be avoided".
const RESTRN_ENTRY: &[i32] = &[7, 8, 14];
/// RESTRN: anchoring prohibited/restricted.
const RESTRN_ANCHOR: &[i32] = &[1, 2];
/// RESTRN: fishing/trawling prohibited/restricted.
const RESTRN_FISHING: &[i32] = &[3, 4, 5, 6];
/// RESTRN: dredging, diving, no-wake — the "other restriction" band.
const RESTRN_OTHER: &[i32] = &[9, 10, 11, 12, 13];

/// CATREA values that rank as significant restrictions in their own right.
/// s52cnsy.cpp writes these as octal escapes — \001\010\011\014\016\023\025\031
/// is 1,8,9,12,14,19,21,25, not 1,10,11,14,16,23,25,31.
const CATREA_MAJOR: &[i32] = &[1, 8, 9, 12, 14, 19, 21, 25];
/// CATREA values that rank as secondary information
/// (\004\005\006\007\012\022\024\026\027\030).
const CATREA_MINOR: &[i32] = &[4, 5, 6, 7, 10, 18, 20, 22, 23, 24];

/// Execute RESARE02 conditional symbology.
pub fn resare02_instructions(
    feature: &Feature,
    settings: &MarinerSettings,
) -> Vec<RenderInstruction> {
    let restrn = list_values(feature, "RESTRN");
    let catrea = list_values(feature, "CATREA");

    let restrn_has = |set: &[i32]| restrn.iter().any(|v| set.contains(v));
    let catrea_has = |set: &[i32]| catrea.iter().any(|v| set.contains(v));

    // (symbol, boundary line complex used when symbolized boundaries are on)
    let (symbol, line_complex) = if !restrn.is_empty() {
        if restrn_has(RESTRN_ENTRY) {
            // Continuation A
            let symb = if restrn_has(RESTRN_ANCHOR) || restrn_has(RESTRN_FISHING) {
                "ENTRES61"
            } else if catrea_has(CATREA_MAJOR) {
                "ENTRES61"
            } else if restrn_has(RESTRN_OTHER) || catrea_has(CATREA_MINOR) {
                "ENTRES71"
            } else {
                "ENTRES51"
            };
            (symb, "RESARE51")
        } else if restrn_has(RESTRN_ANCHOR) {
            // Continuation B
            let symb = if restrn_has(RESTRN_FISHING) {
                "ACHRES61"
            } else if catrea_has(CATREA_MAJOR) {
                "ACHRES61"
            } else if restrn_has(RESTRN_OTHER) || catrea_has(CATREA_MINOR) {
                "ACHRES71"
            } else {
                "RESTRN51"
            };
            (symb, "RESARE51")
        } else if restrn_has(RESTRN_FISHING) {
            // Continuation C
            let symb = if catrea_has(CATREA_MAJOR) {
                "FSHRES51"
            } else if restrn_has(RESTRN_OTHER) || catrea_has(CATREA_MINOR) {
                "FSHRES71"
            } else {
                "FSHRES51"
            };
            (symb, "FSHRES51")
        } else {
            let symb = if restrn_has(RESTRN_OTHER) {
                "INFARE51"
            } else {
                "RSRDEF51"
            };
            (symb, "CTYARE51")
        }
    } else {
        // Continuation D — no RESTRN, symbolise from CATREA alone
        let symb = if catrea.is_empty() {
            "RSRDEF51"
        } else if catrea_has(CATREA_MAJOR) {
            if catrea_has(CATREA_MINOR) {
                "CTYARE71"
            } else {
                "CTYARE51"
            }
        } else if catrea_has(CATREA_MINOR) {
            "INFARE51"
        } else {
            "RSRDEF51"
        };
        (symb, "CTYARE51")
    };

    // OpenCPN assembles the command word as priority, then line, then symbol.
    // The OP(6---) override it computes for the entry/anchoring/fishing
    // branches is deliberately not emitted here: StringToRules has no "OP"
    // instruction, so OpenCPN parses straight past it and the override never
    // reaches its renderer either.
    let mut out = Vec::with_capacity(2);
    if settings.symbolized_boundaries {
        out.push(RenderInstruction::LineComplex {
            name: line_complex.to_string(),
        });
    } else {
        out.push(RenderInstruction::LineStyle {
            pattern: LinePattern::Dashed,
            width: 2,
            color: "CHMGD".to_string(),
        });
    }
    out.push(RenderInstruction::Symbol {
        name: symbol.to_string(),
        rotation: None,
    });
    out
}

/// Read an S-57 list attribute as integers. Values arrive either as a single
/// integer or as the comma-separated string the SENC stores for 'L' types.
fn list_values(feature: &Feature, name: &str) -> Vec<i32> {
    if let Some(s) = feature.attribute_str(name) {
        return s
            .split(',')
            .filter_map(|v| v.trim().parse::<i32>().ok())
            .collect();
    }
    if let Some(v) = feature.attribute_int(name) {
        return vec![v];
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::senc::{AttributeValue, FeatureType, ObjectClass};

    fn resare(attrs: &[(&str, &str)]) -> Feature {
        let mut attributes = crate::senc::Attributes::new();
        for (k, v) in attrs {
            attributes.insert(k, AttributeValue::String(v.to_string()));
        }
        Feature {
            type_code: 0,
            object_class: ObjectClass::RestrictedArea,
            feature_type: FeatureType::Area,
            attributes,
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        }
    }

    fn symbol_of(instrs: &[RenderInstruction]) -> &str {
        instrs
            .iter()
            .find_map(|i| match i {
                RenderInstruction::Symbol { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .expect("expected a symbol")
    }

    #[test]
    fn line_precedes_symbol() {
        let f = resare(&[("RESTRN", "7")]);
        let out = resare02_instructions(&f, &MarinerSettings::default());
        assert!(matches!(out[0], RenderInstruction::LineStyle { .. }));
        assert!(matches!(out[1], RenderInstruction::Symbol { .. }));
    }

    #[test]
    fn entry_alone_is_entres51() {
        let f = resare(&[("RESTRN", "7")]);
        let out = resare02_instructions(&f, &MarinerSettings::default());
        assert_eq!(symbol_of(&out), "ENTRES51");
    }

    #[test]
    fn entry_with_minor_catrea_is_entres71() {
        // The case the conformance harness caught: CATREA 4 is a minor
        // category, which upgrades the plain entry symbol to the "!" variant.
        let f = resare(&[("RESTRN", "7"), ("CATREA", "4")]);
        let out = resare02_instructions(&f, &MarinerSettings::default());
        assert_eq!(symbol_of(&out), "ENTRES71");
    }

    #[test]
    fn entry_with_major_catrea_is_entres61() {
        let f = resare(&[("RESTRN", "7"), ("CATREA", "1")]);
        let out = resare02_instructions(&f, &MarinerSettings::default());
        assert_eq!(symbol_of(&out), "ENTRES61");
    }

    #[test]
    fn anchoring_alone_is_restrn51() {
        let f = resare(&[("RESTRN", "1")]);
        let out = resare02_instructions(&f, &MarinerSettings::default());
        assert_eq!(symbol_of(&out), "RESTRN51");
    }

    #[test]
    fn no_restrn_uses_catrea_only() {
        let f = resare(&[("CATREA", "4,5")]);
        let out = resare02_instructions(&f, &MarinerSettings::default());
        assert_eq!(symbol_of(&out), "INFARE51");
    }

    #[test]
    fn no_attributes_is_default() {
        let f = resare(&[]);
        let out = resare02_instructions(&f, &MarinerSettings::default());
        assert_eq!(symbol_of(&out), "RSRDEF51");
    }
}
