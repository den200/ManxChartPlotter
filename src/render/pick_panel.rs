//! Lay out the object-query bubble.
//!
//! Turns the objects [`crate::pick`] found into a panel and its text. The panel
//! is a screen-space rounded rectangle; the text is ordinary chart glyphs
//! anchored to the *picked world position* with pixel offsets, so the bubble
//! stays on the object as the chart pans under it rather than floating free.

use crate::pick::PickedObject;
use crate::render::label::LabelGlyphInstance;
use crate::render::overlay::{panel, OverlayVertex};
use crate::render::text_layout::{layout_text, text_width_px, HJust, TextParams, VJust, SPACE_STANDARD};

/// Bubble text size in points. `NAVCORE_UI_POINTS` overrides.
///
/// Points, not pixels, because the panel has to be legible on a chart plotter
/// at arm's length. The first version fixed the glyph scale at a constant and
/// so shrank with pixel density: on a 2x display the text came out around a
/// 3-pixel cap height, unreadable, while looking fine in a 1x screenshot.
const DEFAULT_POINTS: f32 = 12.0;
/// Millimetres per point.
const MM_PER_POINT: f32 = 25.4 / 72.0;

/// Everything the panel's layout is measured in, derived from the display.
struct Metrics {
    /// `TextParams::scale` that renders at the requested point size.
    scale: f32,
    line_h: f32,
    pad: f32,
    max_w: f32,
    /// Characters per line for the note wrapper.
    wrap_cols: usize,
}

fn metrics(ppmm: f32) -> Metrics {
    let points = std::env::var("NAVCORE_UI_POINTS")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|p| p.is_finite() && *p >= 6.0 && *p <= 48.0)
        .unwrap_or(DEFAULT_POINTS);
    // An em at this point size, in physical pixels, and the glyph scale that
    // produces it: `em_px` is `CELL_H * scale / cap_height`.
    let em = points * MM_PER_POINT * ppmm.max(1.0);
    let scale = em * super::font::atlas().cap_height / super::text_layout::CELL_H;
    Metrics {
        scale,
        line_h: em * 1.28,
        pad: em * 0.7,
        max_w: em * 26.0,
        wrap_cols: 44,
    }
}
/// Objects listed before the bubble stops and says how many are left.
const MAX_OBJECTS: usize = 4;
/// Lines of a chart note shown before it is elided.
const MAX_NOTE_LINES: usize = 6;

/// One laid-out line of the bubble.
struct Line {
    text: String,
    bold: bool,
    indent: f32,
}

/// The bubble's geometry: a panel and the glyphs that sit on it.
pub struct PickPanel {
    pub overlay: Vec<OverlayVertex>,
    pub glyphs: Vec<LabelGlyphInstance>,
    /// Screen rectangle, so a click can be tested against it.
    pub rect: [f32; 4],
}

/// Build the bubble for `objects`, anchored at the picked world position.
///
/// `anchor_screen` is where that world position currently projects to, and
/// `viewport` the window size; the panel is nudged to stay on screen, which is
/// why both are needed.
pub fn build(
    objects: &[PickedObject],
    anchor_world: [f32; 2],
    anchor_screen: [f32; 2],
    viewport: [f32; 2],
    color_index: u32,
    ppmm: f32,
) -> PickPanel {
    let m = metrics(ppmm);
    let mut lines = compose(objects, m.wrap_cols, m.pad);

    // Never taller than the window. At 12 points a harbour click can easily
    // compose sixty lines — more than fits on a plotter screen — so the list is
    // cut to what there is room for and the tail is reported as a count.
    let budget = (((viewport[1] - 8.0 - 2.0 * m.pad) / m.line_h).floor() as usize).max(3);
    if lines.len() > budget {
        let shown = objects
            .iter()
            .take(MAX_OBJECTS)
            .count()
            .min(count_titles(&lines[..budget - 1]));
        lines.truncate(budget - 1);
        lines.push(Line {
            // ASCII only: the label atlas is baked over ASCII and Latin-1, so
            // an ellipsis or an em dash comes out as the missing-glyph box.
            text: format!("+ {} more here", objects.len().saturating_sub(shown)),
            bold: false,
            indent: 0.0,
        });
    }

    let width = lines
        .iter()
        .map(|l| l.indent + text_width_px(&l.text, m.scale, l.bold, SPACE_STANDARD))
        .fold(0.0_f32, f32::max)
        .min(m.max_w - 2.0 * m.pad)
        + 2.0 * m.pad;
    let height = lines.len() as f32 * m.line_h + 2.0 * m.pad;

    // Up and to the right of the click, then pushed back inside the window.
    let mut x = anchor_screen[0] + 14.0;
    let mut y = anchor_screen[1] - height - 14.0;
    if x + width > viewport[0] - 4.0 {
        x = anchor_screen[0] - width - 14.0;
    }
    x = x.max(4.0);
    if y < 4.0 {
        y = (anchor_screen[1] + 14.0).min(viewport[1] - height - 4.0).max(4.0);
    }

    let overlay = panel(x, y, width, height, m.pad * 0.8, 1.5, [0.99, 0.99, 0.96, 0.985]);

    // Glyph offsets are relative to the anchor and measured with y up, while
    // the panel is laid out with y down from the top left, so the text origin
    // is the panel's top-left expressed as an offset from the anchor.
    let ox = x + m.pad - anchor_screen[0];
    let oy = anchor_screen[1] - y - m.pad;

    let mut glyphs = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.text.is_empty() {
            continue;
        }
        let params = TextParams {
            position: anchor_world,
            text: line.text.clone(),
            color: [0.0, 0.0, 0.0, 1.0],
            color_index,
            scale: m.scale,
            hjust: HJust::Left,
            vjust: VJust::Top,
            xoffs: 0,
            yoffs: 0,
            disp_prio: 9,
            dis: 0,
            bold: line.bold,
            space: SPACE_STANDARD,
        };
        let dx = ox + line.indent;
        let dy = oy - (i as f32 + 1.0) * m.line_h;
        for mut g in layout_text(&params) {
            g.offset_px[0] += dx;
            g.offset_px[1] += dy;
            glyphs.push(g);
        }
    }

    PickPanel {
        overlay,
        glyphs,
        rect: [x, y, width, height],
    }
}

