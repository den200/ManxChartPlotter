//! Text layout engine for converting strings to glyph instances.
//!
//! Converts text strings with positioning parameters into LabelGlyphInstance
//! arrays ready for GPU rendering.

use super::camera::Camera;
use super::label::LabelGlyphInstance;
use super::text::SoundingInstance;

/// Nominal glyph cell, in pixels at `scale = 1.0`.
///
/// The cell is no longer the glyph — the font is proportional — but it stays
/// the unit `scale` is expressed in, and the unit S-52's `xoffs`/`yoffs` count
/// in, so every caller and the tuned `s52_text_scale` keep working unchanged.
const CELL_W: f32 = 8.0;
pub(crate) const CELL_H: f32 = 8.0;

/// Cap height, as a fraction of the cell, that the proportional font is sized
/// to. The 5x7 bitmap it replaces inked 8 of its 8 cell rows, so matching that
/// cap height keeps every label the size the reference captures were tuned to.
const CAP_PER_CELL: f32 = 1.0;

/// Where the baseline sits inside the cell, measured down from the cell centre
/// in cell heights. The old bitmap sat cap-top at the cell top with its
/// descenders inside the cell, which puts the baseline a little above the
/// bottom edge.
const BASELINE_DROP: f32 = 0.38;

/// Pixels per em for a given `scale`, derived from the font's own cap height so
/// a different typeface stays the same visual size.
fn em_px(scale: f32) -> f32 {
    CELL_H * scale * CAP_PER_CELL / super::font::atlas().cap_height
}

/// Width of `text` in pixels at `scale`.
///
/// `space` is the S-52 spacing code. Both of the values the tables actually use
/// lay the text out with the font's own advances; see [`SPACE_STANDARD`].
pub fn text_width_px(text: &str, scale: f32, bold: bool, space: u8) -> f32 {
    let _ = space;
    super::font::atlas().advance_of(text, bold) * em_px(scale)
}

/// S-52 TX/TE character spacing: 1 = fit, 2 = standard, 3 = wrapped.
///
/// Neither 2 nor 3 changes the *pitch* — 3 means the label may break across
/// lines, and 1 means it is stretched to fit between two positions. qutenav
/// names them `{Fit = 1, Standard = 2, Wrapped = 3}` and implements 2 and 3
/// identically, warning on 1. navcore does the same; wrapping is a layout
/// feature nothing in the tables needs yet, and s52plib ignores the field
/// entirely.
pub const SPACE_STANDARD: u8 = 2;

/// Push one glyph, converting the font's em-space quad into the centred pixel
/// quad the vertex shader expects.
///
/// `pen_x` is the pen position and `baseline_y` the baseline, both in the
/// label's local pixel space (y up). Returns the advance.
#[allow(clippy::too_many_arguments)]
fn push_glyph(
    out: &mut Vec<LabelGlyphInstance>,
    c: char,
    bold: bool,
    position: [f32; 2],
    pen_x: f32,
    baseline_y: f32,
    em: f32,
    color: [f32; 4],
    color_index: u32,
) -> f32 {
    let Some(g) = super::font::atlas().glyph(c, bold) else {
        return 0.0;
    };
    let (x0, y0) = (pen_x + g.plane[0] * em, baseline_y + g.plane[1] * em);
    let (x1, y1) = (pen_x + g.plane[2] * em, baseline_y + g.plane[3] * em);
    let advance = g.advance * em;
    if x1 > x0 && y1 > y0 {
        out.push(LabelGlyphInstance {
            position,
            offset_px: [(x0 + x1) * 0.5, (y0 + y1) * 0.5],
            size_px: [x1 - x0, y1 - y0],
            uv_min: [g.uv[0], g.uv[1]],
            uv_max: [g.uv[2], g.uv[3]],
            rotation: 0.0,
            color,
            color_index,
        });
    }
    advance
}

/// Text justification (S-52 convention)
#[derive(Clone, Copy, Debug, Default)]
pub enum HJust {
    Center = 1,
    Right = 2,
    #[default]
    Left = 3,
}

