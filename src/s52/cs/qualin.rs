//! QUALIN01 - Quality of data line style procedure.
//!
//! Checks QUAPOS attribute on line features. If position quality
//! is low (QUAPOS 2-9), renders with dashed low-accuracy pattern.
//! Otherwise uses solid coastline styling.

use crate::s52::instruction::RenderInstruction;
use crate::senc::Feature;

/// Generate render instructions for QUALIN01 conditional symbology.
///
/// Matches OpenCPN's CSQUALIN01:
/// - QUAPOS 2-9: LC(LOWACC21) low-accuracy dashed pattern
/// - COALNE with CONRAD=1: double solid line
/// - Default: solid coastline line
pub fn qualin01_instructions(feature: &Feature) -> Vec<RenderInstruction> {
    // Check QUAPOS attribute
    let quapos = feature.attribute_int("QUAPOS").unwrap_or(0);

    if quapos >= 2 && quapos < 10 {
        // Low accuracy: use LC pattern
        return vec![RenderInstruction::LineComplex {
            name: "LOWACC21".to_string(),
        }];
    }

    // Check if coastline with construction
    let acronym = feature.object_class.acronym();
    if acronym == "COALNE" {
        let conrad = feature.attribute_int("CONRAD").unwrap_or(0);
        if conrad == 1 {
            // Construction coastline: double line
            return vec![
                RenderInstruction::LineStyle {
                    pattern: crate::s52::instruction::LinePattern::Solid,
                    width: 3,
                    color: "CHMGF".to_string(),
                },
                RenderInstruction::LineStyle {
                    pattern: crate::s52::instruction::LinePattern::Solid,
                    width: 1,
                    color: "CSTLN".to_string(),
                },
            ];
        }
    }

    // Default: solid coastline
    vec![RenderInstruction::LineStyle {
        pattern: crate::s52::instruction::LinePattern::Solid,
        width: 1,
        color: "CSTLN".to_string(),
    }]
}