/// How many object headings are in these lines, so the "more" count is right
/// after the list has been cut to fit.
fn count_titles(lines: &[Line]) -> usize {
    lines.iter().filter(|l| l.bold && l.indent == 0.0).count()
}

/// The bubble's text, in reading order.
fn compose(objects: &[PickedObject], wrap_cols: usize, indent: f32) -> Vec<Line> {
    let mut lines = Vec::new();
    if objects.is_empty() {
        lines.push(Line {
            text: "No chart object here".into(),
            bold: true,
            indent: 0.0,
        });
        return lines;
    }

    for (i, o) in objects.iter().take(MAX_OBJECTS).enumerate() {
        if i > 0 {
            lines.push(Line { text: String::new(), bold: false, indent: 0.0 });
        }
        lines.push(Line {
            text: format!("{}  ({})", o.title, o.acronym),
            bold: true,
            indent: 0.0,
        });
        lines.push(Line {
            text: format!("1:{}  {}", o.chart_scale, o.chart),
            bold: false,
            indent,
        });
        for (k, v) in &o.attributes {
            lines.push(Line {
                text: format!("{}   {}", k, v),
                bold: false,
                indent,
            });
        }
        for note in &o.notes {
            for (n, line) in wrap(note, wrap_cols).into_iter().enumerate() {
                if n >= MAX_NOTE_LINES {
                    lines.push(Line { text: "...".into(), bold: false, indent });
                    break;
                }
                lines.push(Line { text: line, bold: false, indent: 8.0 });
            }
        }
    }

    if objects.len() > MAX_OBJECTS {
        lines.push(Line { text: String::new(), bold: false, indent: 0.0 });
        lines.push(Line {
            text: format!("+ {} more object(s) here", objects.len() - MAX_OBJECTS),
            bold: false,
            indent: 0.0,
        });
    }
    lines
}

/// Break `text` into lines of at most `cols` characters, on word boundaries.
///
/// The chart notes arrive already broken for a fixed-width margin and carry
/// their own newlines and trailing spaces; both are honoured, then re-wrapped,
/// so a long line does not run off the panel.
fn wrap(text: &str, cols: usize) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.lines() {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        let mut line = String::new();
        for word in para.split_whitespace() {
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > cols {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        if !line.is_empty() {
            out.push(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::senc::FeatureType;

    fn object(title: &str, attrs: &[(&str, &str)], notes: &[&str]) -> PickedObject {
        PickedObject {
            acronym: "TEST".into(),
            title: title.into(),
            chart: "OC-45-TEST".into(),
            chart_scale: 22000,
            geometry: FeatureType::Point,
            attributes: attrs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            notes: notes.iter().map(|n| (*n).to_string()).collect(),
            distance_m: 0.0,
        }
    }

    #[test]
    fn wrap_breaks_on_words_and_drops_blank_lines() {
        let w = wrap("one two three four five", 9);
        assert_eq!(w, vec!["one two", "three", "four five"]);
        assert_eq!(wrap("a\n\n  b  ", 20), vec!["a", "b"]);
        // A single word longer than the limit is not split; it just overflows,
        // which is better than mangling a place name.
        assert_eq!(wrap("Vallensbæklongword", 5), vec!["Vallensbæklongword"]);
    }

    #[test]
    fn empty_pick_says_so_rather_than_drawing_nothing() {
        let lines = compose(&[], 58, 8.0);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].text.contains("No chart object"));
    }

    #[test]
    fn a_pick_lists_title_chart_and_attributes() {
        let lines = compose(&[object("Buoy, cardinal", &[("CATCAM", "north cardinal mark")], &[])], 58, 8.0);
        let text: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert!(text[0].starts_with("Buoy, cardinal"));
        assert!(text[1].contains("1:22000"));
        assert!(text[2].contains("CATCAM"));
        assert!(text[2].contains("north cardinal mark"));
        assert!(lines[0].bold && !lines[2].bold);
    }

    #[test]
    fn long_lists_are_truncated_with_a_count() {
        let objs: Vec<PickedObject> = (0..7).map(|_| object("Depth area", &[], &[])).collect();
        let text = compose(&objs, 58, 8.0)
            .iter()
            .map(|l| l.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("+ 3 more object(s) here"), "{text}");
    }

    #[test]
    fn a_long_note_is_elided() {
        let note = (0..20).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let lines = compose(&[object("Caution area", &[], &[&note])], 58, 8.0);
        assert!(lines.iter().any(|l| l.text == "..."));
        assert!(lines.len() < 20);
    }

    #[test]
    fn the_panel_stays_inside_the_window() {
        let objs = [object("Buoy, cardinal", &[("CATCAM", "north")], &[])];
        // Anchored hard against the right edge and the top.
        let p = build(&objs, [0.0, 0.0], [1990.0, 5.0], [2000.0, 1200.0], 0, 4.0);
        let [x, y, w, h] = p.rect;
        assert!(x >= 4.0 && x + w <= 2000.0 - 4.0, "x {x} w {w}");
        assert!(y >= 4.0 && y + h <= 1200.0 - 4.0, "y {y} h {h}");
        assert_eq!(p.overlay.len(), 6);
        assert!(!p.glyphs.is_empty());
    }
}
