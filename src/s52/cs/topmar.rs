//! TOPMAR01 - Topmark Conditional Symbology Procedure
//!
//! Selects topmark symbol based on TOPSHP (topmark shape) attribute.
//! Reference: S-52 Annex A / OpenCPN s52cnsy.cpp TOPMAR01

use crate::s52::instruction::RenderInstruction;
use crate::s52::MarinerSettings;
use crate::senc::Feature;

/// Execute TOPMAR01 conditional symbology.
///
/// Selects topmark symbol based on TOPSHP attribute:
///   1 = cone, point up         → TOPMAR02
///   2 = cone, point down       → TOPMAR04
///   3 = sphere                 → TOPMAR10
///   4 = 2 spheres              → TOPMAR12
///   5 = cylinder (can)         → TOPMAR13
///   6 = board                  → TOPMAR14
///   7 = X-shape (St. Andrew's) → TOPMAR65
///   8 = upright cross (+)      → TOPMAR17
///   9 = cube, point up         → TOPMAR16
///  10 = 2 cones, point up      → TOPMAR08
///  11 = 2 cones, base to base  → TOPMAR07
///  12 = 2 cones, point to point→ TOPMAR06
///  13 = 2 cones, points down   → TOPMAR05
///  14 = 2 cones, up + sphere   → TOPMAR01 (as a fallback)
pub fn topmar01_instructions(feature: &Feature, _settings: &MarinerSettings) -> Vec<RenderInstruction> {
    let topshp = feature.attribute_int("TOPSHP").unwrap_or(0);

    let symbol_name = match topshp {
        1 => "TOPMAR02",   // cone, point up
        2 => "TOPMAR04",   // cone, point down
        3 => "TOPMAR10",   // sphere
        4 => "TOPMAR12",   // 2 spheres
        5 => "TOPMAR13",   // cylinder (can)
        6 => "TOPMAR14",   // board
        7 => "TOPMAR65",   // X-shape
        8 => "TOPMAR17",   // upright cross
        9 => "TOPMAR16",   // cube, point up
        10 => "TOPMAR08",  // 2 cones, point up
        11 => "TOPMAR07",  // 2 cones, base to base
        12 => "TOPMAR06",  // 2 cones, point to point
        13 => "TOPMAR05",  // 2 cones, points down
        _ => "TOPMAR01",   // default topmark
    };

    vec![RenderInstruction::Symbol {
        name: symbol_name.to_string(),
    }]
}
