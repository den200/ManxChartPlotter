//! S-52 Day_Bright color palette and line style lookup.
//!
//! This module provides S-52 compliant colors and line styles for nautical chart rendering.
//! Colors are from the Day_Bright palette, converted to 0-1 float RGBA.
//! Line styles follow S-52 LS() specifications with proper dash/dot patterns.

use super::state::LineStyle;
use crate::s52::{LinePattern, LineStyleKey, LookupTables};

pub type Color = [f32; 4];

// ============================================================
// S-52 Day_Bright Color Tokens
// RGB values from OpenCPN S-52 implementation, converted to 0-1 floats
// ============================================================

/// CSTLN - Coastline (82, 90, 92)
pub const CSTLN: Color = [0.322, 0.353, 0.361, 1.0];

/// DEPCN - Depth contour (125, 137, 140)
pub const DEPCN: Color = [0.490, 0.537, 0.549, 1.0];

/// DEPSC - Safety contour (82, 90, 92) - same as CSTLN
pub const DEPSC: Color = [0.322, 0.353, 0.361, 1.0];

/// CHBLK - Chart black (7, 7, 7)
pub const CHBLK: Color = [0.027, 0.027, 0.027, 1.0];

/// CHGRD - Chart gray dark (125, 137, 140) - same as DEPCN
pub const CHGRD: Color = [0.490, 0.537, 0.549, 1.0];

/// CHGRF - Chart gray light (163, 180, 183)
pub const CHGRF: Color = [0.639, 0.706, 0.718, 1.0];

/// CHMGD - Chart magenta dark (197, 69, 195)
pub const CHMGD: Color = [0.773, 0.271, 0.765, 1.0];

/// CHMGF - Chart magenta light (211, 166, 233)
pub const CHMGF: Color = [0.827, 0.651, 0.914, 1.0];

/// CHBRN - Chart brown (177, 145, 57)
pub const CHBRN: Color = [0.694, 0.569, 0.224, 1.0];

/// OUTLW - Outline black (7, 7, 7) - same as CHBLK
pub const OUTLW: Color = [0.027, 0.027, 0.027, 1.0];

/// LITRD - Light red (241, 84, 105)
pub const LITRD: Color = [0.945, 0.329, 0.412, 1.0];

/// LITGN - Light green (104, 228, 86)
pub const LITGN: Color = [0.408, 0.894, 0.337, 1.0];

/// LITYW - Light yellow (244, 218, 72)
pub const LITYW: Color = [0.957, 0.855, 0.282, 1.0];

/// LANDF - Land fill dark (139, 102, 31)
pub const LANDF: Color = [0.545, 0.400, 0.122, 1.0];

/// LANDA - Land fill light (201, 185, 122)
pub const LANDA: Color = [0.788, 0.725, 0.478, 1.0];

/// TRFCD - Traffic dark (197, 69, 195) - same as CHMGD
pub const TRFCD: Color = [0.773, 0.271, 0.765, 1.0];

/// TRFCF - Traffic light (211, 166, 233) - same as CHMGF
pub const TRFCF: Color = [0.827, 0.651, 0.914, 1.0];

/// CURSR - Cursor/range (235, 125, 54)
pub const CURSR: Color = [0.922, 0.490, 0.212, 1.0];

/// SNDG1 - Sounding color 1 (125, 137, 140) - same as DEPCN/CHGRD
/// Used for normal soundings per SNDFRM02
pub const SNDG1: Color = [0.490, 0.537, 0.549, 1.0];

/// SNDG2 - Sounding color 2 (7, 7, 7) - same as CHBLK
/// Used for safety-critical soundings (depth < safety_depth)
pub const SNDG2: Color = [0.027, 0.027, 0.027, 1.0];

// ============================================================
// S-52 Pattern Constants
// PPMM = pixels per millimeter, assuming 96 DPI standard display
// ============================================================

/// Pixels per millimeter at 96 DPI (96 / 25.4)
const PPMM: f32 = 4.0;

// DASH pattern: 3mm period, 0.66 on fraction
const DASH_PERIOD: f32 = 3.0 * PPMM;      // 12 px
const DASH_ON: f32 = DASH_PERIOD * 0.66;  // ~8 px
const DASH_OFF: f32 = DASH_PERIOD * 0.34; // ~4 px

// DOT pattern: 1mm period, 0.5 on fraction
const DOT_PERIOD: f32 = 1.0 * PPMM;       // 4 px
const DOT_ON: f32 = DOT_PERIOD * 0.5;     // 2 px
const DOT_OFF: f32 = DOT_PERIOD * 0.5;    // 2 px

