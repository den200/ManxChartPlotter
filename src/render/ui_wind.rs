//! The wind forecast, drawn over the chart as wind barbs.
//!
//! Barbs rather than arrows because that is what a mariner already reads:
//! the shaft points the way the wind comes *from*, and the feathers count
//! it — a pennant for fifty knots, a full barb for ten, a half for five,
//! with a bare circle for calm. Colour carries the same number again, so
//! strength reads at a glance from the helm without counting anything.

use egui::{Color32, Context, RichText};

use super::ui::{UiAction, WindView};

/// One barb, already projected by the renderer.
#[derive(Debug, Clone, PartialEq)]
pub struct WindBarb {
    /// Logical points, the same space as the route overlay.
    pub screen: [f32; 2],
    /// Where the wind blows *from*, degrees true.
    pub from_deg: f32,
    pub kt: f32,
}

/// Below this the barb is a circle: there is no honest feather for a
/// breath of air, and the standard says so too.
const CALM_KT: f32 = 2.5;
const SHAFT_PX: f32 = 26.0;
const FEATHER_PX: f32 = 9.0;
const FEATHER_GAP: f32 = 4.5;

/// Speed to colour: the Beaufort story in five steps, light to storm.
/// Deliberately not a continuous rainbow — a helmsman wants "is that a reef
/// or not", and a band answers faster than a hue.
fn colour_for(kt: f32) -> Color32 {
    // Chosen against the Day palette's pale blue sea, where a light tint
    // simply disappears — every band here is darker than the water.
    match kt {
        k if k < 6.0 => Color32::from_rgb(90, 120, 160),
        k if k < 12.0 => Color32::from_rgb(30, 120, 60),
        k if k < 20.0 => Color32::from_rgb(170, 130, 10),
        k if k < 30.0 => Color32::from_rgb(215, 100, 20),
        _ => Color32::from_rgb(200, 30, 30),
    }
}

/// Draw the field. Under the vessels and the routes: weather is background,
/// the boat and her plan are not.
pub fn draw(ctx: &Context, barbs: &[WindBarb]) {
    if barbs.is_empty() {
        return;
    }
    let painter = ctx.layer_painter(egui::LayerId::background());
    for barb in barbs {
        draw_one(&painter, barb);
    }
}

fn draw_one(painter: &egui::Painter, barb: &WindBarb) {
    let at = egui::pos2(barb.screen[0], barb.screen[1]);
    let colour = colour_for(barb.kt);
    let stroke = egui::Stroke::new(1.9, colour);

    if barb.kt < CALM_KT {
        painter.circle(at, 3.0, Color32::TRANSPARENT, stroke);
        return;
    }

    // The shaft runs from the position towards where the wind comes from,
    // which is the convention every weather chart uses.
    let rad = (barb.from_deg as f64).to_radians();
    let dir = egui::vec2(rad.sin() as f32, -(rad.cos() as f32));
    let tip = at + dir * SHAFT_PX;
    painter.line_segment([at, tip], stroke);

    // Feathers hang off the outer end, working inwards. They sit on the side
    // that makes the barb read the same in both hemispheres here — northern
    // convention, which is where this boat sails.
    let side = egui::vec2(-dir.y, dir.x);
    let mut remaining = (barb.kt / 5.0).round() * 5.0; // barbs quantise to 5 kt
    let mut along = 0.0f32;

    while remaining >= 50.0 {
        let base = tip - dir * along;
        let inner = base - dir * FEATHER_GAP * 1.6;
        painter.add(egui::Shape::convex_polygon(
            vec![base, inner, base + side * FEATHER_PX],
            colour,
            egui::Stroke::NONE,
        ));
        along += FEATHER_GAP * 2.0;
        remaining -= 50.0;
    }
    while remaining >= 10.0 {
        let base = tip - dir * along;
        painter.line_segment([base, base + side * FEATHER_PX], stroke);
        along += FEATHER_GAP;
        remaining -= 10.0;
    }
    if remaining >= 5.0 {
        // A half barb never sits at the very tip — that reads as a full one.
        if along == 0.0 {
            along = FEATHER_GAP;
        }
        let base = tip - dir * along;
        painter.line_segment([base, base + side * (FEATHER_PX * 0.5)], stroke);
    }
}

/// The wind window: which forecast, which hour, and how to refresh it.
pub fn window(ctx: &Context, view: &mut WindView, actions: &mut Vec<UiAction>) {
    let mut open = view.show;
    egui::Window::new("Wind")
        .open(&mut open)
        .default_size([420.0, 120.0])
        .default_pos([80.0, 70.0])
        .constrain(true)
        .collapsible(true)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Forecast:").weak());
                ui.label(&view.source);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(!view.busy, egui::Button::new("Refresh here"))
                        .on_hover_text("Fetch the wind for the area now on screen")
                        .clicked()
                    {
                        actions.push(UiAction::WindRefresh);
                    }
                });
            });

            if view.steps.is_empty() {
                if view.busy {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new(&view.status).small());
                    });
                } else {
                    ui.label(
                        RichText::new(if view.status.is_empty() {
                            "No forecast loaded yet."
                        } else {
                            &view.status
                        })
                        .small(),
                    );
                }
                return;
            }

            let last = view.steps.len() - 1;
            view.step = view.step.min(last);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(view.step > 0, egui::Button::new("−").small())
                    .clicked()
                {
                    view.step -= 1;
                }
                let mut step = view.step;
                ui.add(
                    egui::Slider::new(&mut step, 0..=last)
                        .show_value(false)
                        .trailing_fill(true),
                );
                if step != view.step {
                    view.step = step;
                }
                if ui
                    .add_enabled(view.step < last, egui::Button::new("+").small())
                    .clicked()
                {
                    view.step += 1;
                }
            });
            ui.label(RichText::new(&view.valid_label).strong());
            if !view.status.is_empty() {
                ui.label(RichText::new(&view.status).small().weak());
            }

            // The key, so the colours mean something without a manual.
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                for (label, kt) in [
                    ("<6", 3.0f32),
                    ("6–12", 9.0),
                    ("12–20", 16.0),
                    ("20–30", 25.0),
                    ("30+", 35.0),
                ] {
                    ui.label(RichText::new(label).small().color(colour_for(kt)));
                }
                ui.label(RichText::new("kt").small().weak());
            });
        });
    view.show = open;
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

    #[test]
    fn the_colour_bands_climb_with_the_wind() {
        let ordered = [3.0, 9.0, 16.0, 25.0, 40.0].map(colour_for);
        for w in ordered.windows(2) {
            assert_ne!(w[0], w[1], "each band must be distinguishable");
        }
        assert_eq!(colour_for(5.9), colour_for(3.0));
        assert_eq!(colour_for(6.0), colour_for(11.9));
    }
}
