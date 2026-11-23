//! The wind forecast, drawn over the chart as wind barbs.
//!
//! Barbs rather than arrows because that is what a mariner already reads:
//! the shaft points the way the wind comes *from*, and the feathers count
//! it — a pennant for fifty knots, a full barb for ten, a half for five,
//! with a bare circle for calm. Colour carries the same number again, so
//! strength reads at a glance from the helm without counting anything.
//!
//! The colour comes from [`super::spectrum`], which is also what the weather
//! sheet's wind lane uses. That is deliberate: a barb on the water and a
//! column in the sheet are the same colour for the same wind, so the eye can
//! move between the two without translating.
//!
//! Which *hour* is drawn is not this module's business either. The sheet owns
//! the cursor and the renderer samples the field at it, so scrubbing the
//! sheet moves the barbs.

use egui::{Color32, Context};

use super::spectrum;
use super::ui_batch::Batch;

/// The wind as a wash of colour over the chart, the way Windy draws it.
///
/// Barbs answer "which way, how hard, exactly here". A fill answers "where is
/// the breeze and where is the gale" in one glance, which is the question you
/// ask when you are deciding whether to go at all. Orca carries both, and the
/// teardown in `doc/orca-weather-overlay-teardown.md` shows it feeding its map
/// overlay from the very spectrum that colours its lanes — the same trick
/// [`super::spectrum`] was written for here.
///
/// Sampled on a screen-space grid rather than the forecast's own, so the cost
/// is bounded by the window and not by how coarse or fine the model happens to
/// be. The corners are shared and the GPU does the gradient between them, so
/// the whole layer is one mesh of a few thousand vertices — cheaper than the
/// barbs it sits under.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WindFill {
    /// Where each lattice corner landed on screen, logical points, row-major.
    /// Carried in full rather than as an origin and a step, because the
    /// lattice is laid out in Mercator and projected: only an untilted camera
    /// maps that to an evenly spaced screen grid.
    pub screen: Vec<[f32; 2]>,
    pub cols: usize,
    pub rows: usize,
    /// `cols × rows` wind speeds in knots, row-major. `None` where the field
    /// does not reach.
    pub kt: Vec<Option<f32>>,
    /// Same order: corners just past the field's edge, which carry the
    /// edge's wind but are drawn fully transparent. Without them a cell
    /// straddling the edge had to be dropped whole, and the wash ended in a
    /// staircase of squares; with them it fades out across the last cell.
    pub outside: Vec<bool>,
    /// How opaque to draw it, 0..1 — the user's choice, from Display.
    pub opacity: f32,
}

impl WindFill {
    pub fn is_empty(&self) -> bool {
        self.cols < 2
            || self.rows < 2
            || self.screen.len() < self.cols * self.rows
            || self.kt.len() < self.cols * self.rows
            || self.kt.iter().zip(self.outside.iter().chain(std::iter::repeat(&false))).all(|(v, &out)| v.is_none() || out)
    }
}

/// The fill's opacity, held to a range where the colour still reads and a
/// depth contour, a wreck and a buoy all still read straight through it.
/// Orca's own overlay is a raster layer with an opacity for exactly this
/// reason: a weather layer that hides a rock is worse than no weather layer.
fn fill_alpha(opacity: f32) -> u8 {
    (opacity.clamp(0.1, 0.9) * 255.0).round() as u8
}

/// Draw the wind fill. Below everything — it is a wash, not a mark.
pub fn draw_fill(ctx: &Context, fill: &WindFill) {
    if fill.is_empty() {
        return;
    }
    let painter = ctx.layer_painter(egui::LayerId::background());
    let mut batch = Batch::new(ctx);
    // Colour is taken from the speed at each corner, not interpolated between
    // colours: the ramp bends at every Beaufort stop, and mixing two colours
    // across a cell would slide the boundary off the number it stands for.
    let alpha = fill_alpha(fill.opacity);
    let colours: Vec<Option<Color32>> = fill
        .kt
        .iter()
        .enumerate()
        .map(|(i, kt)| {
            kt.map(|kt| {
                let c = spectrum::WIND_KT.paint(kt);
                let a = if fill.outside.get(i).copied().unwrap_or(false) { 0 } else { alpha };
                Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
            })
        })
        .collect();
    batch.grid(&fill.screen, fill.cols, fill.rows, &colours);
    batch.paint(&painter);
}