/// Convert S-52 width units to pixels.
/// OpenCPN uses width values directly as pixel widths (not 0.3mm per unit from spec K.3).
/// This matches OpenCPN's RenderLS behavior: `glLineWidth(wxMax(m_GLMinCartographicLineWidth, w))`
const fn width(w: u8) -> f32 {
    if w == 0 { 1.0 } else { w as f32 }
}

// ============================================================
// Precomputed Line Styles
// Each constant corresponds to an S-52 LS() specification
// ============================================================

/// COALNE: LS(SOLD,1,CSTLN) - Coastline: solid, width 1, gray
/// Per S52-RENDERING-SPEC.md Appendix N: coastlines are SOLID, not dashed
pub const COALNE: LineStyle = LineStyle {
    color: CSTLN,
    width_px: width(1),
    dash_on_px: 0.0,
    dash_off_px: 0.0,
};

/// DEPCNT normal: LS(SOLD,1,DEPCN) - Depth contour: solid, width 1, gray-blue
pub const DEPCNT: LineStyle = LineStyle {
    color: DEPCN,
    width_px: width(1),
    dash_on_px: 0.0,
    dash_off_px: 0.0,
};

/// DEPCNT safety: LS(SOLD,2,DEPSC) - Safety contour: solid, width 2, dark gray
pub const DEPCNT_SAFETY: LineStyle = LineStyle {
    color: DEPSC,
    width_px: width(2),
    dash_on_px: 0.0,
    dash_off_px: 0.0,
};

/// DEPCNT low accuracy: LS(DASH,1,DEPCN) - Low accuracy contour: dashed
pub const DEPCNT_LOWACC: LineStyle = LineStyle {
    color: DEPCN,
    width_px: width(1),
    dash_on_px: DASH_ON,
    dash_off_px: DASH_OFF,
};

/// SLCONS default: LS(SOLD,2,CSTLN) - Shoreline construction: solid, width 2
pub const SLCONS: LineStyle = LineStyle {
    color: CSTLN,
    width_px: width(2),
    dash_on_px: 0.0,
    dash_off_px: 0.0,
};

/// SLCONS wharf: LS(SOLD,4,CSTLN) - Wharf/pier: solid, width 4
pub const SLCONS_WHARF: LineStyle = LineStyle {
    color: CSTLN,
    width_px: width(4),
    dash_on_px: 0.0,
    dash_off_px: 0.0,
};

/// RIVBNK: LS(DOTT,2,CSTLN) - River bank: dotted, width 2
pub const RIVBNK: LineStyle = LineStyle {
    color: CSTLN,
    width_px: width(2),
    dash_on_px: DOT_ON,
    dash_off_px: DOT_OFF,
};

/// CBLOHD: LS(DASH,4,CHGRD) - Overhead cable: dashed, width 4, gray
pub const CBLOHD: LineStyle = LineStyle {
    color: CHGRD,
    width_px: width(4),
    dash_on_px: DASH_ON,
    dash_off_px: DASH_OFF,
};

/// CBLSUB: LS(DASH,1,CHMGD) - Submarine cable: dashed, width 1, magenta
pub const CBLSUB: LineStyle = LineStyle {
    color: CHMGD,
    width_px: width(1),
    dash_on_px: DASH_ON,
    dash_off_px: DASH_OFF,
};

/// CHNWIR: LS(DASH,1,CHBLK) - Chain/wire: dashed, width 1, black
pub const CHNWIR: LineStyle = LineStyle {
    color: CHBLK,
    width_px: width(1),
    dash_on_px: DASH_ON,
    dash_off_px: DASH_OFF,
};

/// NAVLNE: LS(DASH,1,CHGRD) - Navigation line: dashed, width 1, gray
pub const NAVLNE: LineStyle = LineStyle {
    color: CHGRD,
    width_px: width(1),
    dash_on_px: DASH_ON,
    dash_off_px: DASH_OFF,
};

/// TSELNE: LS(SOLD,6,TRFCF) - Traffic separation line: solid, width 6, light magenta
pub const TSELNE: LineStyle = LineStyle {
    color: TRFCF,
    width_px: width(6),
    dash_on_px: 0.0,
    dash_off_px: 0.0,
};

/// TSSBND: LS(DASH,2,TRFCD) - TSS boundary: dashed, width 2, dark magenta
pub const TSSBND: LineStyle = LineStyle {
    color: TRFCD,
    width_px: width(2),
    dash_on_px: DASH_ON,
    dash_off_px: DASH_OFF,
};

