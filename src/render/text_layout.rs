//! Text layout engine for converting strings to glyph instances.
//!
//! Converts text strings with positioning parameters into LabelGlyphInstance
//! arrays ready for GPU rendering.

use super::label::{glyph_uv_rect, LabelGlyphInstance};

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
}

impl Default for TextParams {
    fn default() -> Self {
        Self {
            position: [0.0, 0.0],
            text: String::new(),
            color: [0.0, 0.0, 0.0, 1.0], // CHBLK default
            scale: 1.0,
            hjust: HJust::Left,
            vjust: VJust::Bottom,
            xoffs: 0,
            yoffs: 0,
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
        scale: 1.0,
        hjust: HJust::Left,
        vjust: VJust::Center,
        xoffs: 2, // 2 char widths right of symbol
        yoffs: 0,
    })
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
pub fn declutter_labels(glyphs: &mut Vec<LabelGlyphInstance>) {
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
    let rects: Vec<LabelRect> = groups.iter().map(|&(start, count)| {
        let mut min_x = f32::MAX;
        let mut min_y = f32::MAX;
        let mut max_x = f32::MIN;
        let mut max_y = f32::MIN;
        for g in &glyphs[start..start + count] {
            let gx = g.position[0] + g.offset_px[0];
            let gy = g.position[1] + g.offset_px[1];
            min_x = min_x.min(gx);
            min_y = min_y.min(gy);
            max_x = max_x.max(gx + g.size_px[0]);
            max_y = max_y.max(gy + g.size_px[1]);
        }
        LabelRect { min_x, min_y, max_x, max_y }
    }).collect();

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
fn estimate_label_bounds(params: &TextParams) -> LabelRect {
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

    let x = params.position[0] + xadjust;
    let y = params.position[1] + yadjust;

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
pub fn declutter_and_layout_labels(candidates: &[TextParams]) -> Vec<LabelGlyphInstance> {
    if candidates.is_empty() {
        return Vec::new();
    }

    // Phase 1: Cheap AABB pre-check using estimated bounds
    let estimated_rects: Vec<LabelRect> = candidates.iter()
        .map(|p| estimate_label_bounds(p))
        .collect();

    let mut accepted_rects: Vec<LabelRect> = Vec::new();
    let mut keep = vec![false; candidates.len()];

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

    // Phase 2: Full layout only for accepted labels
    let mut glyphs = Vec::new();
    for (i, params) in candidates.iter().enumerate() {
        if keep[i] {
            glyphs.extend(layout_text(params));
        }
    }

    glyphs
}

/// Declutter sounding instances by removing overlapping soundings.
///
/// Shallow soundings (lower depth) have higher priority per S-52.
/// Each sounding's AABB is estimated from its digit count.
pub fn declutter_soundings(soundings: &mut Vec<super::text::SoundingInstance>) {
    if soundings.len() < 2 {
        return;
    }

    // Sort by depth (shallowest first = highest priority per S-52)
    soundings.sort_by(|a, b| a.depth.partial_cmp(&b.depth).unwrap_or(std::cmp::Ordering::Equal));

    // Estimate AABB for each sounding (digit_count * cell_width, cell_height)
    let sounding_rects: Vec<LabelRect> = soundings.iter().map(|s| {
        let digit_count = ((s.flags >> 5) & 0x7) as f32;
        let has_decimal = (s.flags & 0x10) != 0;
        let width = (digit_count + if has_decimal { 1.5 } else { 0.0 }) * CELL_W;
        let height = CELL_H;
        LabelRect {
            min_x: s.position[0],
            min_y: s.position[1] - height,
            max_x: s.position[0] + width,
            max_y: s.position[1],
        }
    }).collect();

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
            scale: 1.0,
            hjust: HJust::Left,
            vjust: VJust::Bottom,
            xoffs: 0,
            yoffs: 0,
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
            scale: 1.0,
            hjust: HJust::Center,
            vjust: VJust::Bottom,
            xoffs: 0,
            yoffs: 0,
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
            scale: 1.0,
            hjust: HJust::Left,
            vjust: VJust::Bottom,
            xoffs: 2, // 2 char widths right
            yoffs: 0,
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
            scale: 2.0,
            hjust: HJust::Left,
            vjust: VJust::Bottom,
            xoffs: 0,
            yoffs: 0,
        };
        let glyphs = layout_text(&params);
        assert_eq!(glyphs.len(), 2);
        // Scaled size
        assert_eq!(glyphs[0].size_px, [16.0, 16.0]); // 8 * 2
        // Scaled advance
        assert_eq!(glyphs[1].offset_px[0], 16.0); // 8 * 2
    }
}