#[derive(Clone, Copy, Debug, Default)]
pub enum VJust {
    #[default]
    Bottom = 1,
    Center = 2,
    Top = 3,
}

impl From<u8> for HJust {
    fn from(v: u8) -> Self {
        match v {
            1 => HJust::Center,
            2 => HJust::Right,
            _ => HJust::Left,
        }
    }
}

impl From<u8> for VJust {
    fn from(v: u8) -> Self {
        match v {
            2 => VJust::Center,
            3 => VJust::Top,
            _ => VJust::Bottom,
        }
    }
}

/// Text layout parameters (based on S-52 TX instruction)
#[derive(Clone, Debug)]
pub struct TextParams {
    /// World position (SM meters)
    pub position: [f32; 2],
    /// Text content
    pub text: String,
    /// Text color (RGBA)
    pub color: [f32; 4],
    /// S-52 palette color index for dynamic Day/Dusk/Night switching.
    pub color_index: u32,
    /// Font scale multiplier (1.0 = default size)
    pub scale: f32,
    /// Horizontal justification
    pub hjust: HJust,
    /// Vertical justification
    pub vjust: VJust,
    /// X offset in character widths
    pub xoffs: i32,
    /// Y offset in character heights
    pub yoffs: i32,
    /// S-52 display priority (0-9, higher = more important)
    pub disp_prio: u8,
    /// S-52 text display group / viewing group (dis field from TX/TE).
    /// Lower = more important. Used by ShowImportantTextOnly filter (hides >= 20).
    pub dis: u8,
    /// Draw in the bold weight. S-52's TX/TE carries a font weight in CHARS[1]
    /// ('6' = bold); OpenCPN honours it, which is why place names are heavy and
    /// light descriptions are not.
    pub bold: bool,
    /// S-52 character spacing: 1 = fit, 2 = standard, 3 = proportional.
    ///
    /// Standard spacing means a fixed pitch — seabed qualities and the TE'd
    /// names are specified that way, place names are not. s52plib parses this
    /// field and never uses it, so every label there gets the platform font's
    /// own spacing regardless of what the lookup asked for.
    pub space: u8,
}

impl Default for TextParams {
    fn default() -> Self {
        Self {
            position: [0.0, 0.0],
            text: String::new(),
            color: [0.0, 0.0, 0.0, 1.0], // CHBLK default
            color_index: 0,
            scale: 1.0,
            hjust: HJust::Left,
            vjust: VJust::Bottom,
            xoffs: 0,
            yoffs: 0,
            disp_prio: 0,
            dis: 10,
            bold: false,
            space: 3,
        }
    }
}

/// Layout text into glyph instances.
///
/// Converts a text string with positioning parameters into an array of
/// LabelGlyphInstance ready for GPU rendering.
///
/// # Arguments
/// * `params` - Text content and positioning parameters
///
/// # Returns
/// Vector of glyph instances for the text
pub fn layout_text(params: &TextParams) -> Vec<LabelGlyphInstance> {
    if params.text.is_empty() {
        return Vec::new();
    }

    let char_count = params.text.chars().count();
    let total_width = text_width_px(&params.text, params.scale, params.bold, params.space);
    let char_height = CELL_H * params.scale;
    let avg_char_width = CELL_W * params.scale;

    // OpenCPN: base is lower-left, with vertical baseline adjustment
    let mut xadjust = params.xoffs as f32 * avg_char_width;
    let mut yadjust = -(char_height * 10.0 / 8.0);
    yadjust += params.yoffs as f32 * char_height;

    // Justification
    match params.hjust {
        HJust::Center => xadjust -= total_width / 2.0,
        HJust::Right => xadjust -= total_width,
        HJust::Left => {}
    }
    match params.vjust {
        VJust::Top => yadjust += char_height,
        VJust::Center => yadjust += char_height / 2.0,
        VJust::Bottom => {}
    }

    // The old fixed cell was centred on (xadjust, yadjust); the pen starts at
    // that cell's left edge and the baseline sits inside it.
    let mut pen_x = xadjust - avg_char_width * 0.5;
    let baseline_y = yadjust - char_height * BASELINE_DROP;
    let em = em_px(params.scale);

    let mut instances = Vec::with_capacity(char_count);
    for c in params.text.chars() {
        pen_x += push_glyph(
            &mut instances,
            c,
            params.bold,
            params.position,
            pen_x,
            baseline_y,
            em,
            params.color,
            params.color_index,
        );
    }

    instances
}

