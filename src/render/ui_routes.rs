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

/// One waypoint of a route, as the editor lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteWaypointRow {
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    /// Distance and bearing from the previous waypoint; `None` for the first.
    pub leg: Option<(f64, f64)>,
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
    /// The route's waypoints in order — listed while it is being edited.
    pub waypoints: Vec<RouteWaypointRow>,
}

/// The waypoint list of the route being edited: rename, reorder, remove,
/// and the standing invitation to tap the chart for one more.
fn waypoint_editor(ui: &mut egui::Ui, row: &RouteRow, actions: &mut Vec<UiAction>) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("tap the chart to add a waypoint")
                .small()
                .color(APLRT),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button("Delete route")
                .on_hover_text("Remove this route. Its waypoints stay in the store.")
                .clicked()
            {
                actions.push(UiAction::RouteDelete { route_id: row.id });
            }
            if ui
                .add_enabled(
                    row.waypoints.len() > 1,
                    egui::Button::new("Reverse").small(),
                )
                .on_hover_text("Sail it the other way round")
                .clicked()
            {
                actions.push(UiAction::RouteReverse { route_id: row.id });
            }
        });
    });
    if row.waypoints.is_empty() {
        ui.label(
            RichText::new("No waypoints yet — the first tap places the start.")
                .small()
                .weak(),
        );
        return;
    }
    let last = row.waypoints.len() - 1;
    for (i, wp) in row.waypoints.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{:>2}.", i + 1)).small().weak());
            let mut name = wp.name.clone();
            if ui
                .add(egui::TextEdit::singleline(&mut name).desired_width(110.0))
                .changed()
            {
                actions.push(UiAction::RouteWaypointRename {
                    route_id: row.id,
                    index: i,
                    name,
                });
            }
            ui.label(
                RichText::new(format!("{:.4}, {:.4}", wp.lat, wp.lon))
                    .small()
                    .weak(),
            );
            if let Some((nm, brg)) = wp.leg {
                ui.label(
                    RichText::new(format!("{nm:.2} nm {brg:03.0}°"))
                        .small()
                        .weak(),
                );
            }
            // Words, not arrows: egui's default font has no ↑ ↓ ✕ and draws
            // them as empty boxes. The same trap as "→" in a route name.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("Del")
                    .on_hover_text("Remove this waypoint from the route")
                    .clicked()
                {
                    actions.push(UiAction::RouteWaypointDelete {
                        route_id: row.id,
                        index: i,
                    });
                }
                if ui
                    .add_enabled(i < last, egui::Button::new("Dn").small())
                    .on_hover_text("Later in the route")
                    .clicked()
                {
                    actions.push(UiAction::RouteWaypointMove {
                        route_id: row.id,
                        index: i,
                        delta: 1,
                    });
                }
                if ui
                    .add_enabled(i > 0, egui::Button::new("Up").small())
                    .on_hover_text("Earlier in the route")
                    .clicked()
                {
                    actions.push(UiAction::RouteWaypointMove {
                        route_id: row.id,
                        index: i,
                        delta: -1,
                    });
                }
            });
        });
    }
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
        // Wide enough that a row of buttons and the editor's waypoint lines
        // fit without the labels being squeezed into wraps.
        .default_size([620.0, 380.0])
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
                    if ui
                        .button("New route")
                        .on_hover_text("Start an empty route and place its waypoints by tapping the chart")
                        .clicked()
                    {
                        actions.push(UiAction::RouteCreate);
                    }
                });
            });
            ui.separator();

            if view.rows.is_empty() {
                ui.label(
                    "No routes yet. Press New route and tap the chart, import a GPX into the \
                     routes folder, or fetch from Signal K.",
                );
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
                        let editing = view.editing == Some(row.id);
                        if editing {
                            let mut name = row.name.clone();
                            let edit = ui.add(
                                egui::TextEdit::singleline(&mut name).desired_width(150.0),
                            );
                            if edit.changed() {
                                actions.push(UiAction::RouteRename {
                                    route_id: row.id,
                                    name,
                                });
                            }
                        } else {
                            let name = if row.active {
                                RichText::new(&row.name).strong()
                            } else {
                                RichText::new(&row.name)
                            };
                            ui.label(name);
                        }
                        // Extend, not wrap: a wrapped summary breaks into
                        // fragments that interleave with the buttons of the
                        // right-to-left group beside it.
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!(
                                    "{} legs · {:.1} nm",
                                    row.legs, row.distance_nm
                                ))
                                .small()
                                .weak(),
                            )
                            .wrap_mode(egui::TextWrapMode::Extend),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if editing {
                                    if ui.button("Done").clicked() {
                                        actions.push(UiAction::RouteEdit { route_id: None });
                                    }
                                } else if ui
                                    .button("Edit")
                                    .on_hover_text("Show the waypoints and add more by tapping the chart")
                                    .clicked()
                                {
                                    actions.push(UiAction::RouteEdit {
                                        route_id: Some(row.id),
                                    });
                                }
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
                    if view.editing == Some(row.id) {
                        waypoint_editor(ui, &row, actions);
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
            ui.checkbox(&mut weather.use_waves, "Waves (GFS Hs: slows the boat, blocks over the limit)")
                .on_hover_text(
                    "Fetches significant wave height alongside the wind. Speed is \
                     reduced by 1/(1 + 0.03·Hs²) and seas over the configured \
                     maximum are treated as impassable. If the wave file is not \
                     yet published the route falls back to wind alone.",
                );
            ui.checkbox(
                &mut weather.use_currents,
                format!("Currents ({})", crate::nav::currents::CURRENT_SOURCE_LABEL),
            )
            .on_hover_text(
                "Fetches surface currents over the passage box and lets the \
                 water carry the boat — in Danish waters the tidal streams \
                 this includes are often worth more than the wind detail. \
                 If the fetch fails the route plans in still water.",
            );
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
