//! The routes window, and the guidance strip that appears while following.
//!
//! No chart drawing here — route rendering is a separate task by the spec's
//! own scoping. This window is management: what routes exist, activate one,
//! move them to and from Signal K. The strip is the M2 payoff: XTE with a
//! steer arrow, distance, bearing and closing speed for the active waypoint,
//! sized to be read from the helm like the instrument bar below it.

use egui::{Color32, Context, RichText};
use uuid::Uuid;

use super::ui::{RoutesView, UiAction};
use crate::nav::Guidance;

/// One row of the routes table, prepared by the renderer.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteRow {
    pub id: Uuid,
    pub name: String,
    pub legs: usize,
    pub distance_nm: f64,
    pub active: bool,
    /// "GFS run 2026-08-01 06Z · built-in cruiser" for generated routes.
    pub provenance: Option<String>,
}

pub fn window(
    ctx: &Context,
    view: &mut RoutesView,
    weather: &mut crate::render::ui::WeatherView,
    actions: &mut Vec<UiAction>,
) {
    let mut open = view.open;
    egui::Window::new("Routes")
        .open(&mut open)
        .default_size([460.0, 320.0])
        .constrain(true)
        .collapsible(false)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("{} route(s)", view.rows.len()))
                        .small()
                        .weak(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button("Fetch from Signal K")
                        .on_hover_text(
                            "Read the routes the Signal K server holds and add them here",
                        )
                        .clicked()
                    {
                        actions.push(UiAction::RoutesFetchSignalK);
                    }
                });
            });
            ui.separator();

            if view.rows.is_empty() {
                ui.label("No routes yet. Import a GPX into the routes folder, or fetch from Signal K.");
            }

            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                for row in view.rows.clone() {
                    ui.horizontal(|ui| {
                        let name = if row.active {
                            RichText::new(&row.name).strong()
                        } else {
                            RichText::new(&row.name)
                        };
                        ui.label(name);
                        ui.label(
                            RichText::new(format!(
                                "{} legs · {:.1} nm",
                                row.legs, row.distance_nm
                            ))
                            .small()
                            .weak(),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if row.active {
                                    if ui.button("Deactivate").clicked() {
                                        actions.push(UiAction::RouteDeactivate);
                                    }
                                } else if ui.button("Activate").clicked() {
                                    actions.push(UiAction::RouteActivate { route_id: row.id });
                                }
                                if ui
                                    .small_button("Publish")
                                    .on_hover_text("Publish this route to the Signal K server")
                                    .clicked()
                                {
                                    actions.push(UiAction::RoutePublish { route_id: row.id });
                                }
                                if ui
                                    .add_enabled(!weather.busy, egui::Button::new("Wx").small())
                                    .on_hover_text(
                                        "Weather-route between this route's endpoints, on the \
                                         latest forecast and the polar below",
                                    )
                                    .clicked()
                                {
                                    actions.push(UiAction::WeatherRoute { route_id: row.id });
                                }
                            },
                        );
                    });
                    if let Some(p) = &row.provenance {
                        ui.label(RichText::new(p).small().weak());
                    }
                    ui.separator();
                }
            });

            // The weather engine's own corner: which forecast feeds it, how
            // far out, and whose boat it thinks it is planning for. Shown,
            // not buried — a route is only as good as the forecast and the
            // polar behind it.
            ui.add_space(6.0);
            ui.label(RichText::new("Weather routing").strong());
            ui.horizontal(|ui| {
                ui.label(RichText::new("Forecast:").weak());
                ui.label("NOAA GFS 0.25° (NOMADS)");
                ui.add_space(10.0);
                ui.label(RichText::new("hours:").weak());
                ui.add(
                    egui::DragValue::new(&mut weather.hours)
                        .speed(3)
                        .range(12..=120),
                );
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("Polar:").weak());
                ui.add(
                    egui::TextEdit::singleline(&mut weather.polar_path)
                        .hint_text("built-in ~10 m cruiser — set a .pol path for your boat")
                        .desired_width(280.0),
                );
            });
            if weather.busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new(&weather.status).small());
                });
            } else if !weather.status.is_empty() {
                ui.label(RichText::new(&weather.status).small());
            }

            if !view.status.is_empty() {
                ui.separator();
                ui.label(RichText::new(&view.status).small());
            }
        });
    view.open = open;
}

/// The strip above the instrument bar while a route is active.
pub fn guidance_strip(ctx: &Context, g: &Guidance) {
    egui::TopBottomPanel::bottom("guidance-strip")
        .exact_height(34.0)
        .show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                ui.add_space(8.0);
                if g.finished {
                    ui.label(RichText::new("Route complete").strong());
                    return;
                }
                // "→" is not in egui's default font and draws as a box;
                // the filled triangle is.
                ui.label(RichText::new("▶").weak());
                ui.label(RichText::new(&g.to_name).strong());
                if g.arrived {
                    ui.label(RichText::new("arrived").color(Color32::from_rgb(80, 190, 120)));
                }
                ui.separator();

                // The steer cue reads as an arrow toward the track: XTE
                // positive = starboard of track = the track is to port.
                let (arrow, side) = if g.xte_nm >= 0.0 {
                    ("◀", "steer port")
                } else {
                    ("▶", "steer stbd")
                };
                let xte_text = format!("XTE {:.2} nm {arrow}", g.xte_nm.abs());
                let xte = if g.xte_nm.abs() > 0.1 {
                    RichText::new(xte_text).strong()
                } else {
                    RichText::new(xte_text)
                };
                ui.label(xte).on_hover_text(side);
                ui.separator();
                ui.label(format!("DTW {:.2} nm", g.dtw_nm));
                ui.separator();
                ui.label(format!("BRG {:03.0}°", g.btw_deg));
                if let Some(vmg) = g.vmg_kt {
                    ui.separator();
                    ui.label(format!("VMG {vmg:.1} kn"));
                    // Time to go, from closing speed — only when actually
                    // closing; an ETA from a negative VMG is an insult.
                    if vmg > 0.2 {
                        let hours = g.dtw_nm / vmg;
                        ui.separator();
                        ui.label(format!("TTG {}:{:02}", hours as u32, (hours.fract() * 60.0) as u32));
                    }
                }
            });
        });
}