/// Layout light text with S-52 defaults.
///
/// Convenience function for LITDSN01 text with standard light text positioning:
/// - Left justified (hjust=3)
/// - Center vertical (vjust=2)
/// - 2 character widths right of symbol
pub fn layout_light_text(position: [f32; 2], text: &str, color: [f32; 4]) -> Vec<LabelGlyphInstance> {
    layout_text(&TextParams {
        position,
        text: text.to_string(),
        color,
        color_index: 0,
        scale: 1.0,
        dis: 24,
        hjust: HJust::Left,
        vjust: VJust::Center,
        xoffs: 2, // 2 char widths right of symbol
        yoffs: 0,
        disp_prio: 0,
        bold: false,
        space: SPACE_STANDARD,
    })
}

fn sounding_color(flags: u32) -> [f32; 4] {
    if (flags & 1) != 0 {
        [0.027, 0.027, 0.027, 1.0]
    } else {
        [0.490, 0.537, 0.549, 1.0]
    }
}

/// Size of the fractional (subscript) digit relative to the main number.
const SUBSCRIPT_SCALE: f32 = 0.62;
/// How far below the main number's vertical center the subscript is dropped,
/// in fractions of a full glyph cell. Mirrors OpenCPN's SNDFRM02 digit
/// position groups, where the fractional digit uses a lower pivot
/// (`pivotHeight/5` vs `pivotHeight/2`, s52plib.cpp RenderSoundingSymbol).
const SUBSCRIPT_DROP: f32 = 0.30;

/// Lay out one sounding the OpenCPN way: the whole-number part on the baseline
/// and the fractional digit rendered smaller and lowered as a subscript, with
/// NO decimal point — e.g. depth 2.5 renders as a full-size "2" with a small,
/// low "5" tucked to its lower right (chart convention "2₅").
fn layout_sounding_glyphs(s: &SoundingInstance) -> Vec<LabelGlyphInstance> {
    let flags = s.flags;
    let has_decimal = (flags & (1 << 4)) != 0;
    let decimal_digit = ((flags >> 8) & 0xF) as u8;
    let whole_part = (flags >> 12) & 0x1_FFFF;
    let is_drying = (flags & (1 << 1)) != 0;
    let show_uncertainty = (flags & (1 << 2)) != 0;
    let is_swept = (flags & (1 << 3)) != 0;

    let color = sounding_color(flags);
    let color_index = s.color_index;
    let scale = s.scale;
    let cell = CELL_W * scale;
    let cell_h = CELL_H * scale;
    let em = em_px(scale);

    // Baseline matches layout_text() for HJust::Left / VJust::Bottom so the
    // sounding sits exactly where it always has.
    let base_y = -(cell_h * 10.0 / 8.0) - cell_h * BASELINE_DROP;

    // Main (full-size) part: optional swept/drying markers + whole digits.
    let mut main = String::new();
    if is_swept {
        main.push('~');
    }
    if is_drying {
        main.push('_');
    }
    main.push_str(&whole_part.to_string());

    let mut glyphs = Vec::new();
    let mut x = -cell * 0.5;
    for c in main.chars() {
        x += push_glyph(
            &mut glyphs, c, false, s.position, x, base_y, em, color, color_index,
        );
    }

    // Fractional digit as a subscript: smaller, dropped below the baseline,
    // tucked slightly under the trailing whole digit, and with no '.'.
    if has_decimal {
        let sub_x = x - cell * 0.12;
        let sub_y = base_y - cell_h * SUBSCRIPT_DROP;
        let c = char::from(b'0' + decimal_digit.min(9));
        x = sub_x
            + push_glyph(
                &mut glyphs,
                c,
                false,
                s.position,
                sub_x,
                sub_y,
                em * SUBSCRIPT_SCALE,
                color,
                color_index,
            );
    }

    // Uncertainty marker stays full size after the value.
    if show_uncertainty {
        push_glyph(
            &mut glyphs, '?', false, s.position, x, base_y, em, color, color_index,
        );
    }

    glyphs
}

