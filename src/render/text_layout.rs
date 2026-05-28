//! Text layout engine for converting strings to glyph instances.
//!
//! Converts text strings with positioning parameters into LabelGlyphInstance
//! arrays ready for GPU rendering.

use super::camera::Camera;
use super::label::{glyph_uv_rect, LabelGlyphInstance};
use super::text::SoundingInstance;

/// Glyph dimensions in pixels (matching label.rs font atlas)
const _GLYPH_W: f32 = 5.0;
const _GLYPH_H: f32 = 7.0;
const CELL_W: f32 = 8.0;
const CELL_H: f32 = 8.0;

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
    let total_width = char_count as f32 * CELL_W * params.scale;
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

    let base_offset_x = xadjust;
    let base_offset_y = yadjust;

    let glyph_size = [CELL_W * params.scale, CELL_H * params.scale];

    let mut instances = Vec::with_capacity(char_count);
    let mut x_advance = 0.0;

    for c in params.text.chars() {
        // Get UV coordinates for this glyph
        let uvs = glyph_uv_rect(c).unwrap_or_else(|| glyph_uv_rect(' ').unwrap());

        instances.push(LabelGlyphInstance {
            position: params.position,
            offset_px: [base_offset_x + x_advance, base_offset_y],
            size_px: glyph_size,
            uv_min: [uvs[0], uvs[1]],
            uv_max: [uvs[2], uvs[3]],
            rotation: 0.0,
            color: params.color,
            color_index: params.color_index,
        });

        x_advance += CELL_W * params.scale;
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
    let full_size = [cell, cell_h];

    // Baseline matches layout_text() for HJust::Left / VJust::Bottom so the
    // sounding sits exactly where it always has.
    let base_y = -(cell_h * 10.0 / 8.0);

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
    let mut x = 0.0_f32;
    let mut push_glyph = |glyphs: &mut Vec<LabelGlyphInstance>, c: char, ox: f32, oy: f32, size: [f32; 2]| {
        let uv = glyph_uv_rect(c).unwrap_or_else(|| glyph_uv_rect(' ').unwrap());
        glyphs.push(LabelGlyphInstance {
            position: s.position,
            offset_px: [ox, oy],
            size_px: size,
            uv_min: [uv[0], uv[1]],
            uv_max: [uv[2], uv[3]],
            rotation: 0.0,
            color,
            color_index,
        });
    };

    for c in main.chars() {
        push_glyph(&mut glyphs, c, x, base_y, full_size);
        x += cell;
    }

    // Fractional digit as a subscript: smaller, dropped below the baseline,
    // tucked slightly under the trailing whole digit, and with no '.'.
    if has_decimal {
        let sub_size = [cell * SUBSCRIPT_SCALE, cell_h * SUBSCRIPT_SCALE];
        let sub_x = x - cell * 0.12;
        let sub_y = base_y - cell_h * SUBSCRIPT_DROP;
        let c = char::from(b'0' + decimal_digit.min(9));
        push_glyph(&mut glyphs, c, sub_x, sub_y, sub_size);
        x = sub_x + sub_size[0];
    }

    // Uncertainty marker stays full size after the value.
    if show_uncertainty {
        push_glyph(&mut glyphs, '?', x, base_y, full_size);
    }

    glyphs
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
    let char_count = params.text.chars().count() as f32;
    let total_width = char_count * CELL_W * params.scale;
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
            let width = (digit_count + if has_decimal { 1.5 } else { 0.0 }) * CELL_W * s.scale;
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

    #[test]
    fn test_layout_single_char() {
        let params = TextParams {
            position: [100.0, 200.0],
            text: "A".to_string(),
            color: [1.0, 0.0, 0.0, 1.0],
            color_index: 0,
            scale: 1.0,
            hjust: HJust::Left,
            vjust: VJust::Bottom,
            xoffs: 0,
            yoffs: 0,
            disp_prio: 0,
            dis: 10,
        };
        let glyphs = layout_text(&params);
        assert_eq!(glyphs.len(), 1);
        assert_eq!(glyphs[0].position, [100.0, 200.0]);
        assert_eq!(glyphs[0].offset_px, [0.0, -10.0]);
        assert_eq!(glyphs[0].size_px, [8.0, 8.0]);
    }

    #[test]
    fn test_layout_center_justified() {
        let params = TextParams {
            position: [0.0, 0.0],
            text: "AB".to_string(),
            color: [0.0, 0.0, 0.0, 1.0],
            color_index: 0,
            scale: 1.0,
            hjust: HJust::Center,
            vjust: VJust::Bottom,
            xoffs: 0,
            yoffs: 0,
            disp_prio: 0,
            dis: 10,
        };
        let glyphs = layout_text(&params);
        assert_eq!(glyphs.len(), 2);
        // Total width = 2 * 8 = 16, center offset = -8
        assert_eq!(glyphs[0].offset_px[0], -8.0);
        assert_eq!(glyphs[1].offset_px[0], 0.0); // -8 + 8
        assert_eq!(glyphs[0].offset_px[1], -10.0);
    }

    #[test]
    fn test_layout_with_xoffs() {
        let params = TextParams {
            position: [0.0, 0.0],
            text: "X".to_string(),
            color: [0.0, 0.0, 0.0, 1.0],
            color_index: 0,
            scale: 1.0,
            hjust: HJust::Left,
            vjust: VJust::Bottom,
            xoffs: 2, // 2 char widths right
            yoffs: 0,
            disp_prio: 0,
            dis: 10,
        };
        let glyphs = layout_text(&params);
        assert_eq!(glyphs[0].offset_px[0], 16.0); // 2 * 8 = 16
        assert_eq!(glyphs[0].offset_px[1], -10.0);
    }

    #[test]
    fn test_layout_light_text() {
        let glyphs = layout_light_text([1000.0, 2000.0], "Fl(3)G", [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(glyphs.len(), 6);
        // First glyph offset: xoffs=2 → 16px
        assert_eq!(glyphs[0].offset_px[0], 16.0);
        // VJust center with char_height=8 → offset = -6 (baseline adjusted)
        assert_eq!(glyphs[0].offset_px[1], -6.0);
    }

    #[test]
    fn test_layout_scaled() {
        let params = TextParams {
            position: [0.0, 0.0],
            text: "AB".to_string(),
            color: [0.0, 0.0, 0.0, 1.0],
            color_index: 0,
            scale: 2.0,
            hjust: HJust::Left,
            vjust: VJust::Bottom,
            xoffs: 0,
            yoffs: 0,
            disp_prio: 0,
            dis: 10,
        };
        let glyphs = layout_text(&params);
        assert_eq!(glyphs.len(), 2);
        // Scaled size
        assert_eq!(glyphs[0].size_px, [16.0, 16.0]); // 8 * 2
        // Scaled advance
        assert_eq!(glyphs[1].offset_px[0], 16.0); // 8 * 2
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
