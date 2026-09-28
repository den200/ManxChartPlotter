//! AIS targets on the chart.
//!
//! The target symbols themselves are S-52 — `AISVES01`, `AISSLP01`,
//! `AISDEF01` and the one- and six-minute vector marks — drawn by the chart
//! pipeline in [`super::mariner`]. What is left in this overlay is everything
//! the presentation library has no symbol for:
//!
//! - **The vector line** between a target and its time marks — a line of
//!   arbitrary length, which no symbol can be. Drawn only for a vessel
//!   actually under way: a moored ship with a stale course would otherwise
//!   sprout a line across the harbour.
//! - **The name**, from AIS, or the MMSI when it has not given one.
//! - **Crossed through when lost.** A target that simply vanishes is
//!   indistinguishable from one that was never there; a target that quietly
//!   stays is worse, because it is a ship that is no longer where you think.
//! - **A ring and a CPA read-out** when the closest approach is both near and
//!   soon. This is the only emphasis on the layer, so it means one thing, and
//!   it adds to the S-52 symbol rather than replacing it.

use egui::{Align2, Color32, Context, FontId, Pos2, Stroke, Vec2};

/// One target, resolved by the renderer which owns the camera.
#[derive(Debug, Clone)]
pub struct AisTarget {
    /// Its Signal K context: which vessel, for the collision alarm.
    pub id: String,
    pub screen: [f32; 2],
    /// Hull orientation, radians clockwise from screen-up (north, when
    /// north-up).
    pub heading: Option<f32>,
    /// Course over ground, for the vector.
    pub cog: Option<f32>,
    /// Speed over ground, m/s.
    pub sog: Option<f32>,
    pub label: Option<String>,
    pub under_way: bool,
    pub lost: bool,
    /// Closest approach, when it could be computed: metres and seconds.
    pub cpa: Option<(f32, f32)>,
}

/// How near, and how soon, before a target is called dangerous. Set by the
/// user, because the right answer depends on the water.
#[derive(Debug, Clone, Copy)]
pub struct CpaAlarm {
    pub distance_m: f32,
    pub seconds: f32,
}

impl CpaAlarm {
    /// Both conditions, and the approach still ahead of us. Near-but-in-an-hour
    /// is not a threat, and neither is soon-but-a-mile-off; sounding on either
    /// alone is how a warning stops being read.
    pub fn triggered_by(&self, cpa: Option<(f32, f32)>) -> bool {
        cpa.is_some_and(|(d, t)| d < self.distance_m && t < self.seconds && t > 0.0)
    }
}

/// Roughly how far the S-52 target symbol reaches from its pivot, in points.
/// Used only to keep the overlay's text and rings clear of the glyph — the
/// glyph's real geometry belongs to the presentation library.
const SIZE: f32 = 9.0;
/// How far ahead the course vector reaches.
const VECTOR_MINUTES: f32 = 6.0;

