//! RESARE02 - Restricted Area Conditional Symbology Procedure
//!
//! Selects boundary style and symbol for restricted areas based on RESTRN attribute.
//! Reference: S-52 Annex A / OpenCPN s52cnsy.cpp RESARE02

use crate::s52::instruction::{LinePattern, RenderInstruction};
use crate::s52::MarinerSettings;
use crate::senc::Feature;

/// Execute RESARE02 conditional symbology.
///
/// Checks RESTRN (restriction) attribute values to determine:
/// - Boundary line style (dashed magenta for most restrictions)
/// - Whether to show entry prohibition symbol
///
/// RESTRN values (S-57):
///   1 = anchoring prohibited
///   2 = anchoring restricted
///   3 = fishing prohibited
///   4 = fishing restricted
///   5 = trawling prohibited
///   6 = trawling restricted
///   7 = entry prohibited
///   8 = entry restricted
///   9 = dredging prohibited
///  10 = dredging restricted
///  13 = no wake
///  14 = area to be avoided
pub fn resare02_instructions(feature: &Feature, _settings: &MarinerSettings) -> Vec<RenderInstruction> {
    let mut instructions = Vec::new();

    // Check RESTRN attribute (may be comma-separated list)
    let restrn_values = get_restrn_values(feature);

    let has_entry_prohibition = restrn_values.iter().any(|&v| v == 7); // entry prohibited
    let has_anchor_prohibition = restrn_values.iter().any(|&v| v == 1); // anchoring prohibited
    let has_fishing_prohibition = restrn_values.iter().any(|&v| v == 3); // fishing prohibited

    // Symbol selection based on restriction type
    if has_entry_prohibition {
        instructions.push(RenderInstruction::Symbol {
            name: "ENTRES51".to_string(),
        });
    } else if has_anchor_prohibition {
        instructions.push(RenderInstruction::Symbol {
            name: "ACHRES51".to_string(),
        });
    } else if has_fishing_prohibition {
        instructions.push(RenderInstruction::Symbol {
            name: "FSHRES51".to_string(),
        });
    } else {
        instructions.push(RenderInstruction::Symbol {
            name: "CTYARE51".to_string(),
        });
    }

    // Boundary line: magenta dashed for all restricted areas
    instructions.push(RenderInstruction::LineStyle {
        pattern: LinePattern::Dashed,
        width: 2,
        color: "CHMGD".to_string(),
    });

    instructions
}

/// Extract RESTRN attribute values from feature.
/// RESTRN can be a single integer or a comma-separated list.
fn get_restrn_values(feature: &Feature) -> Vec<i32> {
    // Try integer attribute first
    if let Some(v) = feature.attribute_int("RESTRN") {
        return vec![v];
    }

    // Try string attribute (comma-separated list)
    if let Some(s) = feature.attribute_str("RESTRN") {
        return s.split(',')
            .filter_map(|v| v.trim().parse::<i32>().ok())
            .collect();
    }

    Vec::new()
}