/// How much of the symbol atlas's native size a sounding digit is drawn at.
///
/// The atlas cells are 6x10px including an antialiasing halo, so the ink is
/// about three quarters of that, and the symbol pipeline then applies its own
/// magnification. Calibrated against the reference: a sounding digit is ~22px
/// of ink on a 2x display. The same factor scales the pivots, which is what
/// keeps the digits of one sounding grouped.
const SOUNDING_SYMBOL_SCALE: f32 = 1.16;

/// Whether to portray soundings with the S-52 digit symbols.
///
/// Off by default, and the reason is the symbol atlas, not the procedure.
/// navcore's atlas is OpenCPN's `rastersymbols-day.png` copied verbatim: a
/// sheet of glyphs drawn for 1:1 display at 96 dpi, in which a sounding digit
/// is 6x10px with an antialiasing halo baked in. The renderer magnifies it 2.5x
/// to reach chart size, which turns that halo into a pale fringe and blends the
/// black core away — measured against the reference over one sounding field,
/// 100 near-black pixels against 636. The SDF text path is sharp at any size
/// and lands within a pixel of the reference, so it stays the default.
///
/// Rasterising the symbols from their vector definitions at display resolution
/// fixes this for *every* symbol on the chart, not just soundings; the digit
/// symbols in particular are `<definition>R</definition>` — raster-only — so
/// they would need upscaling rather than re-rendering. Until then this is
/// available with `NAVCORE_SOUNDING_SYMBOLS=1` for comparison.
pub fn sounding_symbols_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("NAVCORE_SOUNDING_SYMBOLS").is_ok_and(|v| v != "0"))
}

/// Turn decluttered soundings into the S-52 digit symbols that portray them.
///
/// Each symbol carries the pivot that places it, so all of them are emitted at
/// the sounding's own position and the presentation library does the layout —
/// which is the point of the design: the digits stay legible and correctly
/// grouped without the renderer knowing anything about digit metrics.
pub fn layout_sounding_symbols(
    soundings: &[SoundingInstance],
) -> Vec<super::symbols::SymbolInstance> {
    let mut out = Vec::with_capacity(soundings.len() * 2);
    for s in soundings {
        let info = crate::s52::cs::SoundingRenderInfo::from_flags(s.flags);
        for name in info.symbols() {
            let Some(symbol_id) = super::symbols::symbol_id_from_s52_name(&name) else {
                log::debug!("sounding symbol {} not in the atlas", name);
                continue;
            };
            out.push(super::symbols::SymbolInstance {
                position: s.position,
                symbol_id,
                rotation: 0.0,
                // Soundings sit above the depth shades and below the point
                // symbols proper; priority 6 is where S-52 puts them.
                disp_prio: 6,
                scale: s.scale * SOUNDING_SYMBOL_SCALE / 1.8,
            });
        }
    }
    out
}

pub fn layout_sounding_labels(soundings: &[SoundingInstance]) -> Vec<LabelGlyphInstance> {
    let mut glyphs = Vec::new();
    for sounding in soundings {
        glyphs.extend(layout_sounding_glyphs(sounding));
    }
    glyphs
}


/// Simple AABB for text decluttering
#[derive(Clone, Copy)]
struct LabelRect {
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
}

impl LabelRect {
    fn intersects(&self, other: &LabelRect) -> bool {
        self.min_x < other.max_x
            && self.max_x > other.min_x
            && self.min_y < other.max_y
            && self.max_y > other.min_y
    }
}

