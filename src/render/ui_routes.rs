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

/// A route projected for the chart overlay: screen points, the active leg,
/// and whatever the plan knows.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteDisplay {
    pub name: String,
    pub is_active: bool,
    /// Index of the leg currently being sailed, when this route is active.
    pub active_leg: Option<usize>,
    pub points: Vec<RoutePointDisplay>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoutePointDisplay {
    /// Logical points.
    pub screen: [f32; 2],
    pub name: String,
    /// "14:32" from the weather plan, when there is one.
    pub eta: Option<String>,
    /// The arrival circle in points, drawn on the active waypoint.
    pub arrival_radius_px: Option<f32>,
}

/// S-52's own mariner colours, read from the DAY_BRIGHT palette:
/// planned route PLRTE, active planned route APLRT.
const PLRTE: Color32 = Color32::from_rgb(220, 64, 37);
const APLRT: Color32 = Color32::from_rgb(235, 125, 54);

/// Routes on the chart. Dashed in PLRTE as S-52 draws a planned route; the
/// active route in APLRT with its current leg solid and heavier; legs already
/// sailed fall back to quiet.
pub fn draw_overlay(ctx: &Context, routes: &[RouteDisplay]) {
    if routes.is_empty() {
        return;
    }
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Middle,
        egui::Id::new("routes-overlay"),
    ));
    let screen = ctx.screen_rect().expand(2_000.0);

    for route in routes {
        let colour = if route.is_active { APLRT } else { PLRTE };
        for (i, w) in route.points.windows(2).enumerate() {
            let a = egui::pos2(w[0].screen[0], w[0].screen[1]);
            let b = egui::pos2(w[1].screen[0], w[1].screen[1]);
            if !screen.contains(a) && !screen.contains(b) {
                continue;
            }
            let sailed = route.active_leg.map(|l| i < l).unwrap_or(false);
            let current = route.active_leg == Some(i);
            if current {
                painter.line_segment([a, b], egui::Stroke::new(3.0, colour));
            } else {
                let stroke = egui::Stroke::new(
                    2.0,
                    if sailed { colour.gamma_multiply(0.4) } else { colour },
                );
                painter.add(egui::Shape::dashed_line(&[a, b], stroke, 8.0, 6.0));
            }
        }
        for (i, p) in route.points.iter().enumerate() {
            let pos = egui::pos2(p.screen[0], p.screen[1]);
            if !screen.contains(pos) {
                continue;
            }
            painter.circle(
                pos,
                4.0,
                Color32::TRANSPARENT,
                egui::Stroke::new(1.8, colour),
            );
            if let Some(r) = p.arrival_radius_px {
                painter.add(egui::Shape::dashed_line(
                    &circle_points(pos, r),
                    egui::Stroke::new(1.0, colour),
                    4.0,
                    4.0,
                ));
            }
            // Names right of the mark; ETAs below the name. First and last
            // points always speak; intermediates only when the route is
            // active or planned, to keep a dense wx route readable.
            let ends = i == 0 || i + 1 == route.points.len();
            if ends || route.is_active || p.eta.is_some() {
                painter.text(
                    pos + egui::vec2(7.0, -6.0),
                    egui::Align2::LEFT_CENTER,
                    &p.name,
                    egui::FontId::proportional(11.0),
                    colour,
                );
                if let Some(eta) = &p.eta {
                    painter.text(
                        pos + egui::vec2(7.0, 7.0),
                        egui::Align2::LEFT_CENTER,
                        eta,
                        egui::FontId::proportional(10.0),
                        colour.gamma_multiply(0.85),
                    );
                }
            }
        }
    }
}

/// A polygonal circle for dashed rendering — epaint dashes lines, not arcs.
fn circle_points(centre: egui::Pos2, r: f32) -> Vec<egui::Pos2> {
    (0..=32)
        .map(|i| {
            let a = i as f32 / 32.0 * std::f32::consts::TAU;
            egui::pos2(centre.x + r * a.cos(), centre.y + r * a.sin())
        })
        .collect()
}

/// One row of the routes table, prepared by the renderer.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteRow {
    pub id: Uuid,
    pub name: String,
    pub legs: usize,
    pub distance_nm: f64,
    pub active: bool,
    pub visible: bool,
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
                        let mut shown = row.visible;
                        if ui
                            .checkbox(&mut shown, "")
                            .on_hover_text("Show this route on the chart")
                            .changed()
                        {
                            if shown {
                                view.visible.insert(row.id);
                            } else {
                                view.visible.remove(&row.id);
                            }
                        }
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
