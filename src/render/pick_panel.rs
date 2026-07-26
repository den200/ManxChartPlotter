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

/// Text size inside the bubble, in the same units chart labels use.
const SCALE: f32 = 0.85;
const LINE_H: f32 = 15.0;
const PAD: f32 = 10.0;
const MAX_W: f32 = 460.0;
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
) -> PickPanel {
    let lines = compose(objects);

    let width = lines
        .iter()
        .map(|l| l.indent + text_width_px(&l.text, SCALE, l.bold, SPACE_STANDARD))
        .fold(0.0_f32, f32::max)
        .min(MAX_W - 2.0 * PAD)
        + 2.0 * PAD;
    let height = lines.len() as f32 * LINE_H + 2.0 * PAD;

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

    let overlay = panel(x, y, width, height, 7.0, 1.5, [0.99, 0.99, 0.96, 0.985]);

    // Glyph offsets are relative to the anchor and measured with y up, while
    // the panel is laid out with y down from the top left, so the text origin
    // is the panel's top-left expressed as an offset from the anchor.
    let ox = x + PAD - anchor_screen[0];
    let oy = anchor_screen[1] - y - PAD;

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
            scale: SCALE,
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
        let dy = oy - (i as f32 + 1.0) * LINE_H;
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

/// The bubble's text, in reading order.
fn compose(objects: &[PickedObject]) -> Vec<Line> {
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
            indent: 8.0,
        });
        for (k, v) in &o.attributes {
            lines.push(Line {
                text: format!("{}   {}", k, v),
                bold: false,
                indent: 8.0,
            });
        }
        for note in &o.notes {
            for (n, line) in wrap(note, 62).into_iter().enumerate() {
                if n >= MAX_NOTE_LINES {
                    lines.push(Line { text: "…".into(), bold: false, indent: 8.0 });
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
        let lines = compose(&[]);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].text.contains("No chart object"));
    }

    #[test]
    fn a_pick_lists_title_chart_and_attributes() {
        let lines = compose(&[object("Buoy, cardinal", &[("CATCAM", "north cardinal mark")], &[])]);
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
        let text = compose(&objs)
            .iter()
            .map(|l| l.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("+ 3 more object(s) here"), "{text}");
    }

    #[test]
    fn a_long_note_is_elided() {
        let note = (0..20).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let lines = compose(&[object("Caution area", &[], &[&note])]);
        assert!(lines.iter().any(|l| l.text == "…"));
        assert!(lines.len() < 20);
    }

    #[test]
    fn the_panel_stays_inside_the_window() {
        let objs = [object("Buoy, cardinal", &[("CATCAM", "north")], &[])];
        // Anchored hard against the right edge and the top.
        let p = build(&objs, [0.0, 0.0], [1990.0, 5.0], [2000.0, 1200.0], 0);
        let [x, y, w, h] = p.rect;
        assert!(x >= 4.0 && x + w <= 2000.0 - 4.0, "x {x} w {w}");
        assert!(y >= 4.0 && y + h <= 1200.0 - 4.0, "y {y} h {h}");
        assert_eq!(p.overlay.len(), 6);
        assert!(!p.glyphs.is_empty());
    }
}
