//! LC (Line Complex) symbols, drawn along the line by the line shader.
//!
//! An S-52 `LC()` symbol is a small HPGL drawing repeated along a line at its
//! own width, and like every other size in the presentation library it is
//! given in millimetres on the display.
//!
//! These used to be stamped on the CPU when a tile was built: each stamp's
//! segments were converted to Mercator metres at the *tile's* scale and baked
//! into the tile as ordinary line batches. The camera sits anywhere between
//! 0.71× and 1.41× of a tile level's scale, so the symbols (and their
//! spacing) grew and shrank by up to 41% while zooming and jumped at every
//! level change — and past the deepest level they simply kept growing.
//!
//! Now a tile only carries the line's segments ([`LcSegment`]), keyed with
//! `LineStyleKey::lc`, each with the arc length at which it starts along the
//! whole feature line. The line shader draws each segment as a screen-space
//! quad and, in pixels at the current zoom, places a repeat of the symbol at
//! every multiple of `advance_px` of arc length that falls on the segment,
//! turned to the segment's direction and drawn whole — exactly where and how
//! the CPU stamps went, but at a constant size and spacing on screen. Because
//! the arc length is measured along the unclipped line, the tiles either side
//! of a boundary agree on where the repeats fall, and each repeat is drawn by
//! the one segment it starts on, so none is doubled or dropped. The symbols'
//! segments live here, in pixels, in a storage buffer ([`LcAtlas`]).
//!
//! Each repeat is placed as s52plib's `draw_lc_poly` / `RenderFromHPGL`
//! places it: the symbol's *pivot* on the line, HPGL +y (down on the page) to
//! the right of the direction of travel, and the line walked in the direction
//! that makes that side consistent round an area (see
//! `tiles::builder::lc_direction`). The CPU stamps hung the drawing from its
//! origin instead — for CBLSUB06 that put the cable's zigzag 2 mm off to one
//! side of the cable — and mirrored it.

use std::sync::OnceLock;

use bytemuck::{Pod, Zeroable};

use crate::s52::lc::LineStyleTable;
use crate::s52::{LinePattern, LineStyleKey};

/// Every LC symbol's name, sorted: a symbol's index in this list is the
/// `LineStyleKey::lc` that refers to it.
pub fn lc_symbol_names() -> &'static [String] {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    NAMES.get_or_init(|| match crate::tiles::builder::get_line_style_table() {
        Some(table) => {
            let mut names: Vec<String> = table.names().cloned().collect();
            names.sort();
            names
        }
        None => Vec::new(),
    })
}

/// The index of an LC symbol in [`lc_symbol_names`].
pub fn lc_symbol_index(name: &str) -> Option<u16> {
    lc_symbol_names()
        .binary_search_by(|n| n.as_str().cmp(name))
        .ok()
        .and_then(|i| u16::try_from(i).ok())
}

/// One straight piece of an LC() line, drawn as an instanced quad.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Pod, Zeroable)]
pub struct LcSegment {
    /// Start, in metres from the tile centre.
    pub p0: [f32; 2],
    /// End, in metres from the tile centre.
    pub p1: [f32; 2],
    /// Arc length at `p0` along the whole feature line, metres.
    pub arc0: f32,
}

impl LcSegment {
    const ATTRIBS: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![
        0 => Float32x2, // p0
        1 => Float32x2, // p1
        2 => Float32,   // arc0
    ];

    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &Self::ATTRIBS,
        }
    }
}

/// Split pieces of line — each with the arc length its first point lies at —
/// into [`LcSegment`]s. Zero-length segments are dropped.
pub fn lc_segments(pieces: &[(Vec<[f32; 2]>, f32)]) -> Vec<LcSegment> {
    let mut out = Vec::new();
    for (points, start_arc) in pieces {
        let mut arc = *start_arc;
        for w in points.windows(2) {
            let (a, b) = (w[0], w[1]);
            let len = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
            if len > 0.0 {
                out.push(LcSegment { p0: a, p1: b, arc0: arc });
            }
            arc += len;
        }
    }
    out
}

/// Where one symbol's segments sit in [`LcAtlas::segments`], and its size.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LcSymbolGpu {
    /// First segment (each segment is two entries of `segments`).
    pub first: u32,
    /// Number of segments. Zero draws nothing.
    pub count: u32,
    /// Distance between repeats along the line, pixels.
    pub advance_px: f32,
    /// How far the symbol reaches either side of the line, pixels: its
    /// furthest point, plus its stroke, plus a pixel of antialiasing.
    pub half_extent_px: f32,
    /// How far before and after its place on the line the symbol reaches,
    /// pixels (strokes and antialiasing included): `[before, after]`, the
    /// first usually negative.
    pub x_range_px: [f32; 2],
}

