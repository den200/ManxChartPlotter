//! S-52 Day_Bright color palette and line style lookup.
//!
//! This module provides S-52 compliant colors and line styles for nautical chart rendering.
//! Colors are from the Day_Bright palette, converted to 0-1 float RGBA.
//! Line styles follow S-52 LS() specifications with proper dash/dot patterns.

use super::state::LineStyle;

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

/// Convert S-52 width units to pixels (0.5mm per unit)
const fn width(w: u8) -> f32 {
    0.5 * (w as f32) * PPMM
}

// ============================================================
// Precomputed Line Styles
// Each constant corresponds to an S-52 LS() specification
// ============================================================

/// COALNE: LS(DASH,1,CSTLN) - Coastline: dashed, width 1, gray
pub const COALNE: LineStyle = LineStyle {
    color: CSTLN,
    width_px: width(1),
    dash_on_px: DASH_ON,
    dash_off_px: DASH_OFF,
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
