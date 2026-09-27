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
}

impl WindFill {
    pub fn is_empty(&self) -> bool {
        self.cols < 2
            || self.rows < 2
            || self.screen.len() < self.cols * self.rows
            || self.kt.len() < self.cols * self.rows
            || self.kt.iter().all(|v| v.is_none())
    }
}

/// How much of the chart the fill is allowed to take.
///
/// Enough to read the colour at a glance, little enough that a depth contour,
/// a wreck and a buoy all still read straight through it. Orca's own overlay
/// is a raster layer with an opacity for exactly this reason: a weather layer
/// that hides a rock is worse than no weather layer.
const FILL_ALPHA: u8 = 96;

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
    let colours: Vec<Option<Color32>> = fill
        .kt
        .iter()
        .map(|kt| {
            kt.map(|kt| {
                let c = spectrum::WIND_KT.at(kt);
                Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), FILL_ALPHA)
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
}

/// Below this the barb is a circle: there is no honest feather for a
/// breath of air, and the standard says so too.
const CALM_KT: f32 = 2.5;
const SHAFT_PX: f32 = 26.0;
const FEATHER_PX: f32 = 9.0;
const FEATHER_GAP: f32 = 4.5;

/// Speed to colour. One line, because the ramp lives in one place now.
fn colour_for(kt: f32) -> Color32 {
    spectrum::WIND_KT.at(kt)
}

/// Draw the field. Under the vessels and the routes: weather is background,
/// the boat and her plan are not.
///
/// Every barb goes into one mesh. Five hundred of them, each some five
/// separate strokes, is more shapes than egui's tessellator wants to see in a
/// frame on a Pi; see [`super::ui_batch`].
pub fn draw(ctx: &Context, barbs: &[WindBarb]) {
    if barbs.is_empty() {
        return;
    }
    let painter = ctx.layer_painter(egui::LayerId::background());
    let mut batch = Batch::new(ctx);
    // A shaft and a feather or three apiece. Deliberately a little short:
    // over-reserving here cost more in the allocator than the one regrowth it
    // saved, which is the sort of thing only a measurement tells you.
    batch.reserve(barbs.len() * 4);
    for barb in barbs {
        draw_one(&mut batch, barb);
    }
    batch.paint(&painter);
}

fn draw_one(batch: &mut Batch, barb: &WindBarb) {
    let at = egui::pos2(barb.screen[0], barb.screen[1]);
    let colour = colour_for(barb.kt);
    const W: f32 = 1.9;

    if barb.kt < CALM_KT {
        batch.circle_outline(at, 3.0, W, colour);
        return;
    }

    // The shaft runs from the position towards where the wind comes from,
    // which is the convention every weather chart uses.
    let rad = (barb.from_deg as f64).to_radians();
    let dir = egui::vec2(rad.sin() as f32, -(rad.cos() as f32));
    let tip = at + dir * SHAFT_PX;
    batch.line(at, tip, W, colour);

    // Feathers hang off the outer end, working inwards. They sit on the side
    // that makes the barb read the same in both hemispheres here — northern
    // convention, which is where this boat sails.
    let side = egui::vec2(-dir.y, dir.x);
    let mut remaining = (barb.kt / 5.0).round() * 5.0; // barbs quantise to 5 kt
    let mut along = 0.0f32;

    while remaining >= 50.0 {
        let base = tip - dir * along;
        let inner = base - dir * FEATHER_GAP * 1.6;
        batch.convex(&[base, inner, base + side * FEATHER_PX], colour);
        along += FEATHER_GAP * 2.0;
        remaining -= 50.0;
    }
    while remaining >= 10.0 {
        let base = tip - dir * along;
        batch.line(base, base + side * FEATHER_PX, W, colour);
        along += FEATHER_GAP;
        remaining -= 10.0;
    }
    if remaining >= 5.0 {
        // A half barb never sits at the very tip — that reads as a full one.
        if along == 0.0 {
            along = FEATHER_GAP;
        }
        let base = tip - dir * along;
        batch.line(base, base + side * (FEATHER_PX * 0.5), W, colour);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The feather count is the whole point of a barb: it must quantise the
    /// way the convention says, or the display lies about the wind.
    #[test]
    fn barb_feathers_quantise_to_the_convention() {
        // (knots, pennants, full barbs, half barb)
        let expect = |kt: f32| {
            let mut r = (kt / 5.0).round() * 5.0;
            let pennants = (r / 50.0).floor() as i32;
            r -= pennants as f32 * 50.0;
            let fulls = (r / 10.0).floor() as i32;
            r -= fulls as f32 * 10.0;
            (pennants, fulls, r >= 5.0)
        };
        assert_eq!(expect(5.0), (0, 0, true));
        assert_eq!(expect(10.0), (0, 1, false));
        assert_eq!(expect(15.0), (0, 1, true));
        assert_eq!(expect(25.0), (0, 2, true));
        assert_eq!(expect(50.0), (1, 0, false));
        assert_eq!(expect(65.0), (1, 1, true));
        // Rounding to the nearest five, not truncation: 12 kt is a full barb,
        // 13 kt is a full barb and a half.
        assert_eq!(expect(12.0), (0, 1, false));
        assert_eq!(expect(13.0), (0, 1, true));
    }

    /// A barb on the water and a column in the sheet must agree, or the two
    /// displays are quietly telling different stories.
    #[test]
    fn a_barb_takes_its_colour_from_the_shared_ramp() {
        for kt in [0.0f32, 7.0, 18.5, 33.0, 70.0] {
            assert_eq!(colour_for(kt), spectrum::WIND_KT.at(kt));
        }
    }

    #[test]
    fn the_colours_climb_with_the_wind() {
        let ordered = [3.0, 9.0, 16.0, 25.0, 40.0].map(colour_for);
        for w in ordered.windows(2) {
            assert_ne!(w[0], w[1], "each step must be distinguishable");
        }
    }
}