/// Every LC symbol in pixels at one display density, ready for the GPU.
#[derive(Debug, Clone)]
pub struct LcAtlas {
    /// Two entries per segment: `[x0, y0, x1, y1]` in pixels from the
    /// symbol's pivot — x along the direction of travel, y to its *left* —
    /// then `[half_stroke_px, 0, 0, 0]`.
    pub segments: Vec<[f32; 4]>,
    /// Indexed like the names it was built from.
    pub symbols: Vec<LcSymbolGpu>,
}

impl LcAtlas {
    /// Lay out `names` from `table` at `ppmm` pixels per millimetre.
    ///
    /// An S-52 unit is 0.01 mm; the drawing is placed relative to the
    /// symbol's pivot, with HPGL y (down) turned into "left of travel" by a
    /// change of sign; each HPGL pen width is stroked like the LS() width of
    /// the same number.
    pub fn build(table: Option<&LineStyleTable>, names: &[String], ppmm: f32) -> Self {
        let mut atlas = LcAtlas { segments: Vec::new(), symbols: Vec::new() };
        for name in names {
            let Some(symbol) = table.and_then(|t| t.get(name)) else {
                atlas.symbols.push(LcSymbolGpu::default());
                continue;
            };
            let advance_px = symbol.width_pixels(ppmm);
            let first = (atlas.segments.len() / 2) as u32;
            let unit = 0.01 * ppmm; // pixels per S-52 unit
            let (px, py) = (symbol.pivot_x as f32, symbol.pivot_y as f32);
            let mut extent = 0.0f32;
            let mut x_range = [0.0f32, 0.0f32];
            let mut count = 0u32;
            // Below a pixel of advance the symbol cannot be drawn; the stamp
            // code skipped it too.
            if advance_px >= 1.0 {
                for seg in symbol.parse_hpgl().segments {
                    let stroke = super::s52_styles::style_for_key(
                        &LineStyleKey::new(LinePattern::Solid, seg.width.max(1), "CHBLK"),
                        ppmm,
                        None,
                    )
                    .width_px;
                    let x0 = (seg.x0 as f32 - px) * unit;
                    let x1 = (seg.x1 as f32 - px) * unit;
                    let y0 = -(seg.y0 as f32 - py) * unit;
                    let y1 = -(seg.y1 as f32 - py) * unit;
                    let reach = stroke * 0.5 + 1.0;
                    atlas.segments.push([x0, y0, x1, y1]);
                    atlas.segments.push([stroke * 0.5, 0.0, 0.0, 0.0]);
                    extent = extent.max(y0.abs().max(y1.abs()) + reach);
                    if count == 0 {
                        x_range = [x0.min(x1) - reach, x0.max(x1) + reach];
                    } else {
                        x_range = [x_range[0].min(x0.min(x1) - reach), x_range[1].max(x0.max(x1) + reach)];
                    }
                    count += 1;
                }
            }
            atlas.symbols.push(LcSymbolGpu {
                first,
                count,
                advance_px,
                half_extent_px: extent,
                x_range_px: x_range,
            });
        }
        // A storage binding cannot be empty.
        if atlas.segments.is_empty() {
            atlas.segments.push([0.0; 4]);
            atlas.segments.push([0.0; 4]);
        }
        atlas
    }