/// Declutter label glyphs by removing overlapping labels.
///
/// Groups consecutive glyphs by shared world position (same label),
/// computes screen-space bounding boxes, and removes labels that overlap
/// with higher-priority (earlier) labels. This is the same simple AABB
/// approach used by OpenCPN's `CheckTextRectList`.
///
/// Labels should be ordered by priority (highest priority first) before calling.
pub fn declutter_labels(glyphs: &mut Vec<LabelGlyphInstance>, camera: &Camera) {
    if glyphs.len() < 2 {
        return;
    }

    // 1. Identify label groups: consecutive glyphs sharing the same world position
    let mut groups: Vec<(usize, usize)> = Vec::new(); // (start_idx, count)
    let mut group_start = 0;
    for i in 1..glyphs.len() {
        let prev = &glyphs[i - 1];
        let curr = &glyphs[i];
        // New label when world position changes
        if (curr.position[0] - prev.position[0]).abs() > 0.01
            || (curr.position[1] - prev.position[1]).abs() > 0.01
        {
            groups.push((group_start, i - group_start));
            group_start = i;
        }
    }
    groups.push((group_start, glyphs.len() - group_start));

    if groups.len() < 2 {
        return; // Nothing to declutter
    }

    // 2. Compute AABB for each label group
    let rects: Vec<LabelRect> = groups
        .iter()
        .map(|&(start, count)| {
            let mut min_x = f32::MAX;
            let mut min_y = f32::MAX;
            let mut max_x = f32::MIN;
            let mut max_y = f32::MIN;
            for g in &glyphs[start..start + count] {
                let anchor = camera.world_to_screen(g.position[0], g.position[1]);
                let gx = anchor.x + g.offset_px[0];
                let gy = anchor.y + g.offset_px[1];
                min_x = min_x.min(gx);
                min_y = min_y.min(gy);
                max_x = max_x.max(gx + g.size_px[0]);
                max_y = max_y.max(gy + g.size_px[1]);
            }
            LabelRect {
                min_x,
                min_y,
                max_x,
                max_y,
            }
        })
        .collect();

    // 3. First-come-first-serve: keep labels that don't overlap earlier accepted labels
    let mut accepted_rects: Vec<LabelRect> = Vec::new();
    let mut keep = vec![false; groups.len()];

    for (i, rect) in rects.iter().enumerate() {
        let overlaps = accepted_rects.iter().any(|r| r.intersects(rect));
        if !overlaps {
            accepted_rects.push(*rect);
            keep[i] = true;
        }
    }

    // 4. Rebuild glyph list with only kept labels
    let kept_count: usize = groups.iter().zip(keep.iter())
        .filter(|(_, &k)| k)
        .map(|(&(_, count), _)| count)
        .sum();

    if kept_count == glyphs.len() {
        return; // Nothing removed
    }

    let mut new_glyphs = Vec::with_capacity(kept_count);
    for (i, &(start, count)) in groups.iter().enumerate() {
        if keep[i] {
            new_glyphs.extend_from_slice(&glyphs[start..start + count]);
        }
    }

    let removed = glyphs.len() - new_glyphs.len();
    if removed > 0 {
        log::trace!("declutter: removed {} of {} label glyphs ({} labels kept of {})",
            removed, glyphs.len(), accepted_rects.len(), groups.len());
    }
    *glyphs = new_glyphs;
}