/// One barb, already projected by the renderer.
#[derive(Debug, Clone, PartialEq)]
pub struct WindBarb {
    /// Logical points, the same space as the route overlay.
    pub screen: [f32; 2],
    /// Where the wind blows *from*, degrees clockwise from screen-up (true
    /// when the chart is north-up).
    pub from_deg: f32,
    pub kt: f32,
    /// The gust at the same place and hour, when the forecast carries one.
    pub gust_kt: Option<f32>,
    /// How strongly to draw it, 0..1: the zoom fades barbs in and out
    /// between octaves of the lattice rather than popping them.
    pub alpha: f32,
    /// How strongly to print its figures, 0..1 — zero for the barbs that
    /// carry none.
    pub label: f32,
}

/// Below this the barb is a circle: there is no honest feather for a
/// breath of air, and the standard says so too.
const CALM_KT: f32 = 2.5;
const SHAFT_PX: f32 = 26.0;
const FEATHER_PX: f32 = 9.0;
const FEATHER_GAP: f32 = 4.5;
const STROKE: f32 = 2.0;
/// How far the outline reaches past the stroke on each side.
const HALO: f32 = 1.2;

/// Speed to colour. One line, because the ramp lives in one place now.
fn colour_for(kt: f32) -> Color32 {
    spectrum::WIND_KT.paint(kt)
}

/// What a barb's label says: the mean, and the gust when it is worth
/// saying — `12 G18`, the way a METAR or a weather routing report writes
/// it, so no key is needed.
pub fn label_text(kt: f32, gust_kt: Option<f32>) -> String {
    let mean = kt.round().max(0.0) as i32;
    match gust_kt.map(|g| g.round() as i32) {
        Some(g) if g > mean => format!("{mean} G{g}"),
        _ => mean.to_string(),
    }
}

/// Draw the field. Under the vessels and the routes: weather is background,
/// the boat and her plan are not.
///
/// Every barb goes into one mesh. Five hundred of them, each some five
/// separate strokes, is more shapes than egui's tessellator wants to see in a
/// frame on a Pi; see [`super::ui_batch`].
///
/// Each barb is drawn twice: first a dark outline a little wider than the
/// stroke, then the colour on top. A saturated yellow on the pale Day chart,
/// or a blue on its blue water, has almost no contrast of its own; the
/// outline gives every colour an edge, the way map labels carry a halo.
/// All the outlines go down before any colour so one barb's outline never
/// cuts across its neighbour.
pub fn draw(ctx: &Context, barbs: &[WindBarb]) {
    if barbs.is_empty() {
        return;
    }
    let theme = super::theme::current();
    let painter = ctx.layer_painter(egui::LayerId::background());
    let mut batch = Batch::new(ctx);
    // A shaft and a feather or three apiece, twice over. Deliberately a
    // little short: over-reserving here cost more in the allocator than the
    // one regrowth it saved, which is the sort of thing only a measurement
    // tells you.
    batch.reserve(barbs.len() * 8);
    for barb in barbs {
        let halo = theme.halo.gamma_multiply(0.75 * barb.alpha.clamp(0.0, 1.0));
        draw_one(&mut batch, barb, halo, STROKE + 2.0 * HALO);
    }
    for barb in barbs {
        let colour = colour_for(barb.kt).gamma_multiply(barb.alpha.clamp(0.0, 1.0));
        draw_one(&mut batch, barb, colour, STROKE);
    }
    batch.paint(&painter);

    // The figures, on the downwind side of the station point where no
    // feather ever reaches, in the palette's ink over a halo of its ground.
    let font = egui::FontId::proportional(12.0);
    for barb in barbs {
        let strength = barb.label.min(barb.alpha).clamp(0.0, 1.0);
        if strength <= 0.02 {
            continue;
        }
        let rad = (barb.from_deg as f64).to_radians();
        let upwind = egui::vec2(rad.sin() as f32, -(rad.cos() as f32));
        let galley = painter.layout_no_wrap(
            label_text(barb.kt, barb.gust_kt),
            font.clone(),
            Color32::PLACEHOLDER,
        );
        let at = egui::pos2(barb.screen[0], barb.screen[1])
            - upwind * (label_reach(galley.size(), upwind) + 4.0);
        let top_left = at - galley.size() * 0.5;
        let halo = theme.ground.gamma_multiply(0.85 * strength);
        for d in [egui::vec2(-1.0, 0.0), egui::vec2(1.0, 0.0), egui::vec2(0.0, -1.0), egui::vec2(0.0, 1.0)] {
            painter.galley(top_left + d, galley.clone(), halo);
        }
        painter.galley(top_left, galley, theme.ink.gamma_multiply(strength));
    }
}