/// ROADWY: LS(SOLD,2,LANDF) - Road: solid, width 2, brown
pub const ROADWY: LineStyle = LineStyle {
    color: LANDF,
    width_px: width(2),
    dash_on_px: 0.0,
    dash_off_px: 0.0,
};

/// RAILWY: LS(SOLD,2,LANDF) - Railway: solid, width 2, brown
pub const RAILWY: LineStyle = LineStyle {
    color: LANDF,
    width_px: width(2),
    dash_on_px: 0.0,
    dash_off_px: 0.0,
};

/// PIPSOL: solid medium gray (LC texture placeholder)
pub const PIPSOL: LineStyle = LineStyle {
    color: CHGRD,
    width_px: width(2),
    dash_on_px: 0.0,
    dash_off_px: 0.0,
};

/// LAKSHR: LS(SOLD,1,CSTLN) - Lake shore: solid, width 1
pub const LAKSHR: LineStyle = LineStyle {
    color: CSTLN,
    width_px: width(1),
    dash_on_px: 0.0,
    dash_off_px: 0.0,
};

/// VEGATN: LS(DASH,1,LANDF) - Vegetation: dashed, width 1, brown
pub const VEGATN: LineStyle = LineStyle {
    color: LANDF,
    width_px: width(1),
    dash_on_px: DASH_ON,
    dash_off_px: DASH_OFF,
};

/// TUNNEL: LS(DASH,1,CHGRD) - Tunnel: dashed, width 1, gray
pub const TUNNEL: LineStyle = LineStyle {
    color: CHGRD,
    width_px: width(1),
    dash_on_px: DASH_ON,
    dash_off_px: DASH_OFF,
};

/// OBSTRN: LS(DASH,1,CHMGD) - Obstruction: dashed, width 1, magenta
pub const OBSTRN: LineStyle = LineStyle {
    color: CHMGD,
    width_px: width(1),
    dash_on_px: DASH_ON,
    dash_off_px: DASH_OFF,
};

// ============================================================
// Backwards Compatibility Aliases
// These map old color names to S-52 equivalents
// ============================================================

/// Coastline style (dashed per S-52)
pub const COASTLINE_STYLE: LineStyle = COALNE;

/// Depth contour style (solid per S-52)
pub const CONTOUR_STYLE: LineStyle = DEPCNT;

/// Safety contour style (solid, thicker)
pub const SAFETY_CONTOUR_STYLE: LineStyle = DEPCNT_SAFETY;

// ============================================================
// LineStyleId to LineStyle mapping for tile rendering
// ============================================================

use crate::tiles::LineStyleId;

/// Convert S-52 width units to pixels with custom ppmm.
/// Matches OpenCPN's line width handling:
/// - For normal displays (ppmm <= 7): use w directly as pixel width
/// - For HiDPI displays (ppmm > 7): scale assuming w was designed for 6 ppmm
fn width_scaled(w: u8, ppmm: f32) -> f32 {
    let w_f = if w == 0 { 1.0 } else { w as f32 };
    if ppmm > 7.0 {
        // HiDPI: treat w as "pixels at 6 ppmm" and scale to actual ppmm
        // This matches OpenCPN: target_w_mm = w / 6.0; lineWidth = target_w_mm * ppmm
        (w_f / 6.0) * ppmm
    } else {
        // Normal displays: use w directly as pixel width
        w_f
    }
}

/// Compute dash pattern for given ppmm
fn dash_pattern(ppmm: f32) -> (f32, f32) {
    let period = 3.0 * ppmm;  // 3mm period
    (period * 0.66, period * 0.34)
}

/// Compute dot pattern for given ppmm
fn dot_pattern(ppmm: f32) -> (f32, f32) {
    let period = 1.0 * ppmm;  // 1mm period
    (period * 0.5, period * 0.5)
}

