//! AIS targets on the chart.
//!
//! Drawn to the shape the standards settle on, and for the reasons behind it
//! rather than the letter of it:
//!
//! - **A triangle**, pointing along heading. Distinct from own ship at a
//!   glance, which matters more than either shape being pretty.
//! - **Hollow while sleeping, filled once it matters.** A screen with forty
//!   targets on it is unreadable if all forty shout.
//! - **A course vector** only for a vessel actually under way. A moored ship
//!   with a stale course would otherwise sprout a line across the harbour.
//! - **Crossed through when lost.** A target that simply vanishes is
//!   indistinguishable from one that was never there; a target that quietly
//!   stays is worse, because it is a ship that is no longer where you think.
//! - **Red when the closest approach is close and soon.** This is the only
//!   colour on the layer, so it means one thing.
//!
//! Like own ship, this uses egui's painter over the resolved chart. The same
//! interim reasoning applies: S-52 has proper symbology for these (VESSEL01)
//! and when navcore grows it, it belongs in the chart pipeline.

use egui::{Align2, Color32, Context, FontId, Pos2, Stroke, Vec2};

/// One target, resolved by the renderer which owns the camera.
#[derive(Debug, Clone)]
pub struct AisTarget {
    pub screen: [f32; 2],
    /// Hull orientation, radians clockwise from north.
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

/// How near, and how soon, before a target is called dangerous.
///
/// Two cables and twelve minutes: tight enough not to cry wolf in a busy
/// strait, loose enough to give time to act. Real ECDIS makes these settings;
/// so should navcore, once there is somewhere to put them.
const CPA_ALARM_M: f32 = 370.0;
const TCPA_ALARM_S: f32 = 12.0 * 60.0;

/// Half-height of the triangle, in points.
const SIZE: f32 = 9.0;
/// How far ahead the course vector reaches.
const VECTOR_MINUTES: f32 = 6.0;

pub fn draw(ctx: &Context, targets: &[AisTarget], mpp: f32) {
    if targets.is_empty() {
        return;
    }
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Middle,
        egui::Id::new("ais"),
    ));
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

        let dangerous = target
            .cpa
            .is_some_and(|(d, t)| d < CPA_ALARM_M && t < TCPA_ALARM_S && t > 0.0);
        let colour = if target.lost {
            Color32::from_gray(140)
        } else if dangerous {
            Color32::from_rgb(200, 40, 40)
        } else {
            ink
        };
        // Filled means "this one is worth your attention": moving, or close.
        let fill = if target.lost {
            Color32::TRANSPARENT
        } else if dangerous {
            Color32::from_rgb(200, 40, 40)
        } else if target.under_way {
            colour.gamma_multiply(0.35)
        } else {
            Color32::TRANSPARENT
        };

        // Course vector, under the hull, and only when actually moving.
        if target.under_way && !target.lost {
            if let (Some(cog), Some(sog)) = (target.cog, target.sog) {
                if sog > 0.2 && mpp > 0.0 {
                    let length = sog * VECTOR_MINUTES * 60.0 / mpp;
                    if length > SIZE {
                        painter.line_segment(
                            [pos, pos + unit(cog) * length],
                            Stroke::new(1.4, colour),
                        );
                    }
                }
            }
        }

        let bearing = target.heading.or(target.cog).unwrap_or(0.0);
        let points = vec![
            pos + unit(bearing) * SIZE,
            pos + unit(bearing + 2.4) * SIZE * 0.8,
            pos + unit(bearing - 2.4) * SIZE * 0.8,
        ];
        painter.add(egui::Shape::convex_polygon(
            points,
            fill,
            Stroke::new(1.5, colour),
        ));

        // A lost target is struck through rather than removed, so it reads as
        // "was here, no longer reporting" instead of silently disappearing.
        if target.lost {
            let d = SIZE * 1.1;
            painter.line_segment(
                [pos + Vec2::new(-d, -d), pos + Vec2::new(d, d)],
                Stroke::new(1.4, colour),
            );
            painter.line_segment(
                [pos + Vec2::new(-d, d), pos + Vec2::new(d, -d)],
                Stroke::new(1.4, colour),
            );
        }

        if let Some(label) = &target.label {
            painter.text(
                pos + Vec2::new(SIZE + 4.0, -SIZE),
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
                    pos + Vec2::new(SIZE + 4.0, SIZE * 0.6),
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

    /// The rule that decides the only colour on the layer.
    fn dangerous(t: &AisTarget) -> bool {
        t.cpa
            .is_some_and(|(d, s)| d < CPA_ALARM_M && s < TCPA_ALARM_S && s > 0.0)
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
    fn the_alarm_thresholds_are_the_ones_documented() {
        // A quarter-mile and twelve minutes. Pinned because changing either
        // silently changes what the chart calls dangerous.
        assert!((CPA_ALARM_M - 370.0).abs() < 0.1);
        assert!((TCPA_ALARM_S - 720.0).abs() < 0.1);
        assert!(CPA_ALARM_M / 1852.0 < 0.25, "under a quarter of a mile");
    }
}
