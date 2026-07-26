//! RESTRN01 - Restriction symbology procedure.
//!
//! Symbolizes restriction types on navigation features. Ranks restrictions
//! by navigational significance: entry > anchoring > fishing > other.
//! Matches OpenCPN's _RESCSP01 implementation.

use crate::s52::instruction::RenderInstruction;
use crate::senc::Feature;

/// Classify a single RESTRN value.
fn classify_val(val: u32) -> (bool, bool, bool, bool) {
    match val {
        7 | 8 | 14 => (true, false, false, false),
        1 | 2 => (false, true, false, false),
        3 | 4 | 5 | 6 => (false, false, true, false),
        9 | 10 | 11 | 12 | 13 => (false, false, false, true),
        _ => (false, false, false, false),
    }
}

/// Categories of restrictions, ranked by navigational significance.
fn classify_restrn(feature: &Feature) -> (bool, bool, bool, bool) {
    let mut has_entry = false;
    let mut has_anchoring = false;
    let mut has_fishing = false;
    let mut has_other = false;

    // Try string attribute first (list attributes: "1,2" or "7")
    if let Some(restrn_str) = feature.attribute_str("RESTRN") {
        for val in restrn_str.split(',') {
            if let Ok(v) = val.trim().parse::<u32>() {
                let (e, a, f, o) = classify_val(v);
                has_entry |= e;
                has_anchoring |= a;
                has_fishing |= f;
                has_other |= o;
            }
        }
    } else if let Some(v) = feature.attribute_int("RESTRN") {
        // Single integer value fallback
        let (e, a, f, o) = classify_val(v as u32);
        has_entry = e;
        has_anchoring = a;
        has_fishing = f;
        has_other = o;
    }

    (has_entry, has_anchoring, has_fishing, has_other)
}

/// Select restriction symbol name based on RESTRN attribute values.
///
/// Implements the OpenCPN _RESCSP01 priority algorithm:
/// Entry > Anchoring > Fishing > Other, with compound symbols
/// when multiple restriction types are present.
fn restriction_symbol(feature: &Feature) -> &'static str {
    let (has_entry, has_anchoring, has_fishing, has_other) = classify_restrn(feature);

    if has_entry {
        if has_anchoring || has_fishing {
            "ENTRES61" // Entry + anchoring/fishing
        } else if has_other {
            "ENTRES71" // Entry + other
        } else {
            "ENTRES51" // Entry only
        }
    } else if has_anchoring {
        if has_fishing {
            "ACHRES61" // Anchoring + fishing
        } else if has_other {
            "ACHRES71" // Anchoring + other
        } else {
            "ACHRES51" // Anchoring only
        }
    } else if has_fishing {
        if has_other {
            "FSHRES71" // Fishing + other
        } else {
            "FSHRES51" // Fishing only
        }
    } else if has_other {
        "INFARE51" // Other restrictions (dredging, diving, no wake)
    } else {
        "RSRDEF51" // Default/unrecognized restriction
    }
}

/// Generate render instructions for RESTRN01 conditional symbology.
///
/// RESTRN01 is only a signpost for RESCSP01: with no RESTRN attribute there is
/// nothing to symbolise and the procedure returns nothing (s52cnsy.cpp
/// RESTRN01 returns NULL when GetStringAttrWXS(obj, "RESTRN") is NULL). Falling
/// through to the RSRDEF51 default instead put a spurious magenta restriction
/// symbol on every unrestricted OSPARE/DMPGRD/MIPARE/MARCUL/TSSLPT area.
pub fn restrn01_instructions(feature: &Feature) -> Vec<RenderInstruction> {
    if feature.attribute_str("RESTRN").is_none() && feature.attribute_int("RESTRN").is_none() {
        return Vec::new();
    }
    let sym_name = restriction_symbol(feature);
    vec![RenderInstruction::Symbol {
        name: sym_name.to_string(),
        rotation: None,
    }]
}