/// How far a label's centre must sit from the station point, along `dir`,
/// for the label's box to just clear it: the distance from a box's centre to
/// its edge in that direction.
fn label_reach(size: egui::Vec2, dir: egui::Vec2) -> f32 {
    let (hx, hy) = (size.x * 0.5, size.y * 0.5);
    let tx = if dir.x.abs() > 1e-3 { hx / dir.x.abs() } else { f32::INFINITY };
    let ty = if dir.y.abs() > 1e-3 { hy / dir.y.abs() } else { f32::INFINITY };
    tx.min(ty)
}

fn draw_one(batch: &mut Batch, barb: &WindBarb, colour: Color32, w: f32) {
    let at = egui::pos2(barb.screen[0], barb.screen[1]);

    if barb.kt < CALM_KT {
        batch.circle_outline(at, 3.0, w, colour);
        return;
    }

    // The shaft runs from the position towards where the wind comes from,
    // which is the convention every weather chart uses.
    let rad = (barb.from_deg as f64).to_radians();
    let dir = egui::vec2(rad.sin() as f32, -(rad.cos() as f32));
    let tip = at + dir * SHAFT_PX;
    // The outline pass runs a little past each end so the halo closes round
    // the shaft's tips too.
    let over = (w - STROKE) * 0.5;
    batch.line(at - dir * over, tip + dir * over, w, colour);

    // Feathers hang off the outer end, working inwards. They sit on the side
    // that makes the barb read the same in both hemispheres here — northern
    // convention, which is where this boat sails.
    let side = egui::vec2(-dir.y, dir.x);
    let mut remaining = (barb.kt / 5.0).round() * 5.0; // barbs quantise to 5 kt
    let mut along = 0.0f32;

    while remaining >= 50.0 {
        let base = tip - dir * along;
        let inner = base - dir * FEATHER_GAP * 1.6;
        let pennant = [base, inner, base + side * FEATHER_PX];
        if over > 0.0 {
            // A filled pennant gets its halo as an outline round the triangle.
            for e in 0..3 {
                batch.line(pennant[e], pennant[(e + 1) % 3], w - STROKE, colour);
            }
        }
        batch.convex(&pennant, colour);
        along += FEATHER_GAP * 2.0;
        remaining -= 50.0;
    }
    while remaining >= 10.0 {
        let base = tip - dir * along;
        batch.line(base, base + side * (FEATHER_PX + over), w, colour);
        along += FEATHER_GAP;
        remaining -= 10.0;
    }
    if remaining >= 5.0 {
        // A half barb never sits at the very tip — that reads as a full one.
        if along == 0.0 {
            along = FEATHER_GAP;
        }
        let base = tip - dir * along;
        batch.line(base, base + side * (FEATHER_PX * 0.5 + over), w, colour);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vertices one barb's colour pass draws, as they reach the screen.
    fn drawn(kt: f32) -> Vec<egui::epaint::Vertex> {
        let ctx = egui::Context::default();
        let barb = WindBarb {
            screen: [100.0, 100.0],
            // From the north, so the shaft runs straight up the screen and
            // the feathers hang off to the right.
            from_deg: 0.0,
            kt,
            gust_kt: None,
            alpha: 1.0,
            label: 0.0,
        };
        let out = ctx.run(Default::default(), |ctx| {
            let painter = ctx.layer_painter(egui::LayerId::background());
            let mut batch = Batch::new(ctx);
            draw_one(&mut batch, &barb, Color32::WHITE, STROKE);
            batch.paint(&painter);
        });
        out.shapes
            .into_iter()
            .find_map(|c| match c.shape {
                egui::Shape::Mesh(m) => Some(m.vertices),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Read a drawn barb back as (pennants, full barbs, half barbs), plus
    /// how far each feather's root sits from the shaft's outer tip.
    ///
    /// A batched line is eight vertices, its first one the transparent rim;
    /// a pennant is six, its first one solid. The first line is the shaft.
    fn feathers(v: &[egui::epaint::Vertex]) -> ((usize, usize, usize), Vec<f32>) {
        let mid = |a: egui::Pos2, b: egui::Pos2| egui::pos2((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
        let tip = egui::pos2(100.0, 100.0 - SHAFT_PX);
        let (mut pennants, mut fulls, mut halves) = (0, 0, 0);
        let mut roots = Vec::new();
        let mut i = 8;
        while i < v.len() {
            if v[i].color.a() == 0 {
                let a = mid(v[i + 1].pos, v[i + 2].pos);
                let b = mid(v[i + 5].pos, v[i + 6].pos);
                if (b - a).length() > FEATHER_PX * 0.75 {
                    fulls += 1;
                } else {
                    halves += 1;
                }
                roots.push((a - tip).length());
                i += 8;
            } else {
                pennants += 1;
                i += 6;
            }
        }
        ((pennants, fulls, halves), roots)
    }

    /// The feather count is the whole point of a barb: it must quantise the
    /// way the convention says, or the display lies about the wind. Read off
    /// what `draw_one` actually draws — the test this replaced recomputed
    /// the rule in a closure of its own and never called the drawing code.
    #[test]
    fn barb_feathers_quantise_to_the_convention() {
        for (kt, want) in [
            (5.0, (0, 0, 1)),
            (10.0, (0, 1, 0)),
            (15.0, (0, 1, 1)),
            (25.0, (0, 2, 1)),
            (50.0, (1, 0, 0)),
            (65.0, (1, 1, 1)),
            (110.0, (2, 1, 0)),
            // Rounding to the nearest five, not truncation: 12 kt is a full
            // barb, 13 kt is a full barb and a half, 47.6 is a pennant.
            (12.0, (0, 1, 0)),
            (13.0, (0, 1, 1)),
            (47.6, (1, 0, 0)),
        ] {
            let v = drawn(kt);
            assert!(v.len() >= 8, "{kt} kt drew no shaft");
            assert_eq!(feathers(&v).0, want, "{kt} kt");
        }
    }

    /// A half barb at the very tip reads as a full one at a glance, so on
    /// its own it is set one gap in; after full barbs it follows them.
    #[test]
    fn a_lone_half_barb_stands_off_the_tip() {
        let (_, roots) = feathers(&drawn(5.0));
        assert_eq!(roots.len(), 1);
        assert!((roots[0] - FEATHER_GAP).abs() < 0.01, "half barb {} from the tip", roots[0]);

        let (_, roots) = feathers(&drawn(25.0));
        assert!(roots[0].abs() < 0.01, "the first full barb belongs at the tip: {roots:?}");
        assert!((roots[2] - 2.0 * FEATHER_GAP).abs() < 0.01, "{roots:?}");
    }

    /// A calm is a circle round the station point and nothing else — no
    /// shaft pointing in an invented direction.
    #[test]
    fn a_calm_is_a_circle_with_no_shaft() {
        let v = drawn(1.0);
        assert!(!v.is_empty());
        let centre = egui::pos2(100.0, 100.0);
        let reach = v.iter().map(|p| (p.pos - centre).length()).fold(0.0f32, f32::max);
        assert!(reach < 3.0 + STROKE + 1.0, "something reaches {reach} points out");
    }

    /// A barb on the water and a column in the sheet must agree, or the two
    /// displays are quietly telling different stories.
    #[test]
    fn a_barb_takes_its_colour_from_the_shared_ramp() {
        // `paint` is what the sheet's wind lane calls; comparing with it
        // rather than with the bare ramp keeps this true under any palette,
        // whichever theme is live when the test runs.
        for kt in [0.0f32, 7.0, 18.5, 33.0, 70.0] {
            assert_eq!(colour_for(kt), spectrum::WIND_KT.paint(kt));
        }
    }

    /// Mean and gust in the form a sailor reads without a key; no gust
    /// when there is none, or when it is no stronger than the mean.
    #[test]
    fn the_label_says_mean_and_gust() {
        assert_eq!(label_text(12.4, Some(17.6)), "12 G18");
        assert_eq!(label_text(12.4, None), "12");
        assert_eq!(label_text(12.4, Some(12.2)), "12");
        assert_eq!(label_text(-0.3, None), "0");
    }

    /// A label pushed along any direction just clears the station point.
    #[test]
    fn a_label_clears_its_own_barb() {
        let size = egui::vec2(40.0, 14.0);
        assert!((label_reach(size, egui::vec2(1.0, 0.0)) - 20.0).abs() < 1e-4);
        assert!((label_reach(size, egui::vec2(0.0, -1.0)) - 7.0).abs() < 1e-4);
        let diag = egui::vec2(1.0, 1.0).normalized();
        assert!((label_reach(size, diag) - 7.0 * 2f32.sqrt()).abs() < 1e-3);
    }

    /// The wash may never be drawn so faint it is pointless nor so solid it
    /// hides a rock, whatever the setting says.
    #[test]
    fn the_fill_opacity_is_held_inside_its_band() {
        assert_eq!(fill_alpha(0.5), 128);
        assert_eq!(fill_alpha(1.0), 230, "fully opaque would hide the chart");
        assert_eq!(fill_alpha(0.0), 26, "fully clear would hide the weather");
        assert_eq!(fill_alpha(f32::INFINITY), 230);
    }

    /// An empty fill is skipped before it reaches egui, where a mesh with no
    /// triangles panics the renderer.
    #[test]
    fn a_fill_with_nothing_to_draw_is_empty() {
        let grid = |kt: Vec<Option<f32>>, outside: Vec<bool>| WindFill {
            screen: vec![[0.0, 0.0]; 4],
            cols: 2,
            rows: 2,
            kt,
            outside,
            opacity: 0.5,
        };
        assert!(!grid(vec![Some(5.0); 4], vec![false; 4]).is_empty());
        // A missing `outside` list means every corner is inside.
        assert!(!grid(vec![Some(5.0); 4], Vec::new()).is_empty());
        assert!(grid(vec![None; 4], vec![false; 4]).is_empty());
        // Wind only on corners past the edge: nothing visible.
        assert!(grid(vec![Some(5.0); 4], vec![true; 4]).is_empty());
        // Fewer speeds than corners, or a single row: not a grid.
        assert!(grid(vec![Some(5.0); 3], vec![false; 3]).is_empty());
        assert!(WindFill { rows: 1, ..grid(vec![Some(5.0); 4], vec![false; 4]) }.is_empty());
    }
}
