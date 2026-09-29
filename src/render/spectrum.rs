//! Colour ramps for the weather display.
//!
//! Orca's weather sheet does not band its colours — it interpolates a
//! spectrum, and feeds the *same* spectrum to the map overlay, so a column in
//! the sheet and a patch of sea on the chart are the same colour for the same
//! number. That is the part worth copying: one ramp, two surfaces, no
//! translation in the reader's head.
//!
//! The stops are chosen so the boundaries land on numbers a sailor already
//! has opinions about — Beaufort for wind, the reefing decisions for waves —
//! and so that every colour keeps its footing over both the pale Day chart
//! and the near-black Night one. That rules out anything very light and
//! anything very dark; what is left is a saturated ramp that runs blue →
//! green → yellow → orange → red → violet, which is also the order a great
//! many weather products already use, so it needs no explaining. Dusk and
//! Night dim it with the palette ([`Spectrum::paint`]), never by changing
//! the hues: a gale is red in every light.

use egui::Color32;

/// A ramp: value/colour stops in ascending order, linearly interpolated
/// between and clamped outside.
pub struct Spectrum(pub &'static [(f32, [u8; 3])]);

impl Spectrum {
    /// The colour for a value. Below the first stop and above the last, the
    /// end stops hold — a ramp that wrapped or went black at the extremes
    /// would misreport exactly the conditions that matter most.
    pub fn at(&self, v: f32) -> Color32 {
        let stops = self.0;
        debug_assert!(!stops.is_empty(), "a spectrum needs at least one stop");
        if stops.is_empty() {
            return Color32::GRAY;
        }
        if !v.is_finite() || v <= stops[0].0 {
            return rgb(stops[0].1);
        }
        for pair in stops.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            if v <= b.0 {
                let span = b.0 - a.0;
                let f = if span <= 0.0 { 0.0 } else { (v - a.0) / span };
                return mix(rgb(a.1), rgb(b.1), f);
            }
        }
        rgb(stops[stops.len() - 1].1)
    }

    /// The colour to draw a value in on screen: [`Spectrum::at`], brought
    /// down to the live palette. By day that is the ramp itself; at dusk and
    /// at night a full-strength red gale would be the brightest thing on the
    /// bridge, so it falls with the chart (see [`super::theme`]).
    pub fn paint(&self, v: f32) -> Color32 {
        super::theme::current().dim(self.at(v))
    }

    /// The value range the ramp spans, for drawing a key.
    pub fn range(&self) -> (f32, f32) {
        match (self.0.first(), self.0.last()) {
            (Some(a), Some(b)) => (a.0, b.0),
            _ => (0.0, 1.0),
        }
    }
}

fn rgb([r, g, b]: [u8; 3]) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// Blend in linear-ish space by simply mixing the channels. Good enough for a
/// ramp whose stops are close together, and it never produces the muddy grey
/// that a naive mix across distant hues would.
fn mix(a: Color32, b: Color32, f: f32) -> Color32 {
    let f = f.clamp(0.0, 1.0);
    let c = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f).round() as u8;
    Color32::from_rgb(
        c(a.r(), b.r()),
        c(a.g(), b.g()),
        c(a.b(), b.b()),
    )
}

/// Wind, knots. The stops sit on Beaufort: F3 tops out at 10, F4 at 16, F6
/// begins at 22 and is the yacht's first real reef, F8 at 34, F10 at 48, F12
/// at 64.
///
/// Saturated on purpose. The first ramp was mid-saturation, chosen to sit
/// quietly on both the Day and Night charts, and at a third of full opacity
/// it read as a faint stain. Colour on a chart has to be strong to be seen
/// at all in sunlight; at night [`Spectrum::paint`] brings it down with the
/// palette instead. Calm is a deep blue rather than a pale one, so it is not
/// mistaken for the S-52 shallow-water tint underneath.
pub const WIND_KT: Spectrum = Spectrum(&[
    (0.0, [48, 86, 214]),
    (8.0, [0, 150, 200]),
    (12.0, [0, 176, 120]),
    (16.0, [70, 190, 40]),
    (22.0, [240, 200, 0]),
    (27.0, [255, 120, 0]),
    (34.0, [230, 20, 40]),
    (48.0, [200, 0, 140]),
    (64.0, [120, 30, 200]),
]);

