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
/// Might this leg put ink on the screen?
///
/// Asked of the leg, not of its ends. Testing whether either end lands on the
/// screen throws away exactly the legs that matter most: a long one — the
/// weather router happily produces a fifteen-mile leg — has both ends far
/// outside the view while crossing the middle of it, and the route then
/// appeared to stop dead at the edge of the screen. Comparing the leg's own
/// bounding box against the view keeps it. The test is generous, so a
/// diagonal that misses the corner is still drawn; that costs one line and is
/// cheaper than being exact.
fn leg_may_show(screen: egui::Rect, a: egui::Pos2, b: egui::Pos2) -> bool {
    if !(a.x.is_finite() && a.y.is_finite() && b.x.is_finite() && b.y.is_finite()) {
        return false;
    }
    let lo_x = a.x.min(b.x);
    let hi_x = a.x.max(b.x);
    let lo_y = a.y.min(b.y);
    let hi_y = a.y.max(b.y);
    lo_x <= screen.right()
        && hi_x >= screen.left()
        && lo_y <= screen.bottom()
        && hi_y >= screen.top()
}

pub fn draw_overlay(ctx: &Context, routes: &[RouteDisplay]) {
    if routes.is_empty() {
        return;
    }
    let painter = ctx.layer_painter(egui::LayerId::background());
    let screen = ctx.screen_rect().expand(2_000.0);

    for route in routes {
        let colour = if route.is_active { APLRT } else { PLRTE };
        for (i, w) in route.points.windows(2).enumerate() {
            let a = egui::pos2(w[0].screen[0], w[0].screen[1]);
            let b = egui::pos2(w[1].screen[0], w[1].screen[1]);
            if !leg_may_show(screen, a, b) {
                continue;
            }
            let sailed = route.active_leg.map(|l| i < l).unwrap_or(false);
            let current = route.active_leg == Some(i);
            if current {
                painter.line_segment([a, b], egui::Stroke::new(3.0_f32, colour));
            } else {
                let stroke = egui::Stroke::new(
                    2.0_f32,
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
                egui::Stroke::new(1.8_f32, colour),
            );
            if let Some(r) = p.arrival_radius_px {
                painter.add(egui::Shape::dashed_line(
                    &circle_points(pos, r),
                    egui::Stroke::new(1.0_f32, colour),
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
    pub id: Uuid,
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

/// A name being edited. The text lives in egui's memory while the field has
/// focus and is handed back once, when the user leaves it or presses Enter —
/// not per keystroke, which rewrote the route file for every letter and let
/// a half-typed or empty name reach the disk. An empty name is refused and
/// the old one comes back.
fn name_field(
    ui: &mut egui::Ui,
    key: impl std::hash::Hash,
    current: &str,
    width: f32,
) -> Option<String> {
    let id = egui::Id::new(key);
    let mut text = ui
        .data(|d| d.get_temp::<String>(id))
        .unwrap_or_else(|| current.to_string());
    let edit = ui.add(egui::TextEdit::singleline(&mut text).desired_width(width));
    if edit.changed() {
        ui.data_mut(|d| d.insert_temp(id, text.clone()));
    }
    if edit.lost_focus() {
        ui.data_mut(|d| d.remove::<String>(id));
        let name = text.trim();
        if !name.is_empty() && name != current {
            return Some(name.to_string());
        }
    }
    None
}

/// The waypoint list of the route being edited: rename, reorder, remove,
/// and the standing invitation to tap the chart for one more.
fn waypoint_editor(
    ui: &mut egui::Ui,
    row: &RouteRow,
    undo: Option<&(Uuid, usize, Uuid, String)>,
    actions: &mut Vec<UiAction>,
) {
    // Del is one tap on a small button among others; the way back is one tap
    // too, rather than a question before every removal.
    if let Some((route_id, _, _, name)) = undo {
        if *route_id == row.id {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Removed {name}")).small());
                if ui.small_button("Undo").clicked() {
                    actions.push(UiAction::RouteWaypointUndo);
                }
            });
        }
    }
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("tap the chart to add a waypoint")
                .small()
                .color(APLRT),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Two taps, the second on a different word: a planned passage is
            // an evening's work, and "Delete route" sits right beside
            // "Reverse". The pending question lives in egui's memory, so it
            // goes away with the editor.
            let confirm_id = egui::Id::new(("confirm-delete-route", row.id));
            let asking = ui.data(|d| d.get_temp::<bool>(confirm_id)).unwrap_or(false);
            if asking {
                if ui.small_button("Keep").clicked() {
                    ui.data_mut(|d| d.remove::<bool>(confirm_id));
                }
                if ui
                    .small_button(RichText::new("Delete it").color(egui::Color32::from_rgb(200, 60, 60)))
                    .clicked()
                {
                    ui.data_mut(|d| d.remove::<bool>(confirm_id));
                    actions.push(UiAction::RouteDelete { route_id: row.id });
                }
                ui.label(RichText::new("Delete this route?").small());
            } else if ui
                .small_button("Delete route")
                .on_hover_text("Remove this route. Its waypoints stay in the store.")
                .clicked()
            {
                ui.data_mut(|d| d.insert_temp(confirm_id, true));
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
            if let Some(name) = name_field(ui, ("wp-name", wp.id), &wp.name, 110.0) {
                actions.push(UiAction::RouteWaypointRename {
                    route_id: row.id,
                    index: i,
                    waypoint: wp.id,
                    name,
                });
            }
            ui.label(
                RichText::new(crate::geo::format_latlon(wp.lat, wp.lon))
                    .small()
                    .weak(),
            );
            if let Some((nm, brg)) = wp.leg {
                ui.label(
                    RichText::new(format!("{nm:.2} NM {}", crate::geo::format_bearing(brg)))
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
                        waypoint: wp.id,
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
                        waypoint: wp.id,
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
                        waypoint: wp.id,
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
        // Inside the space the menu bar and instrument strip leave: a window
        // over the menu bar hides the very button that closes it.
        .constrain_to(ctx.available_rect())
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
                        .add_enabled(!view.net_busy, egui::Button::new("Fetch from Signal K"))
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
            // Right under the buttons that cause it, not below the weather
            // settings where a publish result used to land out of sight.
            if view.net_busy || !view.status.is_empty() {
                ui.horizontal(|ui| {
                    if view.net_busy {
                        ui.spinner();
                    }
                    ui.label(RichText::new(&view.status).small());
                });
            }
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
                        // The route being followed is always drawn; a box
                        // that could be clicked but never cleared read as
                        // broken, so it is shown ticked and greyed instead.
                        if ui
                            .add_enabled(!row.active, egui::Checkbox::new(&mut shown, ""))
                            .on_hover_text("Show this route on the chart")
                            .on_disabled_hover_text("The route being followed is always shown")
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
                            if let Some(name) =
                                name_field(ui, ("route-name", row.id), &row.name, 150.0)
                            {
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
                                    "{} legs · {:.1} NM",
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
                                    .add_enabled(!view.net_busy, egui::Button::new("Publish"))
                                    .on_hover_text("Publish this route to the Signal K server")
                                    .clicked()
                                {
                                    actions.push(UiAction::RoutePublish { route_id: row.id });
                                }
                                if ui
                                    .add_enabled(!weather.busy, egui::Button::new("Sail"))
                                    .on_hover_text(
                                        "Plan a sailing passage between this route's ends, on \
                                         the latest forecast and the polar below — the same \
                                         as Sail in the menu bar",
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
                        waypoint_editor(ui, &row, view.undo.as_ref(), actions);
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
                let r = ui.add(
                    egui::DragValue::new(&mut weather.hours)
                        .speed(3)
                        .range(12..=120),
                );
                if r.drag_stopped() || r.lost_focus() {
                    actions.push(UiAction::SettingsChanged);
                }
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("Polar:").weak());
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut weather.polar_path)
                            .hint_text("built-in ~10 m cruiser — set a .pol path for your boat")
                            .desired_width(280.0),
                    )
                    .lost_focus()
                {
                    actions.push(UiAction::SettingsChanged);
                }
            });
            let waves = ui
                .checkbox(&mut weather.use_waves, "Waves (GFS Hs: slows the boat, blocks over the limit)")
                .on_hover_text(
                    "Fetches significant wave height alongside the wind. Speed is \
                     reduced by 1/(1 + 0.03·Hs²) and seas over the configured \
                     maximum are treated as impassable. If the wave file is not \
                     yet published the route falls back to wind alone.",
                );
            let currents = ui.checkbox(
                &mut weather.use_currents,
                format!("Currents ({})", crate::nav::currents::CURRENT_SOURCE_LABEL),
            )
            .on_hover_text(
                "Fetches surface currents over the passage box and lets the \
                 water carry the boat — in Danish waters the tidal streams \
                 this includes are often worth more than the wind detail. \
                 If the fetch fails the route plans in still water.",
            );
            if waves.changed() || currents.changed() {
                actions.push(UiAction::SettingsChanged);
            }
            if weather.busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new(&weather.status).small());
                });
            } else if !weather.status.is_empty() {
                ui.label(RichText::new(&weather.status).small());
            }

        });
    // Closing the window ends editing too. Left on, every later chart tap
    // would keep adding waypoints to a route the user can no longer see.
    if view.open && !open && view.editing.is_some() {
        actions.push(UiAction::RouteEdit { route_id: None });
    }
    view.open = open;
}