pub fn draw(ctx: &Context, targets: &[AisTarget], mpp: f32, alarm: CpaAlarm) {
    if targets.is_empty() {
        return;
    }
    let painter = ctx.layer_painter(egui::LayerId::background());
    let screen = ctx.screen_rect().expand(SIZE * 4.0);
    let dark = ctx.style().visuals.dark_mode;
    // Enough contrast on both palettes without becoming a third colour.
    let ink = if dark {
        Color32::from_gray(225)
    } else {
        Color32::from_gray(25)
    };

    for target in targets {
        let pos = Pos2::new(target.screen[0], target.screen[1]);
        if !screen.contains(pos) {
            continue;
        }

        // A lost target's CPA is extrapolated from its last report; ringing
        // it red would contradict the strike-through that says so.
        let dangerous = !target.lost && alarm.triggered_by(target.cpa);
        let colour = if target.lost {
            Color32::from_gray(140)
        } else if dangerous {
            Color32::from_rgb(200, 40, 40)
        } else {
            ink
        };
        // The vector's *line*. Its symbol — the target itself — and the one-
        // and six-minute marks along it are S-52 symbols drawn by the chart
        // pipeline; only the line between them is left here, because the
        // presentation library has no symbol for a line of arbitrary length.
        if target.under_way && !target.lost {
            if let (Some(cog), Some(sog)) = (target.cog, target.sog) {
                if sog > 0.2 && mpp > 0.0 {
                    let length = sog * VECTOR_MINUTES * 60.0 / mpp;
                    if length > SIZE {
                        painter.line_segment(
                            [pos, pos + unit(cog) * length],
                            Stroke::new(1.4_f32, colour),
                        );
                    }
                }
            }
        }

        // A target close enough to matter is ringed, so the eye finds it among
        // forty others. The S-52 symbol underneath is unchanged: this adds
        // emphasis rather than replacing symbology.
        if dangerous {
            painter.circle_stroke(pos, SIZE * 1.6, Stroke::new(2.0_f32, colour));
        }

        // A lost target is struck through rather than removed, so it reads as
        // "was here, no longer reporting" instead of silently disappearing.
        if target.lost {
            let d = SIZE * 1.1;
            painter.line_segment(
                [pos + Vec2::new(-d, -d), pos + Vec2::new(d, d)],
                Stroke::new(1.4_f32, colour),
            );
            painter.line_segment(
                [pos + Vec2::new(-d, d), pos + Vec2::new(d, -d)],
                Stroke::new(1.4_f32, colour),
            );
        }

        if let Some(label) = &target.label {
            // Below and to the right of the reported position. The S-52
            // symbol extends *ahead* of that position — its pivot sits at the
            // base of the triangle — so a label placed above would collide
            // with the glyph on any northerly heading.
            painter.text(
                pos + Vec2::new(SIZE + 5.0, SIZE + 2.0),
                Align2::LEFT_CENTER,
                label,
                FontId::proportional(11.0),
                colour,
            );
        }

        // The number that decides whether to alter course, on the one target
        // it matters for. Putting it on every target would bury it.
        if dangerous {
            if let Some((distance, seconds)) = target.cpa {
                painter.text(
                    pos + Vec2::new(SIZE + 5.0, SIZE + 15.0),
                    Align2::LEFT_CENTER,
                    format!(
                        "CPA {:.2} NM in {:.0} min",
                        distance / 1852.0,
                        seconds / 60.0
                    ),
                    FontId::proportional(10.0),
                    colour,
                );
            }
        }
    }
}

/// A unit vector for a compass bearing, in screen coordinates.
fn unit(bearing: f32) -> Vec2 {
    Vec2::new(bearing.sin(), -bearing.cos())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> AisTarget {
        AisTarget {
            id: String::new(),
            screen: [100.0, 100.0],
            heading: Some(0.0),
            cog: Some(0.0),
            sog: Some(5.0),
            label: Some("NORDLYS".into()),
            under_way: true,
            lost: false,
            cpa: None,
        }
    }

    /// The default alarm, a quarter-mile and twelve minutes.
    fn alarm() -> CpaAlarm {
        CpaAlarm { distance_m: 0.25 * 1852.0, seconds: 12.0 * 60.0 }
    }

    fn dangerous(t: &AisTarget) -> bool {
        alarm().triggered_by(t.cpa)
    }

    #[test]
    fn danger_needs_both_near_and_soon() {
        let mut t = target();
        assert!(!dangerous(&t), "no CPA at all is not dangerous");

        t.cpa = Some((100.0, 300.0));
        assert!(dangerous(&t), "close and soon");

        // Close, but an hour away: there is time, and crying wolf on it makes
        // the colour worthless when it matters.
        t.cpa = Some((100.0, 3600.0));
        assert!(!dangerous(&t));

        // Soon, but a mile off: not a threat.
        t.cpa = Some((1852.0, 300.0));
        assert!(!dangerous(&t));

        // Already past — the approach happened and they are opening.
        t.cpa = Some((100.0, 0.0));
        assert!(!dangerous(&t));
    }

    #[test]
    fn bearings_map_to_screen_directions() {
        assert!(unit(0.0).y < 0.0, "north is up");
        assert!(unit(std::f32::consts::FRAC_PI_2).x > 0.0, "east is right");
        for step in 0..12 {
            let b = step as f32 * std::f32::consts::TAU / 12.0;
            assert!((unit(b).length() - 1.0).abs() < 1e-5);
        }
    }

    #[test]
    fn a_tighter_alarm_stops_calling_the_same_target_dangerous() {
        // The point of making these settings: the same traffic must be able to
        // read as safe in a busy strait and as a threat offshore.
        let mut t = target();
        t.cpa = Some((300.0, 300.0));
        assert!(alarm().triggered_by(t.cpa), "default: close and soon");

        let tight = CpaAlarm { distance_m: 100.0, seconds: 12.0 * 60.0 };
        assert!(!tight.triggered_by(t.cpa), "a tighter ring lets it pass");

        let brief = CpaAlarm { distance_m: 0.25 * 1852.0, seconds: 120.0 };
        assert!(!brief.triggered_by(t.cpa), "a shorter horizon lets it pass");
    }
}
