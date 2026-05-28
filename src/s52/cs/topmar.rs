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
pub fn topmar01_instructions(
    feature: &Feature,
    _settings: &MarinerSettings,
) -> Vec<RenderInstruction> {
    let topshp = feature.attribute_int("TOPSHP");

    // OpenCPN TOPMAR01 uses spatial lookup (_atPtPos) to decide floating vs fixed.
    // We approximate: some SENC encoders stamp a synthetic "is_floating" boolean,
    // others set a "PLATFORM" integer (1=floating, 0=fixed). Default to floating
    // since buoy-mounted topmarks are overwhelmingly the common case.
    let floating = feature
        .attribute_int("PLATFORM")
        .map(|v| v != 0)
        .unwrap_or(true);

    let name = match topshp {
        None => "QUESMRK1",
        Some(shp) => {
            if floating {
                floating_platform_symbol(shp)
            } else {
                fixed_platform_symbol(shp)
            }
        }
    };

    vec![RenderInstruction::Symbol {
        name: name.to_string(),
    }]
}

/// TOPSHP → symbol mapping for floating platforms (buoys).
/// Mirrors `TOPMAR01` floating branch in `s52cnsy.cpp:2962-3068`.
fn floating_platform_symbol(topshp: i32) -> &'static str {
    match topshp {
        1 => "TOPMAR02",
        2 => "TOPMAR04",
        3 => "TOPMAR10",
        4 => "TOPMAR12",
        5 => "TOPMAR13",
        6 => "TOPMAR14",
        7 => "TOPMAR65",
        8 => "TOPMAR17",
        9 => "TOPMAR16",
        10 => "TOPMAR08",
        11 => "TOPMAR07",
        12 => "TOPMAR14",
        13 => "TOPMAR05",
        14 => "TOPMAR06",
        17 => "TMARDEF2",
        18 => "TOPMAR10",
        19 => "TOPMAR13",
        20 => "TOPMAR14",
        21 => "TOPMAR13",
        22 => "TOPMAR14",
        23 => "TOPMAR14",
        24 => "TOPMAR02",
        25 => "TOPMAR04",
        26 => "TOPMAR10",
        27 => "TOPMAR17",
        28 => "TOPMAR18",
        29 => "TOPMAR02",
        30 => "TOPMAR17",
        31 => "TOPMAR14",
        32 => "TOPMAR10",
        _ => "TMARDEF2",
    }
}

/// TOPSHP → symbol mapping for fixed platforms (beacons).
/// Mirrors `TOPMAR01` non-floating branch in `s52cnsy.cpp:3071-3180`.
fn fixed_platform_symbol(topshp: i32) -> &'static str {
    match topshp {
        1 => "TOPMAR22",
        2 => "TOPMAR24",
        3 => "TOPMAR30",
        4 => "TOPMAR32",
        5 => "TOPMAR33",
        6 => "TOPMAR34",
        7 => "TOPMAR85",
        8 => "TOPMAR86",
        9 => "TOPMAR36",
        10 => "TOPMAR28",
        11 => "TOPMAR27",
        12 => "TOPMAR14",
        13 => "TOPMAR25",
        14 => "TOPMAR26",
        15 => "TOPMAR88",
        16 => "TOPMAR87",
        17 => "TMARDEF1",
        18 => "TOPMAR30",
        19 => "TOPMAR33",
        20 => "TOPMAR34",
        21 => "TOPMAR33",
        22 => "TOPMAR34",
        23 => "TOPMAR34",
        24 => "TOPMAR22",
        25 => "TOPMAR24",
        26 => "TOPMAR30",
        27 => "TOPMAR86",
        28 => "TOPMAR89",
        29 => "TOPMAR22",
        30 => "TOPMAR86",
        31 => "TOPMAR14",
        32 => "TOPMAR30",
        _ => "TMARDEF1",
    }
}