/// Significant wave height, metres. One metre is a pleasant day, two is work,
/// four is a gale's sea, six is survival for most cruising boats.
pub const WAVE_M: Spectrum = Spectrum(&[
    (0.0, [48, 110, 214]),
    (0.75, [0, 170, 170]),
    (1.5, [120, 190, 30]),
    (2.5, [255, 150, 0]),
    (4.0, [230, 30, 40]),
    (6.0, [170, 0, 160]),
]);

/// Surface current, knots. Half a knot is worth steering for; two is worth
/// waiting for; four is a gate that opens and shuts.
pub const CURRENT_KT: Spectrum = Spectrum(&[
    (0.0, [60, 120, 210]),
    (0.5, [0, 170, 160]),
    (1.5, [240, 190, 0]),
    (2.5, [255, 110, 0]),
    (4.0, [220, 20, 50]),
]);

/// Precipitation, mm in the hour. Drizzle, rain, heavy rain, deluge.
pub const RAIN_MM: Spectrum = Spectrum(&[
    (0.0, [110, 170, 230]),
    (1.0, [30, 120, 240]),
    (4.0, [20, 60, 200]),
    (10.0, [150, 40, 210]),
]);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stop_returns_its_own_colour_exactly() {
        for &(v, c) in WIND_KT.0 {
            assert_eq!(WIND_KT.at(v), Color32::from_rgb(c[0], c[1], c[2]), "at {v} kt");
        }
    }

    #[test]
    fn between_two_stops_the_colour_is_between_them() {
        // Halfway from the 0 kt blue to the 8 kt teal.
        let c = WIND_KT.at(4.0);
        let a = WIND_KT.at(0.0);
        let b = WIND_KT.at(8.0);
        let between = |x: u8, p: u8, q: u8| x >= p.min(q) && x <= p.max(q);
        assert!(between(c.r(), a.r(), b.r()), "red {} not between", c.r());
        assert!(between(c.g(), a.g(), b.g()), "green {} not between", c.g());
        assert!(between(c.b(), a.b(), b.b()), "blue {} not between", c.b());
        assert_ne!(c, a);
        assert_ne!(c, b);
    }

    #[test]
    fn outside_the_ramp_the_end_stops_hold() {
        assert_eq!(WIND_KT.at(-5.0), WIND_KT.at(0.0));
        assert_eq!(WIND_KT.at(200.0), WIND_KT.at(64.0));
        // A NaN must not paint something arbitrary.
        assert_eq!(WIND_KT.at(f32::NAN), WIND_KT.at(0.0));
    }

    /// The whole point of a ramp is that more wind looks like more wind.
    ///
    /// Not a channel-by-channel climb — the ramp leaves blue for green before
    /// it turns red, and red genuinely dips on the way. What must hold is
    /// that no two readings a reef apart look alike, and that the top of the
    /// scale is unmistakably hotter than the bottom.
    #[test]
    fn the_ramp_separates_every_wind_worth_separating() {
        let sampled: Vec<Color32> = (0..=64).step_by(4).map(|kt| WIND_KT.at(kt as f32)).collect();
        for (i, a) in sampled.iter().enumerate() {
            for b in &sampled[i + 1..] {
                let d = (a.r() as i32 - b.r() as i32).abs()
                    + (a.g() as i32 - b.g() as i32).abs()
                    + (a.b() as i32 - b.b() as i32).abs();
                assert!(d >= 20, "two winds four knots apart look alike: {a:?} vs {b:?}");
            }
        }
        let calm = WIND_KT.at(0.0);
        let storm = WIND_KT.at(56.0);
        assert!(
            storm.r() as i32 - storm.g() as i32 > calm.r() as i32 - calm.g() as i32 + 60,
            "the top of the scale must read hot against the bottom"
        );
    }

    #[test]
    fn every_ramp_spans_the_range_it_advertises() {
        assert_eq!(WIND_KT.range(), (0.0, 64.0));
        assert_eq!(WAVE_M.range(), (0.0, 6.0));
        assert_eq!(CURRENT_KT.range(), (0.0, 4.0));
        assert_eq!(RAIN_MM.range(), (0.0, 10.0));
    }

    /// Nothing in a ramp may be so pale it vanishes on the Day chart nor so
    /// dark it vanishes on the Night one. Checked as luminance staying inside
    /// a usable band.
    #[test]
    fn no_colour_disappears_into_either_palette() {
        for ramp in [&WIND_KT, &WAVE_M, &CURRENT_KT, &RAIN_MM] {
            for &(v, [r, g, b]) in ramp.0 {
                let lum = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
                assert!(
                    (48.0..=200.0).contains(&lum),
                    "stop {v} has luminance {lum:.0}"
                );
            }
        }
    }
}