/// Estimate label bounding box cheaply without full glyph layout.
///
/// Uses text length * average character width instead of computing per-glyph
/// metrics. Returns (width, height) in pixels.
fn estimate_label_bounds(params: &TextParams, camera: &Camera) -> LabelRect {
    let total_width = text_width_px(&params.text, params.scale, params.bold, params.space);
    let char_height = CELL_H * params.scale;
    let avg_char_width = CELL_W * params.scale;

    // Replicate justification offsets from layout_text() but without per-glyph work
    let mut xadjust = params.xoffs as f32 * avg_char_width;
    let mut yadjust = -(char_height * 10.0 / 8.0);
    yadjust += params.yoffs as f32 * char_height;

    match params.hjust {
        HJust::Center => xadjust -= total_width / 2.0,
        HJust::Right => xadjust -= total_width,
        HJust::Left => {}
    }
    match params.vjust {
        VJust::Top => yadjust += char_height,
        VJust::Center => yadjust += char_height / 2.0,
        VJust::Bottom => {}
    }

    let anchor = camera.world_to_screen(params.position[0], params.position[1]);
    let x = anchor.x + xadjust;
    let y = anchor.y + yadjust;

    LabelRect {
        min_x: x,
        min_y: y,
        max_x: x + total_width,
        max_y: y + char_height,
    }
}

/// Two-phase label decluttering: cheap AABB pre-check then full layout.
///
/// Phase 1: Estimate label bounds using text length * avg char width (no glyph layout).
///          Run first-come-first-serve AABB acceptance on these cheap bounds.
/// Phase 2: Only call `layout_text()` for labels that passed the pre-check.
///
/// This eliminates ~80% of `layout_text()` calls since most labels are rejected
/// by AABB overlap.
pub fn declutter_and_layout_labels(
    candidates: &[TextParams],
    camera: &Camera,
) -> Vec<LabelGlyphInstance> {
    declutter_and_layout_labels_ex(candidates, camera, false)
}

/// Declutter variant with the ShowImportantTextOnly filter.
///
/// Mirrors OpenCPN's `m_bShowS57ImportantTextOnly` (s52plib.cpp:2408-2411):
/// when true, drop every candidate whose text display-group (dis) is >= 20.
/// That cuts the overview label stampede (LNDRGN place names at dis=26 etc.)
/// without affecting navigationally-important text (lights at dis=11 etc.).
pub fn declutter_and_layout_labels_ex(
    candidates: &[TextParams],
    camera: &Camera,
    important_text_only: bool,
) -> Vec<LabelGlyphInstance> {
    if candidates.is_empty() {
        return Vec::new();
    }

    // Optional pre-filter: drop low-importance labels outright so they don't
    // consume layout cycles or collide with important ones during declutter.
    let filtered: Vec<usize> = if important_text_only {
        (0..candidates.len())
            .filter(|&i| candidates[i].dis < 20)
            .collect()
    } else {
        (0..candidates.len()).collect()
    };
    if filtered.is_empty() {
        return Vec::new();
    }

    // Sort by importance so label-collision losers are the least critical.
    //   1. Higher S-52 feature priority (disp_prio) wins first.
    //   2. Within the same priority, lower text-display-group (dis) wins — OpenCPN
    //      treats dis=11 ("navigationally important") as outranking dis=26
    //      ("place name, low importance"). See s52plib.cpp:2408-2411.
    let mut sorted_indices: Vec<usize> = filtered;
    sorted_indices.sort_by(|&a, &b| {
        candidates[b]
            .disp_prio
            .cmp(&candidates[a].disp_prio)
            .then_with(|| candidates[a].dis.cmp(&candidates[b].dis))
    });

    // Phase 1: Cheap AABB pre-check using estimated bounds (in priority order)
    let estimated_rects: Vec<LabelRect> = sorted_indices
        .iter()
        .map(|&i| estimate_label_bounds(&candidates[i], camera))
        .collect();

    let mut accepted_rects: Vec<LabelRect> = Vec::new();
    let mut keep = vec![false; sorted_indices.len()];

    for (i, rect) in estimated_rects.iter().enumerate() {
        let overlaps = accepted_rects.iter().any(|r| r.intersects(rect));
        if !overlaps {
            accepted_rects.push(*rect);
            keep[i] = true;
        }
    }

    let accepted_count = keep.iter().filter(|&&k| k).count();
    log::trace!(
        "declutter_and_layout: {} of {} labels accepted by AABB pre-check ({:.0}% skipped)",
        accepted_count,
        candidates.len(),
        (1.0 - accepted_count as f64 / candidates.len() as f64) * 100.0
    );

    // Phase 2: Full layout only for accepted labels (map back to original indices)
    let mut glyphs = Vec::new();
    for (i, &orig_idx) in sorted_indices.iter().enumerate() {
        if keep[i] {
            glyphs.extend(layout_text(&candidates[orig_idx]));
        }
    }

    glyphs
}