/// The strip above the instrument bar while a route is active.
/// The strip, while a route is followed but cannot be steered yet.
pub fn waiting_strip(ctx: &Context, why: &str) {
    egui::TopBottomPanel::bottom("guidance-strip")
        .exact_height(34.0)
        .show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                ui.add_space(8.0);
                ui.label(RichText::new(why).weak());
            });
        });
}

pub fn guidance_strip(ctx: &Context, g: &Guidance, units: &crate::signalk::UnitPrefs) {
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
                let xte_text = format!("XTE {:.2} NM {arrow}", g.xte_nm.abs());
                let xte = if g.xte_nm.abs() > 0.1 {
                    RichText::new(xte_text).strong()
                } else {
                    RichText::new(xte_text)
                };
                ui.label(xte).on_hover_text(side);
                ui.separator();
                ui.label(format!("DTW {:.2} NM", g.dtw_nm));
                ui.separator();
                ui.label(format!("BRG {}", crate::geo::format_bearing(g.btw_deg)));
                if let Some(vmg) = g.vmg_kt {
                    ui.separator();
                    // In the speed unit the instruments use.
                    let r = crate::signalk::Quantity::Speed
                        .format(vmg * crate::geo::METRES_PER_NM / 3600.0, units);
                    ui.label(format!("VMG {} {}", r.value, r.unit));
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

#[cfg(test)]
mod overlay_tests {
    use super::*;
    use egui::{pos2, Rect};

    fn view() -> Rect {
        Rect::from_min_max(pos2(0.0, 0.0), pos2(1000.0, 700.0))
    }

    /// The bug: a leg long enough that both its ends lie outside the view was
    /// dropped, and the route stopped at the edge of the screen. A fifteen
    /// mile leg zoomed in on is exactly that leg.
    #[test]
    fn a_leg_crossing_the_view_is_drawn_even_though_neither_end_is_in_it() {
        // Straight through, left to right.
        assert!(leg_may_show(view(), pos2(-5000.0, 350.0), pos2(6000.0, 350.0)));
        // Straight through, top to bottom.
        assert!(leg_may_show(view(), pos2(500.0, -4000.0), pos2(500.0, 4000.0)));
        // Corner to corner.
        assert!(leg_may_show(
            view(),
            pos2(-3000.0, -3000.0),
            pos2(4000.0, 4000.0)
        ));
    }

    /// Legs that cannot touch the view are still skipped — the point of the
    /// test is to save work, and a route may run off the far side of the world.
    #[test]
    fn a_leg_that_cannot_touch_the_view_is_skipped() {
        assert!(!leg_may_show(view(), pos2(-900.0, 350.0), pos2(-100.0, 350.0)));
        assert!(!leg_may_show(view(), pos2(1100.0, 0.0), pos2(4000.0, 700.0)));
        assert!(!leg_may_show(view(), pos2(0.0, -900.0), pos2(1000.0, -50.0)));
        assert!(!leg_may_show(view(), pos2(0.0, 800.0), pos2(1000.0, 5000.0)));
    }

    /// A leg with an end inside is always drawn, which is what the old test
    /// got right and must not be lost.
    #[test]
    fn a_leg_with_an_end_in_the_view_is_drawn() {
        assert!(leg_may_show(view(), pos2(500.0, 350.0), pos2(9000.0, 350.0)));
        assert!(leg_may_show(view(), pos2(-9000.0, 350.0), pos2(10.0, 10.0)));
    }

    /// A waypoint that failed to project must not take the draw call with it.
    #[test]
    fn a_leg_that_is_not_a_number_is_refused() {
        assert!(!leg_may_show(view(), pos2(f32::NAN, 350.0), pos2(500.0, 350.0)));
        assert!(!leg_may_show(
            view(),
            pos2(500.0, 350.0),
            pos2(500.0, f32::INFINITY)
        ));
    }
}
