//! The label typeface: an SDF glyph atlas baked by `tools/make_font_atlas.py`.
//!
//! navcore used to carry a hand-coded 5x7 bitmap table. It had no glyph above
//! U+007F, so Danish chart names came out as "Vallensbak" and "Brondby", and at
//! the sizes S-52 asks for it read as a dot-matrix printout rather than a chart.
//!
//! An SDF is the right shape for this problem: chart text spans a sounding at
//! 10 px and a place name at 40 px in the same frame, and the size follows the
//! S-52 body size, the display DPI and the zoom, so no single baked size works.
//! One field stays sharp at all of them.

use std::collections::HashMap;
use std::sync::OnceLock;

/// One glyph's placement and texture rectangle.
#[derive(Debug, Clone, Copy)]
pub struct Glyph {
    /// Texture rectangle in normalised UV, `[u0, v0, u1, v1]` with v0 at the top.
    pub uv: [f32; 4],
    /// The quad to draw it in, in em units relative to the pen position, y up
    /// from the baseline: `[x0, y0, x1, y1]`. Includes the SDF padding, so the
    /// quad covers the whole field rather than just the ink.
    pub plane: [f32; 4],
    /// Pen advance in em units.
    pub advance: f32,
}

pub struct FontAtlas {
    pub width: u32,
    pub height: u32,
    /// Single-channel SDF, row-major, one byte per texel.
    pub pixels: Vec<u8>,
    /// Cap height in em — the layout sizes text by cap height, because that is
    /// what the old fixed cell measured and what a chart reader perceives.
    pub cap_height: f32,
    regular: HashMap<char, Glyph>,
    bold: HashMap<char, Glyph>,
}

impl FontAtlas {
    fn load(png: &str, json: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let img = image::open(png)?.to_luma8();
        let (width, height) = img.dimensions();
        let meta: serde_json::Value = serde_json::from_reader(std::io::BufReader::new(
            std::fs::File::open(json)?,
        ))?;

        let cap_height = meta["cap_height"].as_f64().unwrap_or(0.729) as f32;
        let mut weights = [HashMap::new(), HashMap::new()];
        for (slot, name) in ["regular", "bold"].iter().enumerate() {
            let Some(entries) = meta["glyphs"][name].as_object() else {
                continue;
            };
            for (cp, g) in entries {
                let Some(ch) = cp.parse::<u32>().ok().and_then(char::from_u32) else {
                    continue;
                };
                let advance = g["adv"].as_f64().unwrap_or(0.0) as f32;
                // Whitespace has an advance and no ink; keep it so the pen moves.
                let (uv, plane) = match (g.get("uv"), g.get("plane")) {
                    (Some(uv), Some(plane)) => {
                        let n = |v: &serde_json::Value, i: usize| v[i].as_f64().unwrap_or(0.0) as f32;
                        (
                            [
                                n(uv, 0) / width as f32,
                                n(uv, 1) / height as f32,
                                n(uv, 2) / width as f32,
                                n(uv, 3) / height as f32,
                            ],
                            [n(plane, 0), n(plane, 1), n(plane, 2), n(plane, 3)],
                        )
                    }
                    _ => ([0.0; 4], [0.0; 4]),
                };
                weights[slot].insert(ch, Glyph { uv, plane, advance });
            }
        }

        let [regular, bold] = weights;
        if regular.is_empty() {
            return Err("font atlas has no glyphs".into());
        }
        Ok(Self {
            width,
            height,
            pixels: img.into_raw(),
            cap_height,
            regular,
            bold,
        })
    }

    /// The glyph for `c`, falling back to the regular weight and then to `?`
    /// so an unmapped codepoint is visible rather than silently skipped.
    pub fn glyph(&self, c: char, bold: bool) -> Option<&Glyph> {
        let table = if bold { &self.bold } else { &self.regular };
        table
            .get(&c)
            .or_else(|| self.regular.get(&c))
            .or_else(|| table.get(&'?'))
    }

    /// Whether the atlas actually carries `c`, as opposed to substituting for
    /// it. Callers that fold unsupported characters need the exact answer,
    /// which [`glyph`](Self::glyph) deliberately does not give.
    pub fn has_glyph(&self, c: char) -> bool {
        self.regular.contains_key(&c)
    }

    /// Width of `text` in em units.
    pub fn advance_of(&self, text: &str, bold: bool) -> f32 {
        text.chars()
            .filter_map(|c| self.glyph(c, bold))
            .map(|g| g.advance)
            .sum()
    }
}

/// The process-wide label font.
///
/// Layout runs on the tile worker and on the main thread, and both need the
/// metrics; the texture upload needs the same pixels. One lazily-loaded atlas
/// serves all three.
pub fn atlas() -> &'static FontAtlas {
    static ATLAS: OnceLock<FontAtlas> = OnceLock::new();
    ATLAS.get_or_init(|| {
        match FontAtlas::load("assets/fonts/labels.png", "assets/fonts/labels.json") {
            Ok(a) => {
                log::info!(
                    "Label font: {}x{} SDF atlas, {} glyphs, cap height {:.3} em",
                    a.width,
                    a.height,
                    a.regular.len(),
                    a.cap_height
                );
                a
            }
            Err(e) => {
                // Text is not worth aborting the renderer over, but a silent
                // fallback to nothing would look like a layout bug.
                log::error!("Label font: {} — labels will not render", e);
                FontAtlas {
                    width: 1,
                    height: 1,
                    pixels: vec![0],
                    cap_height: 0.729,
                    regular: HashMap::new(),
                    bold: HashMap::new(),
                }
            }
        }
    })
}