/// Get the LineStyle for a given LineStyleId, scaled by ppmm for HiDPI
pub fn style_for_id(id: LineStyleId, ppmm: f32) -> LineStyle {
    let (dash_on, dash_off) = dash_pattern(ppmm);
    let (dot_on, dot_off) = dot_pattern(ppmm);

    match id {
        LineStyleId::Coastline => LineStyle {
            color: CSTLN,
            width_px: width_scaled(1, ppmm),
            dash_on_px: 0.0,  // SOLD per spec Appendix N
            dash_off_px: 0.0,
        },
        LineStyleId::ShorelineConstruction => LineStyle {
            color: CSTLN,
            width_px: width_scaled(2, ppmm),
            dash_on_px: 0.0,
            dash_off_px: 0.0,
        },
        LineStyleId::ShorelineConstructionWharf => LineStyle {
            color: CSTLN,
            width_px: width_scaled(4, ppmm),
            dash_on_px: 0.0,
            dash_off_px: 0.0,
        },
        LineStyleId::DepthContour => LineStyle {
            color: DEPCN,
            width_px: width_scaled(1, ppmm),
            dash_on_px: 0.0,
            dash_off_px: 0.0,
        },
        LineStyleId::DepthContourSafety => LineStyle {
            color: DEPSC,
            width_px: width_scaled(2, ppmm),
            dash_on_px: 0.0,
            dash_off_px: 0.0,
        },
        LineStyleId::CableOverhead => LineStyle {
            color: CHGRD,
            width_px: width_scaled(4, ppmm),
            dash_on_px: dash_on,
            dash_off_px: dash_off,
        },
        LineStyleId::CableSubmarine => LineStyle {
            color: CHMGD,
            width_px: width_scaled(1, ppmm),
            dash_on_px: dash_on,
            dash_off_px: dash_off,
        },
        LineStyleId::TrafficSeparationLine => LineStyle {
            color: TRFCF,
            width_px: width_scaled(6, ppmm),
            dash_on_px: 0.0,
            dash_off_px: 0.0,
        },
        LineStyleId::Road => LineStyle {
            color: LANDF,
            width_px: width_scaled(2, ppmm),
            dash_on_px: 0.0,
            dash_off_px: 0.0,
        },
        LineStyleId::RiverBank => LineStyle {
            color: CSTLN,
            width_px: width_scaled(2, ppmm),
            dash_on_px: dot_on,
            dash_off_px: dot_off,
        },
        LineStyleId::Pipeline => LineStyle {
            color: CHGRD,
            width_px: width_scaled(2, ppmm),
            dash_on_px: 0.0,
            dash_off_px: 0.0,
        },
    }
}

// ============================================================
// Dynamic Style Resolution (for LineStyleKey from S-52 lookups)
// ============================================================

/// Resolve S-52 color token to RGB color.
/// Returns CHBLK (black) for unknown tokens.
pub fn color_for_token(token: &str, tables: Option<&LookupTables>) -> Color {
    if let Some(tables) = tables {
        if let Some(color) = tables.get_color_f32(token) {
            return color;
        }
    }

    match token {
        "CSTLN" => CSTLN,
        "DEPCN" => DEPCN,
        "DEPSC" => DEPSC,
        "CHBLK" => CHBLK,
        "CHGRD" => CHGRD,
        "CHGRF" => CHGRF,
        "CHMGD" => CHMGD,
        "CHMGF" => CHMGF,
        "CHBRN" => CHBRN,
        "OUTLW" => OUTLW,
        "LITRD" => LITRD,
        "LITGN" => LITGN,
        "LITYW" => LITYW,
        "LANDF" => LANDF,
        "LANDA" => LANDA,
        "TRFCD" => TRFCD,
        "ATRFCD" => TRFCD,   // compound ref: foreground traffic route color
        "TRFCF" => TRFCF,
        "CURSR" => CURSR,
        "SNDG1" => SNDG1,
        "SNDG2" => SNDG2,
        // Depth area colors (fallback)
        "DEPIT" => [0.514, 0.698, 0.584, 1.0],  // Intertidal
        "DEPVS" => [0.451, 0.714, 0.937, 1.0],  // Very shallow
        "DEPMS" => [0.596, 0.773, 0.949, 1.0],  // Medium shallow
        "DEPMD" => [0.729, 0.835, 0.882, 1.0],  // Medium deep
        "DEPDW" => [0.831, 0.918, 0.933, 1.0],  // Deep water
        _ => {
            // Unknown token - log and return black
            log::trace!("Unknown S-52 color token: {}", token);
            CHBLK
        }
    }
}

/// Get LineStyle from LineStyleKey, scaled by ppmm for HiDPI.
///
/// This is the dynamic equivalent of style_for_id(), used when
/// styles come from S-52 lookup tables rather than hardcoded enums.
pub fn style_for_key(key: &LineStyleKey, ppmm: f32, tables: Option<&LookupTables>) -> LineStyle {
    // Resolve color token to RGB
    let color = color_for_token(&key.color_token, tables);

    // Convert S-52 width units to pixels using OpenCPN-compatible formula
    let width_px = width_scaled(key.width, ppmm);

    // Convert pattern to dash/dot parameters
    let (dash_on_px, dash_off_px) = match key.pattern {
        LinePattern::Solid => (0.0, 0.0),
        LinePattern::Dashed => dash_pattern(ppmm),
        LinePattern::Dotted => dot_pattern(ppmm),
    };

    LineStyle {
        color,
        width_px,
        dash_on_px,
        dash_off_px,
    }
}