/// Declutter sounding instances by removing overlapping soundings.
///
/// Shallow soundings (lower depth) have higher priority per S-52.
/// Each sounding's AABB is estimated from its digit count.
pub fn declutter_soundings(soundings: &mut Vec<super::text::SoundingInstance>, camera: &Camera) {
    if soundings.len() < 2 {
        return;
    }

    // Sort by depth (shallowest first = highest priority per S-52)
    soundings.sort_by(|a, b| a.depth.partial_cmp(&b.depth).unwrap_or(std::cmp::Ordering::Equal));

    // Estimate AABB for each sounding (digit_count * cell_width, cell_height)
    let sounding_rects: Vec<LabelRect> = soundings
        .iter()
        .map(|s| {
            let digit_count = ((s.flags >> 5) & 0x7) as f32;
            let has_decimal = (s.flags & 0x10) != 0;
            // Digits are tabular, so one digit's advance sizes them all.
            let digit_w = text_width_px("0", s.scale, false, SPACE_STANDARD);
            let width = (digit_count + if has_decimal { SUBSCRIPT_SCALE } else { 0.0 }) * digit_w;
            let height = CELL_H * s.scale;
            let anchor = camera.world_to_screen(s.position[0], s.position[1]);
            LabelRect {
                min_x: anchor.x,
                min_y: anchor.y - height,
                max_x: anchor.x + width,
                max_y: anchor.y,
            }
        })
        .collect();

    let mut accepted_rects: Vec<LabelRect> = Vec::new();
    let mut keep = vec![false; soundings.len()];

    for (i, rect) in sounding_rects.iter().enumerate() {
        let overlaps = accepted_rects.iter().any(|r| r.intersects(rect));
        if !overlaps {
            accepted_rects.push(*rect);
            keep[i] = true;
        }
    }

    let original_len = soundings.len();
    let mut i = 0;
    soundings.retain(|_| { let k = keep[i]; i += 1; k });

    let removed = original_len - soundings.len();
    if removed > 0 {
        log::trace!("declutter_soundings: removed {} of {} soundings",
            removed, original_len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::Camera;

    #[test]
    fn test_layout_empty() {
        let params = TextParams {
            text: String::new(),
            ..Default::default()
        };
        let glyphs = layout_text(&params);
        assert!(glyphs.is_empty());
    }

    /// The font is proportional, so the assertions here are about geometry
    /// that must hold for any typeface — glyph count, anchoring, justification
    /// and scaling — not about a fixed cell size.
    #[test]
    fn test_layout_single_char() {
        let params = TextParams {
            position: [100.0, 200.0],
            text: "A".to_string(),
            ..Default::default()
        };
        let glyphs = layout_text(&params);
        assert_eq!(glyphs.len(), 1);
        assert_eq!(glyphs[0].position, [100.0, 200.0]);
        assert!(glyphs[0].size_px[0] > 0.0 && glyphs[0].size_px[1] > 0.0);
        // Cap height at scale 1.0 is one cell; the quad adds the SDF padding.
        assert!(glyphs[0].size_px[1] > CELL_H && glyphs[0].size_px[1] < CELL_H * 3.0);
    }

    #[test]
    fn test_layout_center_justified() {
        let text = "AB";
        let params = TextParams {
            text: text.to_string(),
            hjust: HJust::Center,
            ..Default::default()
        };
        let glyphs = layout_text(&params);
        assert_eq!(glyphs.len(), 2);
        let width = text_width_px(text, params.scale, false, params.space);
        // Centred text straddles the anchor: the pen starts half a width left.
        let left = glyphs[0].offset_px[0] - glyphs[0].size_px[0] / 2.0;
        let right = glyphs[1].offset_px[0] + glyphs[1].size_px[0] / 2.0;
        assert!((left + right).abs() < width, "not centred: {left}..{right}");
        assert!(glyphs[0].offset_px[0] < glyphs[1].offset_px[0], "A before B");
    }

    #[test]
    fn test_layout_with_xoffs() {
        let base = layout_text(&TextParams {
            text: "X".to_string(),
            ..Default::default()
        });
        let shifted = layout_text(&TextParams {
            text: "X".to_string(),
            xoffs: 2, // 2 character widths right
            ..Default::default()
        });
        // xoffs counts nominal cells, whatever the glyph's own width is.
        assert_eq!(shifted[0].offset_px[0] - base[0].offset_px[0], 2.0 * CELL_W);
        assert_eq!(shifted[0].offset_px[1], base[0].offset_px[1]);
    }

    #[test]
    fn test_layout_light_text() {
        let glyphs = layout_light_text([1000.0, 2000.0], "Fl(3)G", [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(glyphs.len(), 6);
        assert!(glyphs.iter().all(|g| g.position == [1000.0, 2000.0]));
        // Left-justified with xoffs=2: the first glyph sits right of the anchor.
        assert!(glyphs[0].offset_px[0] > CELL_W);
    }

    #[test]
    fn test_layout_scaled() {
        let mk = |scale| {
            layout_text(&TextParams {
                text: "AB".to_string(),
                scale,
                ..Default::default()
            })
        };
        let one = mk(1.0);
        let two = mk(2.0);
        assert_eq!(one.len(), 2);
        assert_eq!(two.len(), 2);
        // Doubling the scale doubles both the glyph size and the advance.
        for i in 0..2 {
            assert!((two[i].size_px[0] - one[i].size_px[0] * 2.0).abs() < 0.01);
            assert!((two[i].size_px[1] - one[i].size_px[1] * 2.0).abs() < 0.01);
        }
        let adv_one = one[1].offset_px[0] - one[0].offset_px[0];
        let adv_two = two[1].offset_px[0] - two[0].offset_px[0];
        assert!((adv_two - adv_one * 2.0).abs() < 0.01);
    }

    /// Danish, German and French chart names must survive to the glyph list —
    /// the old 5x7 table had nothing above U+007F and folded them to ASCII.
    #[test]
    fn test_latin1_names_have_glyphs() {
        for name in ["Vallensbæk", "Brøndby Lystbådehavn", "Køge", "Läsö"] {
            let glyphs = layout_text(&TextParams {
                text: name.to_string(),
                ..Default::default()
            });
            assert_eq!(
                glyphs.len(),
                name.chars().filter(|c| !c.is_whitespace()).count(),
                "{name} lost glyphs"
            );
        }
    }

    #[test]
    fn test_declutter_labels_uses_projected_screen_space() {
        let camera = Camera::new(0.0, 0.0, 1000.0, 800.0, 600.0);
        let labels = vec![
            TextParams {
                position: [0.0, 0.0],
                text: "A".to_string(),
                ..Default::default()
            },
            TextParams {
                position: [100.0, 0.0],
                text: "B".to_string(),
                ..Default::default()
            },
        ];

        let glyphs = declutter_and_layout_labels(&labels, &camera);
        assert_eq!(glyphs.len(), 1);
    }

    #[test]
    fn test_declutter_soundings_uses_projected_screen_space() {
        let camera = Camera::new(0.0, 0.0, 1000.0, 800.0, 600.0);
        let mut soundings = vec![
            super::super::text::SoundingInstance {
                position: [0.0, 0.0],
                depth: 5.0,
                flags: 1 << 5,
                scale: 1.0,
                color_index: 0,
            },
            super::super::text::SoundingInstance {
                position: [100.0, 0.0],
                depth: 6.0,
                flags: 1 << 5,
                scale: 1.0,
                color_index: 0,
            },
        ];

        declutter_soundings(&mut soundings, &camera);
        assert_eq!(soundings.len(), 1);
    }
}