    /// The shared chartsymbols LC set at `ppmm`, indexed like
    /// [`lc_symbol_names`].
    pub fn shared(ppmm: f32) -> Self {
        Self::build(
            crate::tiles::builder::get_line_style_table(),
            lc_symbol_names(),
            ppmm,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> LineStyleTable {
        LineStyleTable::load_from_xml("assets/s52/chartsymbols.xml").unwrap()
    }

    /// Names are sorted and every one maps back to its own index.
    #[test]
    fn symbol_indices_round_trip() {
        let names = lc_symbol_names();
        assert!(names.len() > 50, "only {} LC symbols", names.len());
        assert!(names.windows(2).all(|w| w[0] < w[1]));
        for (i, n) in names.iter().enumerate() {
            assert_eq!(lc_symbol_index(n), Some(i as u16));
        }
        assert_eq!(lc_symbol_index("NOT_A_SYMBOL"), None);
    }

    /// Pieces become segments whose arc lengths carry on from each piece's
    /// start, so a repeat's position does not depend on where the line was
    /// cut.
    #[test]
    fn segments_carry_the_arc_length() {
        let pieces = vec![
            (vec![[0.0, 0.0], [3.0, 4.0], [3.0, 4.0], [3.0, 10.0]], 100.0),
            (vec![[50.0, 0.0], [60.0, 0.0]], 250.0),
        ];
        let segs = lc_segments(&pieces);
        assert_eq!(segs.len(), 3, "the zero-length segment is dropped: {segs:?}");
        assert_eq!(segs[0].arc0, 100.0);
        assert_eq!(segs[1].arc0, 105.0);
        assert_eq!(segs[1].p0, [3.0, 4.0]);
        assert_eq!(segs[2].arc0, 250.0);
        assert_eq!(std::mem::size_of::<LcSegment>(), 20);
    }

    /// LOWACC21 laid out in pixels: 1.3 mm advance at 4 px/mm, and ranges
    /// that contain every segment.
    #[test]
    fn lowacc21_is_laid_out_in_pixels() {
        let t = table();
        let names = vec!["LOWACC21".to_string()];
        let ppmm = 4.0;
        let atlas = LcAtlas::build(Some(&t), &names, ppmm);
        let sym = atlas.symbols[0];
        assert!((sym.advance_px - 130.0 * 0.01 * ppmm).abs() < 1e-4, "{sym:?}");
        assert!(sym.count > 0);
        for i in sym.first..sym.first + sym.count {
            let [x0, y0, x1, y1] = atlas.segments[2 * i as usize];
            let half_stroke = atlas.segments[2 * i as usize + 1][0];
            assert!(half_stroke >= 0.5);
            for (x, y) in [(x0, y0), (x1, y1)] {
                assert!(x - half_stroke > sym.x_range_px[0] && x + half_stroke < sym.x_range_px[1], "x {x}");
                assert!(y.abs() + half_stroke < sym.half_extent_px, "y {y} outside the extent");
            }
        }
    }

    /// The whole point: the layout depends on the display, not the zoom —
    /// there is no zoom anywhere in it — and scales exactly with density.
    #[test]
    fn layout_scales_with_display_density_only() {
        let t = table();
        let names = vec!["NAVARE51".to_string(), "CBLSUB06".to_string()];
        let a = LcAtlas::build(Some(&t), &names, 4.0);
        let b = LcAtlas::build(Some(&t), &names, 8.0);
        for (sa, sb) in a.symbols.iter().zip(&b.symbols) {
            assert!(sa.count > 0);
            assert!((sb.advance_px - 2.0 * sa.advance_px).abs() < 1e-3);
        }
        for (ga, gb) in a.segments.iter().zip(&b.segments).step_by(2) {
            for k in 0..4 {
                assert!((gb[k] - 2.0 * ga[k]).abs() < 1e-3);
            }
        }
    }

    /// CBLSUB06's zigzag hangs off its pivot, which sits on the zigzag's own
    /// centre line: centred on the cable, not 2 mm to one side of it. And
    /// HPGL y grows *down*, so a point below the pivot on the page is to the
    /// right of travel — negative in the atlas's left-positive y.
    #[test]
    fn cable_symbol_is_centred_on_the_line() {
        let t = table();
        let atlas = LcAtlas::build(Some(&t), &["CBLSUB06".to_string()], 4.0);
        let sym = atlas.symbols[0];
        let ys: Vec<f32> = (sym.first..sym.first + sym.count)
            .flat_map(|i| {
                let s = atlas.segments[2 * i as usize];
                [s[1], s[3]]
            })
            .collect();
        let (lo, hi) = ys.iter().fold((f32::MAX, f32::MIN), |(l, h), y| (l.min(*y), h.max(*y)));
        // The drawing spans 1050..1550 in y with its pivot at 1274: 2.24 mm
        // above and 2.76 mm below, i.e. +8.96 px left and -11.04 px right.
        assert!((hi - 8.96).abs() < 0.01 && (lo + 11.04).abs() < 0.01, "{lo}..{hi}");
        // Every repeat's drawing lies within a couple of advances of its place.
        assert!(sym.x_range_px[0] > -sym.advance_px && sym.x_range_px[1] < 2.0 * sym.advance_px);
    }

    /// Every symbol in the shared set gets a slot, so indices line up, and a
    /// missing table still yields a bindable (non-empty) buffer.
    #[test]
    fn shared_atlas_has_a_slot_per_name() {
        let atlas = LcAtlas::shared(4.0);
        assert_eq!(atlas.symbols.len(), lc_symbol_names().len());
        let empty = LcAtlas::build(None, &["LOWACC21".to_string()], 4.0);
        assert_eq!(empty.symbols[0].count, 0);
        assert_eq!(empty.segments.len(), 2);
    }
}
