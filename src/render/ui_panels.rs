//! The panels themselves.
//!
//! Kept apart from the egui plumbing in [`super::ui`] so the layout of a panel
//! can be read, and changed, without wading through render passes.

use crate::render::ui::ShowWindow as _;
use egui::{Align2, Context, RichText, ScrollArea, Window};

use super::ui::{ShopView, UiAction, UiState};
use crate::shop::protocol::choose_download;
use crate::pick::PickedObject;
use crate::senc::FeatureType;

#[allow(clippy::too_many_arguments)]
pub fn build(
    ctx: &Context,
    state: &UiState<'_>,
    shop: &mut ShopView,
    charts: &mut crate::render::ui::ChartFolderView,
    instruments: &mut crate::render::ui::InstrumentView,
    routes: &mut crate::render::ui::RoutesView,
    weather: &mut crate::render::ui::WeatherView,
    plan: &mut crate::render::ui::PlanView,
    boat: &mut crate::render::ui::BoatView,
    wind: &mut crate::render::ui::WindView,
    sheet: &mut crate::render::ui_weather::SheetView,
    display: &mut crate::render::ui::DisplayView,
    free: &mut crate::render::ui::FreeChartsView,
    safety: &mut crate::render::ui_safety::SafetyView,
    logbook: &mut crate::render::ui_logbook::LogView,
    fleet: &crate::signalk::Fleet,
    actions: &mut Vec<UiAction>,
) {
    // Everything drawn on the chart goes into the panels' own background
    // layer, and goes in FIRST. egui orders layers of the same rank by
    // insertion, and layers that are not registered areas — which is what a
    // bare layer painter creates — end up in a hash map whose iteration
    // order is not stable. That is why the AIS targets sometimes drew over
    // the menus and sometimes did not. Sharing one layer with the panels
    // makes the order a fact rather than a coin toss: chart first, interface
    // over it, always.
    //
    // Within the chart, bottom to top: weather, the plan, then the vessels.
    // The boat sails over her plan, not beneath it.
    // The wash goes down first: it is background even to the barbs.
    super::ui_wind::draw_fill(ctx, &state.wind_fill);
    super::ui_wind::draw(ctx, &state.wind);
    super::ui_weather::draw_current_field(ctx, &state.current);
    if let Some(at) = state.weather_anchor {
        super::ui_weather::draw_anchor(ctx, at, !sheet.busy && sheet.data.is_some());
    }
    super::ui_routes::draw_overlay(ctx, &state.routes);
    draw_plan_pins(ctx, &state.plan_pins);
    super::ui_safety::draw_on_chart(
        ctx,
        &state.track,
        &state.log_track,
        &state.log_notes,
        state.anchor.as_ref(),
        state.mob.as_ref(),
    );
    if instruments.show_ais {
        super::ui_ais::draw(
            ctx,
            &state.ais,
            state.mpp,
            super::ui_ais::CpaAlarm {
                distance_m: instruments.cpa_alarm_nm * 1852.0,
                seconds: instruments.tcpa_alarm_min * 60.0,
            },
        );
    }
    if let Some(ref ship) = state.own_ship {
        super::ui_ownship::draw(ctx, ship);
    }

    menu_bar(
        ctx,
        shop,
        instruments,
        routes,
        plan,
        boat,
        display,
        sheet,
        safety,
        logbook,
        state.anchor.is_some(),
        actions,
    );
    // The instrument bar takes the bottom edge first. egui gives the outermost
    // edge to the panel declared first, so declaring the strip first — as this
    // did — put the strip *below* the bar, hard against the screen edge under
    // the helm's hand, which is the opposite of what was wanted.
    // A copy, so the strip that edits the route can have `routes` to itself.
    let guidance = routes.guidance.clone();
    let sources = super::ui_instruments::Sources {
        vessel: &fleet.own,
        route: guidance.as_ref(),
    };
    super::ui_instruments::bar(ctx, instruments, sources, actions);
    // What the menu bar and the instruments leave. The MOB button is
    // anchored to this and to nothing declared later: the guidance strip
    // and the weather sheet come and go, and the button must not go with
    // them. A control reached for in a hurry has to be where it was.
    let chart_floor = ctx.available_rect();
    if let Some(ref g) = routes.guidance {
        super::ui_routes::guidance_strip(ctx, g, &instruments.units);
    } else if let Some(ref why) = routes.guidance_waiting {
        super::ui_routes::waiting_strip(ctx, why);
    }
    plan_card(ctx, weather, actions);
    super::ui_routes::edit_strip(ctx, routes, actions);
    if let (Some(objects), Some(at)) = (state.picked, state.pick_geo) {
        pick_card(
            ctx,
            objects,
            at,
            state.pick_from_boat,
            state.pick_id,
            routes.editing.is_some(),
            weather.busy,
            actions,
        );
    }
    // Last of the bottom panels, so it sits *above* the instrument strip and
    // the guidance line rather than pushing them off the screen edge — the
    // same ordering rule the strip and the bar already rely on.
    super::ui_weather::sheet(ctx, sheet, wind, &instruments.units, actions);
    if let Some(objects) = state.picked {
        // The whole list, with every attribute, only when asked for.
        if pick_more_open(ctx, state.pick_id) {
            object_query(ctx, objects, state.pick_anchor, state.pick_id, actions);
        }
    }
    if shop.open {
        chart_shop(ctx, shop, charts, free, actions);
    }
    if instruments.open {
        super::ui_instruments::settings(ctx, instruments, sources, actions);
    }
    if plan.open {
        goto_window(ctx, plan, weather, &boat.name, actions);
    }
    if routes.open {
        super::ui_routes::window(ctx, routes, weather, actions);
    }
    if boat.open {
        super::ui_boat::window(ctx, boat, weather, instruments.units.depth, actions);
    }
    if display.open {
        display_window(ctx, display, instruments, boat.draft_m, actions);
    }
    let watch = super::ui_safety::WatchState {
        anchor: state.anchor.as_ref(),
        has_fix: state.own_ship.as_ref().is_some_and(|s| !s.stale),
        depth_m: state.depth_m,
    };
    if safety.open {
        super::ui_safety::window(ctx, safety, instruments, &watch, actions);
    }
    if safety.anchor_open {
        super::ui_safety::anchor_window(ctx, safety, &watch, actions);
    }
    if logbook.open {
        super::ui_logbook::window(ctx, logbook, &state.logbook, actions);
    }
    if state.no_charts {
        no_charts(ctx, shop);
    }
    // Last, over the chart area the panels have left: what a chart tap will
    // do right now, and the way back to following the boat.
    tap_mode_chip(ctx, plan, routes, actions);
    scale_bar(ctx, state.mpp);
    chart_buttons(
        ctx,
        instruments,
        display.chart_up,
        state.chart_rotation,
        state.own_ship.is_some(),
        actions,
    );
    super::ui_safety::mob_button(ctx, chart_floor, state.mob.is_some(), actions);
    if let Some(ref mob) = state.mob {
        super::ui_safety::mob_panel(ctx, chart_floor, mob, safety, actions);
    } else {
        safety.mob_clear_armed = false;
    }
    super::ui_safety::guide(ctx, safety);
    super::ui_safety::banner(ctx, &state.alarms, actions);
    eula(ctx, &mut shop.eula, actions);
    // Over everything, first start: nothing else is usable until it is read.
    super::ui_safety::notice(ctx, safety, actions);
}

/// The o-charts licence agreement: to accept before a chart set is used,
/// or to read at any time.
///
/// o-charts' conditions: the user accepts the agreement that comes with the
/// chart set before the charts are used, can get it back at any moment from
/// within the program, and understands that the charts are not official
/// charts under carriage requirements.
fn eula(ctx: &Context, view: &mut crate::render::ui::EulaView, actions: &mut Vec<UiAction>) {
    let text = |ui: &mut egui::Ui, text: &str| {
        egui::ScrollArea::vertical()
            .max_height((ctx.available_rect().height() - 260.0).max(120.0))
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.label(RichText::new(text).small());
            });
    };
    if let Some(pending) = &view.gate {
        // Dim and block everything behind it.
        let screen = ctx.screen_rect();
        egui::Area::new(egui::Id::new("eula-shade"))
            .fixed_pos(screen.min)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                let (rect, _) = ui.allocate_exact_size(screen.size(), egui::Sense::click_and_drag());
                ui.painter().rect_filled(rect, 0.0, egui::Color32::from_black_alpha(160));
            });
        Window::new("o-charts licence agreement")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .order(egui::Order::Foreground)
            .default_width(560.0)
            .show_fitted(ctx, |ui| {
                ui.label(format!(
                    "The charts in {} are licensed by o-charts. Read the agreement and accept \
                     it to use them.",
                    pending.set
                ));
                ui.add_space(4.0);
                text(ui, &pending.text);
                ui.add_space(6.0);
                ui.label(RichText::new(crate::shop::eula::NOT_OFFICIAL).strong());
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(RichText::new("I accept the agreement and understand this").strong())
                        .clicked()
                    {
                        actions.push(UiAction::EulaAccept);
                    }
                    if ui.button("Do not open these charts").clicked() {
                        actions.push(UiAction::EulaDecline);
                    }
                });
            });
        return;
    }
    let mut open = view.reading.is_some();
    if let Some((set, agreement)) = &view.reading {
        Window::new("o-charts licence agreement")
            .collapsible(false)
            .open(&mut open)
            .default_width(560.0)
            .constrain_to(ctx.available_rect())
            .show_fitted(ctx, |ui| {
                ui.label(RichText::new(set).weak().small());
                text(ui, agreement);
                ui.add_space(6.0);
                ui.label(RichText::new(crate::shop::eula::NOT_OFFICIAL).strong());
            });
    }
    if !open {
        view.reading = None;
    }
}

/// How the chart is drawn: palette, safety depth, detail.
///
/// The palette is the one a helm reaches for at dusk, so it comes first and
/// takes effect on the tap. The depths are the mariner's settings S-52 is
/// built around: water shallower than the safety contour is drawn as
/// danger, and soundings shallower than the safety depth print bold.
fn display_window(
    ctx: &Context,
    display: &mut crate::render::ui::DisplayView,
    instruments: &mut crate::render::ui::InstrumentView,
    draft_m: f64,
    actions: &mut Vec<UiAction>,
) {
    use crate::render::ui::{ChartDetail, Palette};
    let before = display.clone();
    let mut open = display.open;
    Window::new("Display")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .constrain_to(ctx.available_rect())
        .show_fitted(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Colours").strong());
                ui.selectable_value(&mut display.palette, Palette::Day, "Day");
                ui.selectable_value(&mut display.palette, Palette::Dusk, "Dusk");
                ui.selectable_value(&mut display.palette, Palette::Night, "Night");
            });
            ui.label(
                RichText::new("Night keeps your eyes adjusted to the dark; use it after sunset.")
                    .small()
                    .weak(),
            );
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                ui.label(RichText::new("Weather layer").strong());
                let mut percent = (display.weather_opacity * 100.0).round();
                if ui
                    .add(
                        egui::Slider::new(&mut percent, 10.0..=90.0)
                            .step_by(5.0)
                            .suffix(" %"),
                    )
                    .on_hover_text("How strongly the wind colours cover the chart")
                    .changed()
                {
                    display.weather_opacity = percent / 100.0;
                }
            });
            ui.add_space(8.0);

            ui.label(RichText::new("Safe water").strong());
            let known_draft = draft_m > 0.0;
            ui.add_enabled_ui(known_draft, |ui| {
                ui.checkbox(
                    &mut display.depth_from_draft,
                    "Work it out from the boat's draft",
                )
                .on_disabled_hover_text("Enter the draft in the Boat window first");
            });
            let auto = display.depth_from_draft && known_draft;
            let metres = |ui: &mut egui::Ui, v: &mut f32, max: f32| {
                crate::render::ui::stepper_f32(ui, v, 0.5, 0.0..=max, 1, " m");
            };
            if auto {
                ui.horizontal(|ui| {
                    ui.label(format!("Draft {draft_m:.1} m + clearance"));
                    metres(ui, &mut display.clearance_m, 10.0);
                    ui.label(format!(
                        "= safety depth {:.1} m",
                        draft_m as f32 + display.clearance_m
                    ));
                });
                ui.label(
                    RichText::new(
                        "The chart shades water shallower than the next depth contour \
                         at or below this as unsafe, and draws that contour bold.",
                    )
                    .small()
                    .weak(),
                );
            } else {
                egui::Grid::new("display-depths").num_columns(2).show(ui, |ui| {
                    ui.label("Safety depth");
                    metres(ui, &mut display.safety_depth_m, 50.0);
                    ui.end_row();
                    ui.label("Safety contour");
                    metres(ui, &mut display.safety_contour_m, 50.0);
                    ui.end_row();
                });
            }
            egui::Grid::new("display-contours").num_columns(2).show(ui, |ui| {
                ui.label("Shallow contour");
                metres(ui, &mut display.shallow_contour_m, 50.0);
                ui.end_row();
                ui.label("Deep contour");
                metres(ui, &mut display.deep_contour_m, 100.0);
                ui.end_row();
            });
            ui.add_space(8.0);

            ui.label(RichText::new("Chart detail").strong());
            ui.horizontal(|ui| {
                ui.selectable_value(&mut display.detail, ChartDetail::Base, "Base")
                    .on_hover_text("Coastline, dangers and the safety contour only");
                ui.selectable_value(&mut display.detail, ChartDetail::Standard, "Standard")
                    .on_hover_text("What an ECDIS shows by default");
                ui.selectable_value(&mut display.detail, ChartDetail::All, "All")
                    .on_hover_text("Everything the chart carries");
            });
            ui.checkbox(&mut display.show_text, "Names and light descriptions");
            ui.checkbox(&mut display.show_soundings, "Soundings");
            ui.separator();
            ui.label(RichText::new("Interface size").strong());
            ui.horizontal_wrapped(|ui| {
                for size in crate::render::ui::UiSize::ALL {
                    ui.selectable_value(&mut display.size, size, size.label());
                }
            });
            ui.separator();
            super::ui_instruments::layout(ui, instruments, actions);
        });
    display.open = open;
    // Applied as they change, but a drag emits a change per frame, and each
    // one restarts the tiles: wait for the drag to finish.
    let dragging = ctx.input(|i| i.pointer.any_down());
    if *display != before && !dragging {
        actions.push(UiAction::DisplayChanged);
    } else if *display != before {
        ctx.data_mut(|d| d.insert_temp(egui::Id::new("display-dirty"), true));
    } else if !dragging
        && ctx.data_mut(|d| d.remove_temp::<bool>(egui::Id::new("display-dirty"))).is_some()
    {
        actions.push(UiAction::DisplayChanged);
    }
}

/// While a tap means something other than "what is that?", say so on the
/// chart itself, with the way out beside it. A mode that lives only in a
/// window the user has scrolled past, or in small grey text in the menu
/// bar, is a mode the user does not know they are in.
fn tap_mode_chip(
    ctx: &Context,
    plan: &mut crate::render::ui::PlanView,
    routes: &crate::render::ui::RoutesView,
    actions: &mut Vec<UiAction>,
) {
    use crate::render::ui::PlanPickTarget;
    // An armed planner pin is a one-shot and claims the tap first; an open
    // route is a standing mode (the same order `tap_at_screen` uses).
    let (text, button) = if let Some(target) = plan.picking {
        let text = match target {
            PlanPickTarget::From => "Tap the chart to set the start",
            PlanPickTarget::To => "Tap the chart to set the destination",
        };
        (text, "Cancel")
    } else {
        // A route being drawn has a strip of its own under the chart.
        let _ = (routes, &actions);
        return;
    };
    // Bottom centre of the chart: windows open from the top, so down here
    // it neither covers their buttons nor hides under them.
    let area = ctx.available_rect();
    egui::Area::new(egui::Id::new("tap-mode-chip"))
        .fixed_pos(egui::pos2(area.center().x, area.bottom() - 10.0))
        .pivot(Align2::CENTER_BOTTOM)
        .order(egui::Order::Middle)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(text).strong());
                    if ui.button(button).on_hover_text("Esc does the same").clicked() {
                        plan.picking = None;
                    }
                });
            });
        });
}

/// With no chart loaded, say so on the empty sea and offer the way out.
/// A new boat's first screen used to be a blue rectangle and a menu bar.
fn no_charts(ctx: &Context, shop: &mut ShopView) {
    if shop.open {
        return;
    }
    let area = ctx.available_rect();
    egui::Area::new(egui::Id::new("no-charts"))
        .fixed_pos(area.center())
        .pivot(Align2::CENTER_CENTER)
        .order(egui::Order::Middle)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("No charts yet").strong());
                    ui.label("Download free NOAA charts, charts you bought at o-charts, or open a folder.");
                    if ui.button("Get charts").clicked() {
                        shop.open = true;
                    }
                });
            });
        });
}

/// Whether the full object list is open for this tap. Kept per tap: a new
/// tap starts again with the card alone.
fn pick_more_open(ctx: &Context, pick_id: u64) -> bool {
    ctx.data(|d| d.get_temp::<u64>(egui::Id::new("pick-more"))) == Some(pick_id)
}

/// What a picked object is called on the card: its own name where it has
/// one, what it is, and how the chart reads it — in a sailor's words, with
/// the catalogue's codes left to the full list.
fn headline(o: &PickedObject) -> String {
    let attr = |name: &str| {
        o.attributes
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.trim())
            .filter(|v| !v.is_empty())
    };
    let mut text = match attr("OBJNAM") {
        Some(name) => format!("{name} · {}", o.title),
        None => o.title.clone(),
    };
    match o.acronym.as_str() {
        "DEPCNT" => {
            if let Some(depth) = attr("VALDCO") {
                text = format!("{text} {depth} m");
            }
        }
        "DEPARE" | "DRGARE" => match (attr("DRVAL1"), attr("DRVAL2")) {
            (Some(a), Some(b)) => text = format!("{text} {a}–{b} m"),
            (Some(a), None) => text = format!("{text} {a} m"),
            _ => {}
        },
        _ => {}
    }
    if crate::pick::is_overhead(&o.acronym) {
        text = overhead_text(o.acronym == "BRIDGE", &attr);
        if let Some(name) = attr("OBJNAM") {
            text = format!("{name} · {text}");
        }
    }
    if let Some(summary) = &o.summary {
        text = format!("{text} · {summary}");
    }
    text
}

/// A bridge or an overhead cable, as the answer to "does the mast go
/// under?": what it is, how much air there is, and how wide the opening.
///
/// The chart labels it `clr 16.0` (S-52), which says the same to someone
/// who knows the abbreviation and nothing about whether the bridge opens.
/// S-57 gives the clearances as VERCLR (fixed), VERCCL and VERCOP (an
/// opening bridge closed and open), VERCSA (the safe clearance under a
/// power cable, less than the physical one) and HORCLR (width).
fn overhead_text<'a>(bridge: bool, attr: &impl Fn(&str) -> Option<&'a str>) -> String {
    let metres = |v: &str| format!("{} m", v.trim_end_matches(".0"));
    let kind = if bridge {
        attr("CATBRG").map_or("Bridge".to_string(), |kind| {
            let mut c = kind.chars();
            c.next().map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
        })
    } else {
        "Overhead cable or pipe".to_string()
    };
    let air = match (attr("VERCSA"), attr("VERCLR"), attr("VERCCL"), attr("VERCOP")) {
        (Some(safe), ..) => format!("safe clearance {}", metres(safe)),
        (_, _, Some(closed), Some(open)) => {
            format!("clearance {} closed, {} open", metres(closed), metres(open))
        }
        (_, _, Some(closed), None) => {
            format!("clearance {} closed, no limit charted when open", metres(closed))
        }
        (_, Some(fixed), ..) => format!("clearance {}", metres(fixed)),
        (_, _, None, Some(open)) => format!("clearance {} open", metres(open)),
        _ => "clearance not charted".to_string(),
    };
    match attr("HORCLR") {
        Some(width) => format!("{kind} · {air} · {} wide", metres(width)),
        None => format!("{kind} · {air}"),
    }
}

/// How far and which way, as the card says it: metres under a cable,
/// nautical miles beyond.
fn range_text(metres: f64, bearing_deg: f64) -> String {
    let nm = metres / crate::geo::METRES_PER_NM;
    let range = if nm < 0.1 {
        format!("{metres:.0} m")
    } else if nm < 10.0 {
        format!("{nm:.2} NM")
    } else {
        format!("{nm:.1} NM")
    };
    format!("{range} · {} from the boat", crate::geo::format_bearing(bearing_deg))
}

/// The card for a tap on the chart: what is there, where it is, and what
/// can be done about it.
///
/// A tap used to open a list — twenty-seven objects in a harbour mouth, a
/// depth contour first, each with a Copy button and nothing else. Going
/// there, adding the place to a route or asking for its weather all began
/// somewhere else on the screen. The chart is where a sailor points, so
/// the tap is where those start.
#[allow(clippy::too_many_arguments)]
fn pick_card(
    ctx: &Context,
    objects: &[PickedObject],
    at: [f64; 2],
    from_boat: Option<(f64, f64)>,
    pick_id: u64,
    editing_route: bool,
    planning: bool,
    actions: &mut Vec<UiAction>,
) {
    let [lat, lon] = at;
    let lead = objects.iter().find(|o| !is_cell_description(&o.acronym));
    egui::TopBottomPanel::bottom("pick-card").show(ctx, |ui| {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            // Clear of the MOB button's corner.
            ui.add_space(super::ui_routes::MOB_CORNER);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    let title = lead.map(headline).unwrap_or_else(|| "Nothing charted here".into());
                    ui.add(egui::Label::new(RichText::new(title).strong()).wrap());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Close").clicked() {
                            actions.push(UiAction::DismissPick);
                        }
                    });
                });
                let mut place = crate::geo::format_latlon(lat, lon);
                if let Some((metres, bearing)) = from_boat {
                    place = format!("{place} · {}", range_text(metres, bearing));
                }
                ui.label(RichText::new(place).weak());
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(!planning, egui::Button::new("Sail here"))
                        .on_hover_text("Weather-routed passage to this place")
                        .clicked()
                    {
                        actions.push(UiAction::PlanTo { lat, lon, sail: true });
                    }
                    if ui
                        .add_enabled(!planning, egui::Button::new("Motor here"))
                        .on_hover_text("Shortest safe route to this place")
                        .clicked()
                    {
                        actions.push(UiAction::PlanTo { lat, lon, sail: false });
                    }
                    let add = if editing_route { "Add to route" } else { "Start a route here" };
                    if ui.button(add).clicked() {
                        actions.push(UiAction::RouteAddPoint { lat, lon });
                    }
                    if ui.button("Weather here").clicked() {
                        actions.push(UiAction::WeatherAt { lat, lon });
                    }
                    if !objects.is_empty() {
                        let open = pick_more_open(ui.ctx(), pick_id);
                        if ui
                            .selectable_label(open, format!("More ({})", objects.len()))
                            .on_hover_text("Everything charted here, with every attribute")
                            .clicked()
                        {
                            ui.ctx().data_mut(|d| {
                                let id = egui::Id::new("pick-more");
                                if open {
                                    d.remove::<u64>(id);
                                } else {
                                    d.insert_temp(id, pick_id);
                                }
                            });
                        }
                    }
                });
            });
        });
        ui.add_space(4.0);
    });
}

/// The planner's card, under the chart: what it is doing, what it found,
/// or why it could not — and, for a plan, what to do with it.
///
/// A finished plan used to be a line of small text in the menu bar and a
/// route drawn on the chart. To follow it the crew opened Routes, found
/// its row and pressed Activate. The answer to "where to?" is "follow
/// this", so that is the first button.
fn plan_card(ctx: &Context, weather: &mut crate::render::ui::WeatherView, actions: &mut Vec<UiAction>) {
    if !weather.busy && weather.planned.is_none() && weather.status.is_empty() {
        return;
    }
    egui::TopBottomPanel::bottom("plan-card").show(ctx, |ui| {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            // Clear of the MOB button's corner.
            ui.add_space(super::ui_routes::MOB_CORNER);
            // The words on lines of their own, as many as they need: a
            // warning about the route is the part not to cut short.
            ui.vertical(|ui| {
                if weather.busy {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(&weather.status);
                    });
                    if ui.button("Cancel").on_hover_text("Stop planning this passage").clicked() {
                        actions.push(UiAction::PlanCancel);
                    }
                } else if let Some(planned) = weather.planned.clone() {
                    ui.add(egui::Label::new(RichText::new(&weather.status).strong()).wrap());
                    if let Some(arrival) = &planned.arrival {
                        ui.label(arrival);
                    }
                    let route_id = planned.route_id;
                    ui.horizontal(|ui| {
                        if ui.button("Follow").on_hover_text("Steer by this route now").clicked() {
                            actions.push(UiAction::RouteActivate { route_id });
                            actions.push(UiAction::PlanDismiss);
                        }
                        if ui
                            .button("Look at it")
                            .on_hover_text("Put the whole route on the screen")
                            .clicked()
                        {
                            actions.push(UiAction::RouteShow { route_id });
                        }
                        if ui.button("Discard").on_hover_text("Delete this route").clicked() {
                            actions.push(UiAction::RouteDelete { route_id });
                            actions.push(UiAction::PlanDismiss);
                        }
                        if ui
                            .button("Keep")
                            .on_hover_text("Keep the route under Routes and close this")
                            .clicked()
                        {
                            actions.push(UiAction::PlanDismiss);
                        }
                    });
                } else {
                    ui.add(egui::Label::new(&weather.status).wrap());
                    if ui.button("OK").clicked() {
                        actions.push(UiAction::PlanDismiss);
                    }
                }
            });
        });
        ui.add_space(4.0);
    });
}

/// A length for the scale bar: the longest round distance that fits in
/// `max_pt` points when one point covers `ground_m_per_pt` metres of sea.
/// Round means 1, 2 or 5 times a power of ten — in metres up to 100 m, in
/// nautical miles (a cable, 0.1 NM, and up) beyond. `None` when there is
/// no scale to draw.
fn scale_bar_length(ground_m_per_pt: f32, max_pt: f32) -> Option<(f32, String)> {
    if !(ground_m_per_pt.is_finite() && ground_m_per_pt > 0.0 && max_pt > 0.0) {
        return None;
    }
    const NM: f32 = 1852.0;
    let metres = [1.0_f32, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0]
        .into_iter()
        .map(|m| (m, format!("{m:.0} m")));
    let miles = (-1..=3).flat_map(|exp| {
        [1.0_f32, 2.0, 5.0].into_iter().map(move |d| {
            let nm = d * 10f32.powi(exp);
            let label = if nm < 1.0 { format!("{nm:.1} NM") } else { format!("{nm:.0} NM") };
            (nm * NM, label)
        })
    });
    metres
        .chain(miles)
        .map(|(m, label)| (m / ground_m_per_pt, label))
        .filter(|(pt, _)| *pt <= max_pt)
        .last()
        // Off either end of the list the longest that fits is a stub: no
        // bar is better than one too short to read a distance off.
        .filter(|(pt, _)| *pt >= max_pt * 0.25)
}

/// The scale bar: how far is that, without a tap. Bottom left of the chart,
/// beside the MOB button, drawn on the chart itself — it is part of the
/// chart, not a control, and takes no taps.
fn scale_bar(ctx: &Context, ground_m_per_pt: f32) {
    let Some((length, label)) = scale_bar_length(ground_m_per_pt, 120.0) else { return };
    let area = ctx.available_rect();
    let left = area.left() + 90.0;
    let y = area.bottom() - 16.0;
    if left + length + 8.0 > area.right() || y < area.top() + 30.0 {
        return;
    }
    let visuals = ctx.style().visuals.clone();
    let ink = visuals.strong_text_color();
    let paper = visuals.panel_fill.gamma_multiply(0.85);
    let painter = ctx.layer_painter(egui::LayerId::background()).with_clip_rect(area);
    let galley = painter.layout_no_wrap(label, egui::FontId::proportional(12.0), ink);
    // A plate behind it: the chart under it may be any colour at all.
    let plate = egui::Rect::from_min_max(
        egui::pos2(left - 6.0, y - galley.size().y - 6.0),
        egui::pos2(left + length.max(galley.size().x) + 6.0, y + 6.0),
    );
    painter.rect_filled(plate, 4.0, paper);
    painter.galley(egui::pos2(left, y - galley.size().y - 3.0), galley, ink);
    let stroke = egui::Stroke::new(2.0_f32, ink);
    painter.line_segment([egui::pos2(left, y), egui::pos2(left + length, y)], stroke);
    for x in [left, left + length] {
        painter.line_segment([egui::pos2(x, y - 4.0), egui::pos2(x, y + 1.0)], stroke);
    }
}

/// The buttons in the chart's corner: which way up, and follow the boat.
///
/// Follow is a button because panning by hand turns it off, which is right,
/// but the only way back used to be a checkbox in the Instruments settings —
/// one accidental drag and the chart stopped tracking the boat for good, with
/// nothing on screen to say so. Pressing it recentres at the zoom already
/// chosen.
///
/// The orientation button carries a north arrow that turns with the chart, so
/// a head-up or twisted chart always says where north went.
fn chart_buttons(
    ctx: &Context,
    instruments: &crate::render::ui::InstrumentView,
    chart_up: crate::render::ui::ChartUp,
    rotation: f32,
    has_boat: bool,
    actions: &mut Vec<UiAction>,
) {
    use crate::render::ui::ChartUp;
    let area = ctx.available_rect();
    egui::Area::new(egui::Id::new("follow-button"))
        .fixed_pos(egui::pos2(area.right() - 10.0, area.bottom() - 10.0))
        .pivot(Align2::RIGHT_BOTTOM)
        .order(egui::Order::Middle)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                // A column in the corner, each button a fingertip across.
                // Zoom has buttons because a pinch takes two fingers and a
                // steady hand, and at the helm there is often neither.
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    if icon_button(ui, false, Icon::Plus).on_hover_text("Zoom in").clicked() {
                        actions.push(UiAction::Zoom { factor: ZOOM_STEP });
                    }
                    if icon_button(ui, false, Icon::Minus).on_hover_text("Zoom out").clicked() {
                        actions.push(UiAction::Zoom { factor: 1.0 / ZOOM_STEP });
                    }
                    if has_boat {
                        let on = instruments.follow;
                        if icon_button(ui, on, Icon::Follow)
                            .on_hover_text(if on {
                                "Following the boat. Drag the chart to look elsewhere."
                            } else {
                                "Centre the chart on the boat and keep it there"
                            })
                            .clicked()
                        {
                            actions.push(UiAction::FollowSet { on: !on });
                        }
                    }
                    let hover = match chart_up {
                        ChartUp::North => "North up. Tap for head-up.",
                        ChartUp::Head => "Head up: the boat's heading is at the top. Tap for north-up.",
                        ChartUp::Free => "Turned by hand. Tap for north-up.",
                    };
                    if compass_button(ui, rotation, chart_up != ChartUp::North)
                        .on_hover_text(hover)
                        .clicked()
                    {
                        actions.push(UiAction::ChartUpSet { mode: chart_up.next() });
                    }
                });
            });
        });
}

/// One press of a zoom button: the same step the `+` and `-` keys take.
const ZOOM_STEP: f32 = 1.5;

/// The side of a button on the chart, points: a fingertip, with room.
const CHART_BUTTON: f32 = 44.0;

#[derive(Clone, Copy)]
enum Icon {
    Plus,
    Minus,
    /// A ring round a dot: the boat held in the middle of the chart.
    Follow,
}

/// A square button on the chart whose face is drawn, not written: the
/// strokes are the same in every font and every palette, and need no
/// translating.
fn icon_button(ui: &mut egui::Ui, selected: bool, icon: Icon) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(CHART_BUTTON, CHART_BUTTON), egui::Sense::click());
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let visuals = ui.style().interact_selectable(&response, selected);
    let painter = ui.painter();
    painter.rect(rect, 6.0, visuals.bg_fill, visuals.bg_stroke);
    let c = rect.center();
    let stroke = egui::Stroke::new(2.5_f32, visuals.fg_stroke.color);
    let arm = CHART_BUTTON * 0.22;
    match icon {
        Icon::Plus => {
            painter.line_segment([c - egui::vec2(arm, 0.0), c + egui::vec2(arm, 0.0)], stroke);
            painter.line_segment([c - egui::vec2(0.0, arm), c + egui::vec2(0.0, arm)], stroke);
        }
        Icon::Minus => {
            painter.line_segment([c - egui::vec2(arm, 0.0), c + egui::vec2(arm, 0.0)], stroke);
        }
        Icon::Follow => {
            painter.circle_stroke(c, arm, stroke);
            painter.circle_filled(c, arm * 0.35, visuals.fg_stroke.color);
        }
    }
    response
}

/// The orientation button: a round compass whose needle points where north
/// is on the screen (`rotation` is the true bearing at the top), filled in
/// when the chart is not north-up. An icon rather than words, because it sits
/// on the chart and the chart is what the screen is for.
fn compass_button(ui: &mut egui::Ui, rotation: f32, active: bool) -> egui::Response {
    let size = CHART_BUTTON;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let visuals = ui.style().interact_selectable(&response, active);
    let painter = ui.painter();
    let c = rect.center();
    let r = size * 0.5 - 1.0;
    painter.circle(c, r, visuals.bg_fill, visuals.bg_stroke);

    let ink = visuals.fg_stroke.color;
    let north = north_on_screen(rotation);
    let east = egui::vec2(-north.y, north.x);

    // A tick at each cardinal point, just inside the rim.
    for dir in [north, east, -north, -east] {
        painter.line_segment(
            [c + dir * (r * 0.78), c + dir * (r * 0.93)],
            egui::Stroke::new(1.2_f32, ink.gamma_multiply(0.6)),
        );
    }

    // The needle: a slim diamond, red to the north.
    let tip = r * 0.62;
    let waist = r * 0.2;
    let red = crate::render::theme::current().red;
    painter.add(egui::Shape::convex_polygon(
        vec![c + north * tip, c + east * waist, c - east * waist],
        red,
        egui::Stroke::NONE,
    ));
    painter.add(egui::Shape::convex_polygon(
        vec![c - north * tip, c - east * waist, c + east * waist],
        ink.gamma_multiply(0.75),
        egui::Stroke::NONE,
    ));
    painter.circle_filled(c, r * 0.09, visuals.bg_fill);

    response
}

/// Which way north lies on the screen, as a unit vector (screen y runs
/// down), when `rotation` is the true bearing at the top: straight up when
/// north-up, and `rotation` anticlockwise from up otherwise.
fn north_on_screen(rotation: f32) -> egui::Vec2 {
    egui::vec2(-rotation.sin(), -rotation.cos())
}

/// The planner's pins: a classic map pin — a filled head on a stem whose
/// point is the position — green for the start, red for the destination.
/// Drawn whenever a field holds a real position, so what the router will be
/// given is visible before anyone presses Sail.
fn draw_plan_pins(ctx: &Context, pins: &[crate::render::ui::PlanPin]) {
    if pins.is_empty() {
        return;
    }
    let painter = ctx.layer_painter(egui::LayerId::background());
    for pin in pins {
        let tip = egui::pos2(pin.screen[0], pin.screen[1]);
        let theme = crate::render::theme::current();
        let color = if pin.is_start { theme.green } else { theme.red };
        let r = 7.0;
        let head = egui::pos2(tip.x, tip.y - 14.0);
        painter.add(egui::Shape::convex_polygon(
            vec![
                tip,
                egui::pos2(head.x - r * 0.55, head.y + r * 0.4),
                egui::pos2(head.x + r * 0.55, head.y + r * 0.4),
            ],
            color,
            egui::Stroke::NONE,
        ));
        painter.circle(head, r, color, egui::Stroke::new(1.5_f32, theme.on_signal));
        painter.circle_filled(head, 2.5, theme.on_signal);
    }
}

/// What the position fields accept, for their hover.
const POSITION_FORMATS: &str = "Press \"Tap the chart\" and then the chart, or type a position:\n\
    56°24.6'N 10°58.8'E  ·  56 24.6N 10 58.8E  ·  56.41, 10.98";

/// A thin strip along the top. Deliberately thin: the chart is the instrument,
/// and every row of pixels the interface takes is a row of sea it does not show.
///
/// What is on it is what is reached for under way, named for the job: where
/// to go, the routes, the weather, the anchor, the light. What is set up
/// once — charts, the connection to the boat, the boat herself, how the
/// chart is drawn, the alarms' limits — and the logbook are one tap further,
/// under More. The bar used to list all eight windows side by side with the
/// planner's two fields after them: thirteen controls, the set-up first.
#[allow(clippy::too_many_arguments)]
fn menu_bar(
    ctx: &Context,
    shop: &mut ShopView,
    instruments: &mut crate::render::ui::InstrumentView,
    routes: &crate::render::ui::RoutesView,
    plan: &mut crate::render::ui::PlanView,
    boat: &mut crate::render::ui::BoatView,
    display: &mut crate::render::ui::DisplayView,
    sheet: &crate::render::ui_weather::SheetView,
    safety: &mut crate::render::ui_safety::SafetyView,
    logbook: &mut crate::render::ui_logbook::LogView,
    anchored: bool,
    actions: &mut Vec<UiAction>,
) {
    egui::TopBottomPanel::top("menu").show(ctx, |ui| {
        ui.horizontal_wrapped(|ui| {
            if ui
                .selectable_label(plan.open, "Go to…")
                .on_hover_text("Plan a passage: where from, where to, sail or motor")
                .clicked()
            {
                plan.open = !plan.open;
            }
            if ui.selectable_label(routes.open, "Routes").clicked() {
                actions.push(UiAction::RoutesOpen);
            }
            if ui
                .selectable_label(sheet.show, "Weather")
                .on_hover_text("Wind, sea, current, rain and tide, on one time axis")
                .clicked()
            {
                actions.push(UiAction::WeatherSheetToggle);
            }
            // Lit while the watch is set, so a glance says the boat is
            // being watched.
            if ui
                .selectable_label(safety.anchor_open || anchored, "Anchor")
                .on_hover_text(if anchored { "The anchor watch is set" } else { "Set an anchor watch" })
                .clicked()
            {
                safety.anchor_open = !safety.anchor_open;
            }
            ui.separator();
            // The palette, one tap from anywhere: it is changed every
            // evening and every morning, and was three taps into a window.
            if ui
                .button(display.palette.label())
                .on_hover_text(format!("Chart colours. Tap for {}.", display.palette.next().label()))
                .clicked()
            {
                display.palette = display.palette.next();
                actions.push(UiAction::DisplayChanged);
            }
            let any_open = shop.open || instruments.open || boat.open || display.open || safety.open || logbook.open;
            let more = egui::RichText::new("More");
            let more = if any_open { more.strong() } else { more };
            ui.menu_button(more, |ui| {
                // Each a full-height row: this is a menu for fingers.
                ui.spacing_mut().interact_size.y = 40.0;
                let item = |ui: &mut egui::Ui, open: &mut bool, label: &str, hint: &str| {
                    if ui.selectable_label(*open, label).on_hover_text(hint).clicked() {
                        *open = !*open;
                        ui.close_menu();
                    }
                };
                item(ui, &mut shop.open, "Charts", "Get charts: o-charts, free NOAA charts, or a folder");
                item(
                    ui,
                    &mut instruments.open,
                    "Connection",
                    "The Signal K server on the boat, and what the instrument bar shows",
                );
                item(ui, &mut boat.open, "Boat", "Her name, her draft and air draft, her sailing speeds");
                item(
                    ui,
                    &mut display.open,
                    "Display",
                    "Safety depth, chart detail, units, where the instrument bar sits",
                );
                item(ui, &mut safety.open, "Alarms", "Collision and shallow-water alarms, and the man overboard guide");
                item(ui, &mut logbook.open, "Log", "The logbook: where the boat went, the numbers, your notes");
                if ui.selectable_label(safety.notice_open, "About").clicked() {
                    safety.notice_open = true;
                    ui.close_menu();
                }
            });
            // With the instrument bar hidden, its connection dot would go
            // with it; keep one here so a dropped link is still visible.
            let bar_shown = instruments.position != crate::render::ui::BarPosition::Hidden
                && !instruments.tiles().is_empty();
            if !bar_shown && instruments.active {
                let colour = if instruments.connected {
                    crate::render::theme::current().green
                } else {
                    crate::render::theme::current().amber
                };
                let (rect, response) =
                    ui.allocate_exact_size(egui::Vec2::splat(12.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 4.0, colour);
                response.on_hover_text(&instruments.status);
            }
        });
    });
}

/// The Go to window: where from, where to, sail or motor — and what the
/// router should allow for.
///
/// The planner's two fields and two verbs used to live in the menu bar,
/// always on show; its options were at the foot of the Routes list. Most
/// passages now start from a tap on the chart ("Sail here"), so this is
/// for the rest: a position typed in, a start that is not the boat, the
/// options.
fn goto_window(
    ctx: &Context,
    plan: &mut crate::render::ui::PlanView,
    weather: &mut crate::render::ui::WeatherView,
    boat_name: &str,
    actions: &mut Vec<UiAction>,
) {
    use crate::render::ui::PlanPickTarget;
    let mut open = plan.open;
    let mut changed = false;
    Window::new("Go to")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .constrain_to(ctx.available_rect())
        .default_width(380.0)
        .show_fitted(ctx, |ui| {
            let field = |ui: &mut egui::Ui, label: &str, text: &mut String, hint: &str, target: PlanPickTarget, picking: &mut Option<PlanPickTarget>| {
                let mut edited = None;
                ui.horizontal(|ui| {
                    ui.add_sized([44.0, 32.0], egui::Label::new(RichText::new(label).weak()));
                    let edit = ui
                        .add(egui::TextEdit::singleline(text).hint_text(hint).desired_width(190.0))
                        .on_hover_text(POSITION_FORMATS);
                    let armed = *picking == Some(target);
                    if ui
                        .selectable_label(armed, "Tap the chart")
                        .on_hover_text("Then tap the place on the chart")
                        .clicked()
                    {
                        *picking = (!armed).then_some(target);
                    }
                    edited = Some(edit);
                });
                edited.expect("the row was drawn")
            };
            let from = field(ui, "From", &mut plan.from, "the boat", PlanPickTarget::From, &mut plan.picking);
            let to = field(ui, "To", &mut plan.to, "56°24.6'N 10°58.8'E", PlanPickTarget::To, &mut plan.picking);
            changed = from.changed() || to.changed();
            // Enter in the destination field is the promise the layout makes:
            // type where you are going, press enter, sail.
            let entered = to.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let go = |sail: bool| UiAction::PlanRoute {
                from: plan.from.clone(),
                to: plan.to.clone(),
                sail,
            };
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                // Greyed while a plan runs rather than looking ready and
                // ignoring the tap.
                if (ui
                    .add_enabled(!weather.busy, egui::Button::new("Sail"))
                    .on_hover_text("Weather-routed passage on the forecast and the boat's sailing speeds")
                    .clicked()
                    || entered)
                    && !weather.busy
                {
                    actions.push(go(true));
                }
                if ui
                    .add_enabled(!weather.busy, egui::Button::new("Motor"))
                    .on_hover_text("Shortest safe route, no weather")
                    .clicked()
                {
                    actions.push(go(false));
                }
            });
            ui.separator();
            ui.label(RichText::new("When sailing").strong());
            super::ui_routes::routing_options(ui, weather, boat_name, actions);
        });
    plan.open = open;
    // A message about the last plan stops being true once the plan's
    // fields change — an error about a malformed destination must not
    // outlive the fix.
    if changed && !weather.busy && weather.planned.is_none() {
        weather.status.clear();
    }
}

/// Free charts: NOAA's packages, one row per state.
///
/// Each row says everything about its state at a glance — size, whether it
/// is installed and when, whether NOAA has a newer one — and offers the one
/// thing to do next. A download shows its progress in its own row, so there
/// is nothing to go and look for.
fn free_charts(
    ui: &mut egui::Ui,
    free: &mut crate::render::ui::FreeChartsView,
    actions: &mut Vec<UiAction>,
) {
    if !free.probed {
        actions.push(UiAction::FreeChartsRefresh);
    }
    ui.label(RichText::new("Free charts from NOAA").strong());
    ui.label(
        RichText::new(
            "Official charts of US waters, free to use and updated every week. \
             They are kept with your other charts and appear on the map as soon \
             as they are downloaded.",
        )
        .small()
        .weak(),
    );
    ui.add_space(4.0);
    ui.add(
        egui::TextEdit::singleline(&mut free.filter)
            .hint_text("Find a state…")
            .desired_width(220.0),
    );
    ui.add_space(4.0);

    let mb = |b: u64| {
        if b >= 10_000_000 {
            format!("{} MB", b / 1_000_000)
        } else {
            format!("{:.1} MB", b as f64 / 1e6)
        }
    };
    let day = |ymd: &str| {
        chrono::NaiveDate::parse_from_str(ymd, "%Y-%m-%d")
            .map(|d| d.format("%-d %b").to_string())
            .unwrap_or_else(|_| ymd.to_string())
    };
    let needle = free.filter.trim().to_lowercase();
    let busy = free.active.is_some();

    ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
        egui::Grid::new("free-charts")
            .num_columns(3)
            .striped(true)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                // Yours first, then the rest: what is on the plotter is what
                // gets updated and removed, and should not need finding.
                let mut rows = free.rows.clone();
                rows.sort_by_key(|r| (r.installed.is_none(), r.name));
                for row in rows {
                    if !needle.is_empty()
                        && !row.name.to_lowercase().contains(&needle)
                        && !row.code.eq_ignore_ascii_case(&needle)
                    {
                        continue;
                    }
                    ui.label(row.name);
                    // What there is to know, in one short line.
                    let size = row.remote.as_ref().map(|r| mb(r.size)).unwrap_or_else(|| "…".into());
                    let newer = match (&row.installed, &row.remote) {
                        (Some(i), Some(r)) => crate::shop::noaa::update_available(i, r),
                        _ => false,
                    };
                    match &row.installed {
                        Some(i) if newer => ui.label(
                            RichText::new(format!("installed {} · update available", day(&i.downloaded)))
                                .small()
                                .color(crate::render::theme::current().blue),
                        ),
                        Some(i) => ui.label(
                            RichText::new(if row.remote.is_some() {
                                format!("installed {} · up to date", day(&i.downloaded))
                            } else {
                                format!("installed {}", day(&i.downloaded))
                            })
                            .small()
                            .weak(),
                        ),
                        None => match free.failed.as_ref().filter(|(c, _)| c == row.code) {
                            Some((_, why)) => ui
                                .label(
                                    RichText::new("download failed")
                                        .small()
                                        .color(crate::render::theme::current().red),
                                )
                                .on_hover_text(why.as_str()),
                            None => ui.label(RichText::new(size).small().weak()),
                        },
                    };
                    ui.horizontal(|ui| {
                        let active = free.active.as_ref().filter(|(c, _, _)| c == row.code);
                        if let Some((_, done, total)) = active {
                            let f = if *total > 0 { *done as f32 / *total as f32 } else { 0.0 };
                            ui.add(
                                egui::ProgressBar::new(f)
                                    .desired_width(150.0)
                                    .text(if *total > 0 && done >= total {
                                        // Unzipping a big state takes a while.
                                        "installing…".to_string()
                                    } else if *total > 0 {
                                        format!("{} of {}", mb(*done), mb(*total))
                                    } else {
                                        mb(*done)
                                    }),
                            );
                            if ui.button("Cancel").clicked() {
                                actions.push(UiAction::FreeChartsCancel);
                            }
                        } else if free.confirm_remove.as_deref() == Some(row.code) {
                            ui.label(RichText::new("Remove these charts?").small());
                            if ui
                                .button(RichText::new("Remove").color(crate::render::theme::current().red))
                                .clicked()
                            {
                                free.confirm_remove = None;
                                actions.push(UiAction::FreeChartsRemove { code: row.code.into() });
                            }
                            if ui.button("Keep").clicked() {
                                free.confirm_remove = None;
                            }
                        } else if row.installed.is_some() {
                            // Update only when there is one: a greyed button
                            // reads as something broken, not as "up to date".
                            if newer
                                && ui
                                    .add_enabled(!busy, egui::Button::new("Update"))
                                    .on_disabled_hover_text("one download at a time")
                                    .clicked()
                            {
                                actions.push(UiAction::FreeChartsDownload { code: row.code.into() });
                            }
                            if ui.add_enabled(!busy, egui::Button::new("Remove")).clicked() {
                                free.confirm_remove = Some(row.code.into());
                            }
                        } else if ui
                            .add_enabled(
                                !busy,
                                egui::Button::new(
                                    if free.failed.as_ref().is_some_and(|(c, _)| c == row.code) {
                                        "Try again"
                                    } else {
                                        "Download"
                                    },
                                ),
                            )
                            .on_disabled_hover_text("one download at a time")
                            .clicked()
                        {
                            actions.push(UiAction::FreeChartsDownload { code: row.code.into() });
                        }
                    });
                    ui.end_row();
                }
            });
    });
    if !free.status.is_empty() {
        ui.add_space(4.0);
        let text = RichText::new(&free.status);
        ui.label(if free.status.starts_with("Download failed") {
            text.color(crate::render::theme::current().red)
        } else {
            text
        });
    }
    if !free.folder.is_empty() {
        ui.add_space(4.0);
        ui.label(RichText::new(format!("Saved in {}", free.folder)).small().weak());
    }
    ui.add_space(4.0);
    ui.label(
        RichText::new("Source: NOAA Office of Coast Survey, charts.noaa.gov")
            .small()
            .weak(),
    );
}

/// Walk the disk for a folder of charts.
///
/// A list of folders with big rows rather than a native file dialog: the
/// plotter runs on a Raspberry Pi behind a touchscreen, where the OS dialog
/// is either absent or unusable with a finger, and where pulling in GTK for
/// one button would be the heaviest dependency in the build. This also looks
/// and behaves the same on the Mac it is developed on.
fn chart_folder_browser(
    ui: &mut egui::Ui,
    charts: &mut crate::render::ui::ChartFolderView,
    actions: &mut Vec<UiAction>,
) {
    charts.rescan();
    let here = std::path::PathBuf::from(&charts.at);

    // Words, not symbols. egui's bundled fonts have no 🗀, no ⌂ and no ↑, and
    // every one of them drew as an empty box — the same trap the weather
    // sheet's chevrons fell into. A word cannot be missing from a font.
    ui.horizontal(|ui| {
        if ui.button("Home").clicked() {
            if let Some(home) = dirs::home_dir() {
                charts.go(home.display().to_string());
            }
        }
        if ui
            .add_enabled(here.parent().is_some(), egui::Button::new("Up"))
            .on_hover_text("The folder above this one")
            .clicked()
        {
            if let Some(parent) = here.parent() {
                charts.go(parent.display().to_string());
            }
        }
        ui.add(
            egui::Label::new(RichText::new(&charts.at).small().weak())
                .truncate(),
        );
    });
    ui.add_space(4.0);

    // A folder of six hundred cells is a long list; it scrolls, and the rows
    // are full width so they can be hit with a thumb.
    let mut descend: Option<String> = None;
    egui::ScrollArea::vertical()
        .max_height(220.0)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if charts.entries.is_empty() {
                ui.label(RichText::new("no folders here").small().weak());
            }
            for name in &charts.entries {
                // Painted rather than added as a Button: a button centres its
                // label, and a centred list of folder names is unreadable.
                // The row is the full width and 26 points tall so it can be
                // hit with a thumb on the plotter's touchscreen.
                let (rect, resp) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 26.0),
                    egui::Sense::click(),
                );
                if ui.is_rect_visible(rect) {
                    if resp.hovered() {
                        ui.painter().rect_filled(
                            rect,
                            3.0,
                            ui.visuals().widgets.hovered.bg_fill,
                        );
                    }
                    ui.painter().text(
                        rect.left_center() + egui::vec2(8.0, 0.0),
                        Align2::LEFT_CENTER,
                        name,
                        egui::TextStyle::Body.resolve(ui.style()),
                        ui.visuals().text_color(),
                    );
                }
                if resp.clicked() {
                    descend = Some(here.join(name).display().to_string());
                }
            }
        });
    if let Some(dir) = descend {
        charts.go(dir);
    }

    ui.add_space(6.0);
    ui.separator();
    // What is actually in the folder on show, counted the way the catalogue
    // counts: it reads `.oesu` and nothing else, so this is exactly what
    // would load. Saying "no charts here" before the button is pressed saves
    // the twenty seconds it takes to find that out by decrypting.
    let ready = charts.cells > 0;
    ui.horizontal(|ui| {
        ui.label(match (charts.cells, charts.cells_partial) {
            (0, false) => RichText::new("no charts in this folder (o-charts .oesu or S-57 .000)").weak(),
            (0, true) => RichText::new("no charts near the top of this folder — open the one that holds them").weak(),
            (1, false) => RichText::new("1 cell here").strong(),
            (n, false) => RichText::new(format!("{n} cells here")).strong(),
            (n, true) => RichText::new(format!("{n}+ cells here")).strong(),
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add_enabled(ready && !charts.loading, egui::Button::new("Use this folder"))
                .on_hover_text("Decrypt and load these charts, and open them again next time")
                .clicked()
            {
                actions.push(UiAction::ChartFolderOpen {
                    path: charts.at.clone(),
                });
            }
            if charts.loading {
                ui.spinner();
            }
        });
    });
    if !charts.status.is_empty() {
        ui.label(RichText::new(&charts.status).small().weak());
    }
    if !charts.chosen.is_empty() && charts.chosen != charts.at {
        ui.label(
            RichText::new(format!("in use: {}", charts.chosen))
                .small()
                .weak(),
        );
    }
}

/// The chart shop: sign in, see what the account owns, see what is stale —
/// and, beside it, the charts already on this disk.
fn chart_shop(
    ctx: &Context,
    shop: &mut ShopView,
    charts: &mut crate::render::ui::ChartFolderView,
    free: &mut crate::render::ui::FreeChartsView,
    actions: &mut Vec<UiAction>,
) {
    use crate::render::ui::ChartsTab;
    let mut open = shop.open;
    Window::new("Charts")
        // No title-bar collapse: on a touchscreen it is an easy accidental
        // tap that leaves an empty title bar and no obvious way back.
        .collapsible(false)
        .open(&mut open)
        .default_size([620.0, 460.0])
        .default_pos([60.0, 80.0])
        // Kept inside the screen, and never repositioned by anything but a
        // drag: an area that egui re-places from an anchor each frame cannot
        // be moved by hand, and one that is free to leave the screen can be
        // dragged somewhere it cannot be dragged back from.
        // Inside the space the menu bar and instrument strip leave: a window
        // over the menu bar hides the very button that closes it.
        .constrain_to(ctx.available_rect())
        .collapsible(false)
        .show_fitted(ctx, |ui| {
            // Two ways to get a chart: buy one, or point at the folder the
            // last purchase was unpacked into. The second is the common case
            // and had no way in at all before — the charts could only be named
            // on the command line, which is no use on a plotter with no
            // keyboard.
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(charts.tab == ChartsTab::Shop, "My o-charts")
                    .on_hover_text("Download charts you bought at o-charts.org")
                    .clicked()
                {
                    charts.tab = ChartsTab::Shop;
                }
                if ui
                    .selectable_label(charts.tab == ChartsTab::Free, "Free charts")
                    .on_hover_text("Official US charts from NOAA, free to download")
                    .clicked()
                {
                    charts.tab = ChartsTab::Free;
                }
                if ui.selectable_label(charts.tab == ChartsTab::Folder, "Open a folder").clicked() {
                    charts.tab = ChartsTab::Folder;
                    if charts.at.is_empty() {
                        // Start where the charts already are, else where the
                        // user is: both beat starting at the root of the disk.
                        charts.at = if charts.chosen.is_empty() {
                            std::env::current_dir()
                                .map(|d| d.display().to_string())
                                .unwrap_or_else(|_| "/".into())
                        } else {
                            charts.chosen.clone()
                        };
                    }
                }
            });
            ui.separator();
            if charts.tab == ChartsTab::Free {
                free_charts(ui, free, actions);
                return;
            }
            if charts.tab == ChartsTab::Folder {
                chart_folder_browser(ui, charts, actions);
                return;
            }
            // Numbered to match o-charts' own instructions — sign in, identify
            // this system, install the chart — and walkable both ways: a step
            // already reached can be revisited without undoing what came after.
            let auto = if !shop.signed_in {
                1
            } else if shop.system_name.is_none() {
                2
            } else {
                3
            };
            let step = shop.step.filter(|s| (1..=auto).contains(s)).unwrap_or(auto);
            step_bar(ui, shop, step, auto);
            if shop.signed_in {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&shop.email).strong());
                    if let Some(name) = &shop.system_name {
                        ui.label(RichText::new(format!("on \"{name}\"")).weak().small());
                    } else {
                        ui.label(
                            RichText::new("this machine is not registered yet")
                                .weak()
                                .small(),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Not mid-download: signing out drops the session
                        // the download is running on.
                        if ui.add_enabled(!shop.busy, egui::Button::new("Sign out")).clicked() {
                            actions.push(UiAction::ShopSignOut);
                        }
                        if ui.add_enabled(!shop.busy, egui::Button::new("Refresh")).clicked() {
                            actions.push(UiAction::ShopRefresh);
                        }
                        if shop.busy {
                            ui.spinner();
                        }
                    });
                });
            }
            match step {
                1 if !shop.signed_in => {
                    ui.label(RichText::new("1. Sign in to o-charts").strong());
                    ui.label(
                        RichText::new("The same account you bought the charts with.")
                            .small()
                            .weak(),
                    );
                    ui.add_space(4.0);
                    ui.add_space(6.0);
                    let mut submitted = false;
                    egui::Grid::new("shop-login")
                        .num_columns(2)
                        .spacing([10.0, 8.0])
                        .show(ui, |ui| {
                            ui.label("Email");
                            ui.add(
                                egui::TextEdit::singleline(&mut shop.email)
                                    .hint_text("you@example.com")
                                    .desired_width(280.0),
                            );
                            ui.end_row();
                            ui.label("Password");
                            let pw = ui.add(
                                egui::TextEdit::singleline(&mut shop.password)
                                    .password(true)
                                    .desired_width(280.0),
                            );
                            // Enter in a text field presses its window's main
                            // button, here as in the planner and the boat search.
                            submitted = pw.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                            ui.end_row();
                            ui.label("");
                            ui.checkbox(&mut shop.remember, "Remember me on this device")
                                .on_hover_text(
                                    "Keeps o-charts' session key, not your password, so \
                                     Manx starts signed in. Sign out forgets it.",
                                );
                            ui.end_row();
                        });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let ready = !shop.email.trim().is_empty() && !shop.password.is_empty();
                        if (ui
                            .add_enabled(ready && !shop.busy, egui::Button::new("Sign in"))
                            .clicked()
                            || submitted)
                            && ready
                            && !shop.busy
                        {
                            actions.push(UiAction::ShopSignIn {
                                email: shop.email.trim().to_string(),
                                password: std::mem::take(&mut shop.password),
                                remember: shop.remember,
                            });
                        }
                        if shop.busy {
                            ui.spinner();
                        }
                    });
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "Your password is sent to o-charts over TLS to sign in, and is \
                             not stored on this computer.",
                        )
                        .small()
                        .weak(),
                    );
                }
                1 => {
                    ui.label(RichText::new("1. Sign in to o-charts").strong());
                    ui.label(format!("Signed in as {}.", shop.email));
                    ui.label(
                        RichText::new("Sign out above to use another account.")
                            .small()
                            .weak(),
                    );
                }
                2 => {
                    if let Some(name) = shop.system_name.clone() {
                        ui.label(
                            RichText::new(format!(
                                "This machine is registered as \"{name}\". Registering again \
                                 re-points a name; it does not free a slot."
                            ))
                            .small(),
                        );
                    }
                    ui.add_space(6.0);
                    ui.label(RichText::new("2. Identify this system").strong());
                    ui.label(
                        RichText::new(
                            "A chart licence is assigned to named computers: two or five \
                             of them, depending on the chart. Registering a name costs \
                             nothing — a slot is spent only when a chart set is assigned to \
                             it. One name is one real machine: a cloned system or a virtual \
                             machine sharing a slot breaks the licence.",
                        )
                        .small()
                        .weak(),
                    );
                    if cfg!(target_os = "android") {
                        ui.add_space(4.0);
                        android_licence_note(ui);
                    }
                    // Reusing the name this machine already carries is almost
                    // always what is wanted, and typing a fresh one is the
                    // expensive mistake: o-charts will not move or cancel an
                    // assignment once made, so a second name for one computer
                    // spends a second slot permanently.
                    if !shop.systems.is_empty() {
                        ui.label(
                            RichText::new("Already on this account — reuse one:")
                                .small()
                                .weak(),
                        );
                        let mut chosen = None;
                        ui.horizontal_wrapped(|ui| {
                            for name in &shop.systems {
                                let key = crate::shop::types::is_dongle_name(name);
                                let label = if key {
                                    format!("{name} (USB key)")
                                } else {
                                    name.clone()
                                };
                                let hint = if key {
                                    "A USB key: the licence follows the key between machines, \
                                     and only works while it is plugged in."
                                } else {
                                    "Reuse this name if it is what this computer was \
                                     registered as before."
                                };
                                if ui.button(label).on_hover_text(hint).clicked() {
                                    chosen = Some(name.clone());
                                }
                            }
                        });
                        if let Some(name) = chosen {
                            shop.new_system_name = name;
                        }
                    }
                    ui.horizontal(|ui| {
                        ui.label("Name:");
                        let field = ui.add(
                            egui::TextEdit::singleline(&mut shop.new_system_name)
                                .hint_text(if cfg!(target_os = "android") {
                                    "e.g. odroid-manx"
                                } else {
                                    "e.g. saloon-mac"
                                })
                                .desired_width(180.0),
                        );
                        let entered =
                            field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        let ready = !shop.new_system_name.trim().is_empty();
                        if (ui
                            .add_enabled(ready && !shop.busy, egui::Button::new("Register"))
                            .clicked()
                            || entered)
                            && ready
                            && !shop.busy
                        {
                            actions.push(UiAction::ShopRegister {
                                system_name: shop.new_system_name.trim().to_string(),
                            });
                        }
                    });
                    if shop
                        .systems
                        .iter()
                        .any(|s| s.eq_ignore_ascii_case(shop.new_system_name.trim()))
                    {
                        ui.label(
                            RichText::new(
                                "This name is already on the account, so registering it \
                                 re-points it at this machine rather than spending a new slot.",
                            )
                            .small()
                            .weak(),
                        );
                    } else if !shop.new_system_name.trim().is_empty() {
                        ui.label(
                            RichText::new(
                                "This is a new name. o-charts cannot move or cancel an \
                                 assignment once a chart is requested for it.",
                            )
                            .small()
                            .color(crate::render::theme::current().amber),
                        );
                    }
                }
                _ => {
                    ui.add_space(6.0);
                    ui.label(RichText::new("3. Install your charts").strong());
                    ui.separator();
                    chart_table(ui, shop, actions);
                    if let Some(pending) = shop.pending.as_mut() {
                        confirm_download(ui, pending, actions);
                    }
                }
            }
            step_nav(ui, shop, step, auto);
            // Signing in or registering moves the process on; the step shown
            // goes back to following it.
            if actions.iter().any(|a| {
                matches!(a, UiAction::ShopSignIn { .. } | UiAction::ShopRegister { .. })
            }) {
                shop.step = None;
            }

            if !shop.warning.is_empty() {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(&shop.warning)
                        .small()
                        .color(crate::render::theme::current().amber),
                );
            }
            if !shop.status.is_empty() {
                ui.add_space(6.0);
                ui.separator();
                ui.label(RichText::new(&shop.status).small());
            }
            // o-charts' conditions: the agreement can be had at any moment.
            ui.add_space(6.0);
            if ui.button("o-charts licence agreement (EULA)").clicked() {
                actions.push(UiAction::EulaRead);
            }
            // o-charts supports OpenCPN only, and sends Manx questions back
            // to us: said where the user would otherwise go looking for help.
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "o-charts licenses its charts for OpenCPN and programs derived from it, \
                     like Manx, but supports OpenCPN only. For help with o-charts in Manx, \
                     ask us, not o-charts: github.com/den200/ManxChartPlotter/issues",
                )
                .small()
                .weak(),
            );
        });
    shop.open = open;
}

/// On Android o-charts treats each app as its own system: said where the
/// user names this system and again before a slot is spent.
fn android_licence_note(ui: &mut egui::Ui) {
    let amber = crate::render::theme::current().amber;
    egui::Frame::group(ui.style()).stroke(egui::Stroke::new(1.0_f32, amber)).show(ui, |ui| {
        ui.label(RichText::new("On Android, each app needs its own licence").strong().color(amber));
        ui.label(
            "Charts installed in Manx use one of your o-charts slots and work in Manx only. \
             They can't be shared with OpenCPN or any other app on this device. Installing \
             the same charts in OpenCPN too uses a second slot.",
        );
    });
}

/// The three steps as a bar: where the user is, and which steps they may go
/// back to. A step not yet reached is shown but cannot be chosen.
fn step_bar(ui: &mut egui::Ui, shop: &mut ShopView, step: u8, reached: u8) {
    ui.horizontal(|ui| {
        for (n, name) in [(1, "Sign in"), (2, "This system"), (3, "Charts")] {
            if n > 1 {
                ui.label(RichText::new("›").weak());
            }
            let label = egui::SelectableLabel::new(step == n, format!("{n}. {name}"));
            if ui.add_enabled(n <= reached, label).clicked() {
                shop.step = Some(n);
            }
        }
    });
    ui.separator();
}

/// Back and Next under the step, for a touchscreen where the bar's labels
/// are small targets.
fn step_nav(ui: &mut egui::Ui, shop: &mut ShopView, step: u8, reached: u8) {
    if step == reached && step == 1 {
        return;
    }
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if step > 1 && ui.button("◀ Back").clicked() {
            shop.step = Some(step - 1);
        }
        if step < reached && ui.button("Next ▶").clicked() {
            shop.step = (step + 1 < reached).then_some(step + 1);
        }
    });
}

/// Put a download that spends a licence slot to the user before sending it.
fn confirm_download(
    ui: &mut egui::Ui,
    pending: &mut crate::render::ui::PendingDownload,
    actions: &mut Vec<UiAction>,
) {
    ui.add_space(8.0);
    egui::Frame::group(ui.style()).show(ui, |ui| {
        let title = if pending.new_slot {
            format!("{} — use a licence slot?", pending.chart_name)
        } else {
            format!("{} — install?", pending.chart_name)
        };
        ui.label(RichText::new(title).strong());
        // Spending a slot is said first and in colour, whatever else is
        // being asked: it is the one part of this that cannot be undone.
        if let Some(note) = &pending.slot_note {
            ui.label(RichText::new(note).color(crate::render::theme::current().amber));
        }
        // o-charts' terms: on Android each app holds its own licence, so a
        // slot spent here is Manx's alone. The user ticks that they know
        // before the slot can be spent, rather than finding out in OpenCPN.
        let per_app = cfg!(target_os = "android") && pending.new_slot;
        if per_app {
            ui.add_space(4.0);
            android_licence_note(ui);
            ui.checkbox(
                &mut pending.understood,
                "I understand these charts will work in Manx only",
            );
        }
        ui.label(RichText::new(&pending.because).small());
        // o-charts' conditions: the licence agreement is accepted before the
        // install, and the user understands what the charts are not. The
        // set's own copy of the agreement is shown in full, and accepted
        // again, before its charts are first opened.
        ui.add_space(4.0);
        ui.label(
            "Installing these charts means accepting o-charts' licence agreement (EULA). \
             It comes with the chart set and is shown in full before the charts are opened.",
        );
        ui.label(RichText::new(crate::shop::eula::NOT_OFFICIAL).strong());
        ui.horizontal_wrapped(|ui| {
            if ui.button("Read the agreement").clicked() {
                actions.push(UiAction::EulaRead);
            }
            ui.label(RichText::new(crate::shop::eula::URL).small().weak());
        });
        ui.checkbox(&mut pending.accepted, "I accept the agreement and understand this");
        let may_assign = (!per_app || pending.understood) && pending.accepted;
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            let go = if pending.new_slot { "Assign and download" } else { "Download" };
            if ui.add_enabled(may_assign, egui::Button::new(go)).clicked()
            {
                actions.push(UiAction::ShopDownload {
                    chart_id: pending.chart_id.clone(),
                });
            }
            if ui.button("Cancel").clicked() {
                actions.push(UiAction::ShopCancelDownload);
            }
        });
    });
}

fn chart_table(ui: &mut egui::Ui, shop: &ShopView, actions: &mut Vec<UiAction>) {
    if shop.charts.is_empty() {
        ui.label(if shop.busy {
            "Fetching your charts…"
        } else {
            "No charts on this account."
        });
        return;
    }
    ui.label(
        RichText::new(format!("{} chart set(s)", shop.charts.len()))
            .small()
            .weak(),
    );
    ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("shop-charts")
            .num_columns(5)
            .striped(true)
            .spacing([14.0, 6.0])
            .show(ui, |ui| {
                ui.label(RichText::new("Chart set").strong());
                ui.label(RichText::new("Edition").strong());
                ui.label(RichText::new("State").strong());
                ui.label(RichText::new("This machine").strong());
                ui.label("");
                ui.end_row();

                for c in &shop.charts {
                    let installed = shop.installed.get(&c.id).copied();
                    let target = choose_download(installed, c.edition);
                    ui.label(&c.name);
                    ui.label(RichText::new(c.edition.to_string()).monospace().small());
                    let state = if c.expired {
                        RichText::new("Expired").color(crate::render::theme::current().red)
                    } else {
                        RichText::new(target.label())
                    };
                    ui.label(state);
                    // What this machine's claim on the chart is. A slot is
                    // spent per machine, so the count has to be the shop's
                    // real one: saying five are free when three are invites
                    // the user to hand out slots they do not have.
                    let mine = shop.system_name.as_deref().and_then(|n| c.slot_for(n));
                    // Which other machines hold the rest — a USB key among
                    // them — is in the hover, not in place of the count: a
                    // key's name here read as the machine the download was for.
                    let cell = if mine.is_some() {
                        "assigned here".to_string()
                    } else {
                        format!("not here yet · {} of {} free", c.free_slots(), c.total_slots())
                    };
                    let holders = c.holders();
                    let cell = ui.label(RichText::new(cell).small().weak());
                    if !holders.is_empty() {
                        cell.on_hover_text(format!(
                            "{} of {} slot(s) in use: {}",
                            c.assigned_slots(),
                            c.total_slots(),
                            holders.join(", ")
                        ));
                    }
                    // Not for an expired licence: an install can only
                    // happen during the licensing period (o-charts'
                    // conditions). What is installed keeps working.
                    let can_ask = shop.system_name.is_some()
                        && !shop.busy
                        && !c.is_fully_assigned()
                        && !c.expired;
                    let button = ui
                        .add_enabled(can_ask, egui::Button::new("Download"))
                        .on_disabled_hover_text(if c.expired {
                            crate::shop::protocol::LICENCE_EXPIRED
                        } else if shop.system_name.is_none() {
                            "register this machine first"
                        } else if c.is_fully_assigned() {
                            "every slot on this licence is assigned to another machine"
                        } else {
                            "busy"
                        });
                    if button.clicked() {
                        // The first download to this machine spends a
                        // licence slot that can never be taken back, so it
                        // is put to the user first.
                        // And every install is preceded by the licence
                        // agreement, slot or no slot.
                        actions.push(UiAction::ShopConfirmDownload {
                            chart_id: c.id.clone(),
                        });
                    }
                    ui.end_row();
                    // The provenance of the licence, on its own row: what was
                    // bought, when it lapsed, and which edition this machine's
                    // slot actually holds.
                    let held = mine
                        .map(|(_, s)| s.last_requested.trim())
                        .filter(|v| !v.is_empty())
                        .map(str::to_string)
                        .or_else(|| installed.map(|e| e.to_string()));
                    let mut facts = Vec::new();
                    if !c.purchased.is_empty() {
                        facts.push(format!("bought {}", c.purchased));
                    }
                    if !c.expires.is_empty() {
                        facts.push(format!(
                            "{} {}",
                            if c.expired { "expired" } else { "renews" },
                            c.expires
                        ));
                    }
                    if let Some(ref held) = held {
                        facts.push(format!("your edition {held}"));
                    }
                    if !facts.is_empty() {
                        ui.label("");
                        ui.label("");
                        ui.label(RichText::new(facts.join(" · ")).small().weak());
                        ui.label("");
                        ui.label("");
                        ui.end_row();
                    }
                    if let Some(answer) = shop.grants.get(&c.id) {
                        ui.label("");
                        ui.label("");
                        ui.label(RichText::new(answer).small().weak());
                        ui.label("");
                        ui.label("");
                        ui.end_row();
                    }
                }
            });
    });
}

/// The object-query bubble.
///
/// Anchored near the tap but free to be dragged, because on a small screen the
/// thing you just tapped is exactly what the panel covers.
/// Does this object describe the cell rather than the water?
///
/// The S-57 meta classes, `M_` and `C_`. Kept as one predicate so the bubble
/// and [`crate::pick::sort_picks`] agree about what counts.
fn is_cell_description(acronym: &str) -> bool {
    acronym.starts_with("M_") || acronym.starts_with("C_")
}

fn object_query(
    ctx: &Context,
    objects: &[PickedObject],
    anchor: Option<[f32; 2]>,
    pick_id: u64,
    actions: &mut Vec<UiAction>,
) {
    let mut open = true;
    let screen = ctx.screen_rect();
    // Beside the tap, and never more than half the screen, so the chart stays
    // visible while the panel is up.
    let max_h = (screen.height() * 0.6).max(160.0);
    // The 260 floor is for readability, but never wider than the window
    // itself: a narrow window got a bubble running off its edge.
    let max_w = (screen.width() * 0.45).clamp(260.0, 460.0).min(screen.width() - 16.0);

    // `default_pos` only applies to a window egui has not seen before; one
    // id per query is what lets each bubble open beside its own tap.
    let mut window = Window::new("Chart object")
        .id(egui::Id::new(("chart-object", pick_id)))
        .constrain_to(ctx.available_rect())
        .open(&mut open)
        .resizable(true)
        .collapsible(false)
        .max_width(max_w)
        .max_height(max_h);
    window = match anchor {
        Some([x, y]) => window.default_pos(egui::pos2(
            (x + 16.0).min(screen.right() - max_w - 8.0).max(8.0),
            (y + 16.0).min(screen.bottom() - 160.0).max(8.0),
        )),
        None => window.anchor(Align2::RIGHT_TOP, [-8.0, 8.0]),
    };

    window.show_fitted(ctx, |ui| {
        if objects.is_empty() {
            ui.label("Nothing charted here.");
            return;
        }
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "{} object{} here",
                    objects.len(),
                    if objects.len() == 1 { "" } else { "s" }
                ))
                .small()
                .weak(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("Copy all")
                    .on_hover_text("Copy every object listed here")
                    .clicked()
                {
                    let all: String = objects.iter().map(as_text).collect::<Vec<_>>().join("\n");
                    ui.ctx().copy_text(all);
                }
            });
        });
        ui.separator();

        // Scrolls, so nothing is truncated: a tap in a harbour can find forty
        // objects and every one of them is now reachable.
        ScrollArea::vertical().show(ui, |ui| {
            // The meta objects describe the *cell* — which publication it came
            // from, which buoyage system is in force, how good the survey was.
            // A tap in a harbour finds all of them and they are never the
            // answer to "what is that?", so they go behind one line instead of
            // pushing the thing you actually tapped off the screen.
            let (cell, real): (Vec<_>, Vec<_>) = objects
                .iter()
                .enumerate()
                .partition(|(_, o)| is_cell_description(&o.acronym));

            for (n, (i, o)) in real.iter().enumerate() {
                if n > 0 {
                    ui.add_space(4.0);
                    ui.separator();
                }
                object(ui, o, *i);
            }

            if !cell.is_empty() {
                ui.add_space(6.0);
                egui::CollapsingHeader::new(
                    RichText::new(format!("About this chart cell ({})", cell.len()))
                        .small()
                        .weak(),
                )
                .id_salt("pick-cell-meta")
                .default_open(false)
                .show(ui, |ui| {
                    for (n, (i, o)) in cell.iter().enumerate() {
                        if n > 0 {
                            ui.add_space(4.0);
                            ui.separator();
                        }
                        object(ui, o, *i);
                    }
                });
            }
        });
    });

    if !open {
        actions.push(UiAction::DismissPick);
    }
}

fn object(ui: &mut egui::Ui, o: &PickedObject, index: usize) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(&o.title).strong());
        ui.label(RichText::new(format!("({})", o.acronym)).weak().small());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Selecting text with a fingertip on a wet screen is not a thing
            // anyone does successfully. One button copies the whole object —
            // class, chart, every attribute and any chart note — which is what
            // you want when reporting a chart error or asking someone ashore.
            if ui
                .button("Copy")
                .on_hover_text("Copy this object's details")
                .clicked()
            {
                ui.ctx().copy_text(as_text(o));
            }
        });
    });
    // Provenance, but only as much of it as anyone reads. The cell's name is
    // what you quote when reporting a chart error and nothing else, so it goes
    // to the hover; fifteen objects each carrying "• OC-45-HBEOK5" was a
    // column of noise down the middle of the answer.
    let mut line = format!("{} \u{2022} 1:{}", geometry_name(o.geometry), o.chart_scale);
    if o.duplicates > 0 {
        // Said plainly rather than hidden: the chart set really does carry
        // this object more than once, and a reader comparing against another
        // plotter should know the answer was folded.
        line.push_str(&format!(" \u{2022} +{} identical", o.duplicates));
    }
    ui.label(RichText::new(line).small().weak())
        .on_hover_text(&o.chart);

    // The reading, where S-52 composes one. `Fl(1)G 3s 4M, 165°–305°` is what
    // a light means; the eight attributes behind it are how the chart stores
    // it, and they go behind a disclosure so they are there without being in
    // the way.
    if let Some(summary) = &o.summary {
        ui.label(RichText::new(summary).strong());
    }

    let attrs = |ui: &mut egui::Ui| {
        // Keyed by position, not by content: a tap routinely finds two lights
        // of the same class in the same cell, and two grids with one id is an
        // egui collision — which is what those red boxes were.
        egui::Grid::new(("pick-attrs", index))
            .num_columns(2)
            .spacing([12.0, 2.0])
            .show(ui, |ui| {
                for (k, v) in o.attributes.iter().filter(|(k, _)| crate::pick::is_worth_showing(k)) {
                    ui.label(RichText::new(k).monospace().weak());
                    // Labels in a grid do not wrap by default: one long value
                    // (a SORIND, an INFORM sentence) widened the whole bubble
                    // past its max_width and off the window.
                    ui.add(egui::Label::new(v).wrap());
                    ui.end_row();
                }
            });
    };

    if !o.attributes.is_empty() {
        match &o.summary {
            Some(_) => {
                egui::CollapsingHeader::new(RichText::new("details").small().weak())
                    .id_salt(("pick-attrs-fold", index))
                    .default_open(false)
                    .show(ui, attrs);
            }
            None => attrs(ui),
        }
    }

    for note in &o.notes {
        ui.add_space(2.0);
        // The chart's own caution text, as printed in the margin of the paper
        // chart. Worth its own framing: it is regulation, not metadata.
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(RichText::new(note.trim()).small());
        });
    }
}

/// One object as plain text, for the clipboard.
fn as_text(o: &PickedObject) -> String {
    let mut out = format!("{} ({})\n", o.title, o.acronym);
    out.push_str(&format!(
        "{} · 1:{} · {}\n",
        geometry_name(o.geometry),
        o.chart_scale,
        o.chart
    ));
    // The copied text keeps everything, summary included: it is what gets
    // pasted into a message to a chart producer, and there the raw attributes
    // are the evidence.
    if let Some(summary) = &o.summary {
        out.push_str(summary);
        out.push('\n');
    }
    for (k, v) in o.attributes.iter().filter(|(k, _)| crate::pick::is_worth_showing(k)) {
        out.push_str(&format!("  {k}  {v}\n"));
    }
    for note in &o.notes {
        out.push('\n');
        out.push_str(note.trim());
        out.push('\n');
    }
    out
}

fn geometry_name(kind: FeatureType) -> &'static str {
    match kind {
        FeatureType::Point => "point",
        FeatureType::Line => "line",
        FeatureType::Area => "area",
        FeatureType::Multipoint => "soundings",
    }
}

#[cfg(test)]
mod pick_card_tests {
    use super::{headline, range_text};
    use crate::pick::PickedObject;

    fn object(acronym: &str, title: &str, attrs: &[(&str, &str)], summary: Option<&str>) -> PickedObject {
        PickedObject {
            acronym: acronym.into(),
            title: title.into(),
            chart: "X".into(),
            chart_scale: 12_000,
            geometry: crate::senc::FeatureType::Point,
            attributes: attrs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            notes: Vec::new(),
            distance_m: 0.0,
            summary: summary.map(str::to_string),
            duplicates: 0,
        }
    }

    /// The card says what a thing is in a sailor's words: a contour by its
    /// depth, a light by its name and character — not by attribute codes.
    /// Kalvebodbroen as the Danish cell charts it: CATBRG 1 (fixed),
    /// VERCLR 16, HORCLR 35. And an opening bridge by S-57's own
    /// attributes: VERCCL is the clearance closed, VERCOP open.
    #[test]
    fn a_bridge_says_whether_it_opens_and_how_much_air_there_is() {
        let fixed = object(
            "BRIDGE",
            "Bridge",
            &[("CATBRG", "fixed bridge"), ("HORCLR", "35"), ("VERCLR", "16")],
            None,
        );
        assert_eq!(headline(&fixed), "Fixed bridge · clearance 16 m · 35 m wide");
        let bascule = object(
            "BRIDGE",
            "Bridge",
            &[("CATBRG", "bascule bridge"), ("OBJNAM", "Knippelsbro"), ("VERCCL", "5.4")],
            None,
        );
        assert_eq!(
            headline(&bascule),
            "Knippelsbro · Bascule bridge · clearance 5.4 m closed, no limit charted when open"
        );
        let lifting = object(
            "BRIDGE",
            "Bridge",
            &[("CATBRG", "lifting bridge"), ("VERCCL", "3"), ("VERCOP", "42.5")],
            None,
        );
        assert_eq!(headline(&lifting), "Lifting bridge · clearance 3 m closed, 42.5 m open");
        let bare = object("BRIDGE", "Bridge", &[], None);
        assert_eq!(headline(&bare), "Bridge · clearance not charted");
        // A power cable: the safe clearance is the one to sail by.
        let cable = object("CBLOHD", "Cable, overhead", &[("VERCLR", "30"), ("VERCSA", "26")], None);
        assert_eq!(headline(&cable), "Overhead cable or pipe · safe clearance 26 m");
    }

    #[test]
    fn the_card_names_things_in_plain_words() {
        let contour = object("DEPCNT", "Depth contour", &[("VALDCO", "6")], None);
        assert_eq!(headline(&contour), "Depth contour 6 m");
        let area = object("DEPARE", "Depth area", &[("DRVAL1", "5"), ("DRVAL2", "10")], None);
        assert_eq!(headline(&area), "Depth area 5–10 m");
        let light = object("LIGHTS", "Light", &[("OBJNAM", "Trekroner")], Some("Iso 2s 20m"));
        assert_eq!(headline(&light), "Trekroner · Light · Iso 2s 20m");
        let buoy = object("BOYLAT", "Buoy, lateral", &[("OBJNAM", "  ")], None);
        assert_eq!(headline(&buoy), "Buoy, lateral", "a blank name is no name");
    }

    /// A cable is 185 m: nearer than that reads in metres, beyond in miles.
    #[test]
    fn range_reads_in_metres_close_to_and_miles_beyond() {
        assert_eq!(range_text(120.0, 47.4), "120 m · 047°T from the boat");
        assert_eq!(range_text(2222.4, 359.6), "1.20 NM · 000°T from the boat");
        assert_eq!(range_text(46_300.0, 180.0), "25.0 NM · 180°T from the boat");
    }
}

#[cfg(test)]
mod scale_bar_tests {
    use super::scale_bar_length;

    /// The bar is the longest round distance that fits in 120 points.
    /// A nautical mile is 1852 m.
    #[test]
    fn the_scale_bar_is_the_longest_round_distance_that_fits() {
        let at = |m_per_pt: f32| scale_bar_length(m_per_pt, 120.0).map(|(pt, l)| ((pt * 10.0).round() / 10.0, l));
        // 10 m a point: 1200 m of room. Half a mile (926 m) fits, a mile does not.
        assert_eq!(at(10.0), Some((92.6, "0.5 NM".into())));
        // 1 m a point: 120 m of room. 100 m fits; a cable (185 m) does not.
        assert_eq!(at(1.0), Some((100.0, "100 m".into())));
        // Zoomed right in, 5 cm a point: 6 m of room.
        assert_eq!(at(0.05), Some((100.0, "5 m".into())));
        // 100 m a point: 12 km of room. 5 NM (9260 m) fits, 10 NM does not.
        assert_eq!(at(100.0), Some((92.6, "5 NM".into())));
        // 2 m a point: a cable is 92.6 points.
        assert_eq!(at(2.0), Some((92.6, "0.1 NM".into())));
    }

    #[test]
    fn no_scale_no_bar() {
        assert_eq!(scale_bar_length(0.0, 120.0), None);
        assert_eq!(scale_bar_length(f32::NAN, 120.0), None);
        assert_eq!(scale_bar_length(-1.0, 120.0), None);
        // Zoomed out so far that not even 500 NM fits... the largest is
        // still the answer while anything fits, and nothing when none does.
        assert_eq!(scale_bar_length(1.0e9, 120.0), None);
    }
}

#[cfg(test)]
mod compass_tests {
    use super::north_on_screen;
    use crate::render::Camera;

    #[test]
    fn the_needle_points_up_when_north_up() {
        let n = north_on_screen(0.0);
        assert!(n.x.abs() < 1e-6 && (n.y + 1.0).abs() < 1e-6, "{n:?}");
    }

    /// Head-up at any heading: the needle points exactly where the chart
    /// draws north — at a point due north of the middle of the screen.
    #[test]
    fn the_needle_follows_north_as_the_chart_turns() {
        for heading_deg in [0.0f64, 30.0, 90.0, 135.0, 180.0, 270.0, 359.0] {
            let mut cam = Camera::new(1_400_000.0, 7_500_000.0, 2.0, 800.0, 600.0);
            cam.rotation = heading_deg.to_radians();
            let middle = cam.world_to_screen(cam.position.x, cam.position.y);
            let ahead = cam.world_to_screen(cam.position.x, cam.position.y + 100.0);
            let drawn = (ahead - middle).normalize();
            let needle = north_on_screen(cam.rotation as f32);
            assert!(
                (drawn.x - needle.x).abs() < 1e-3 && (drawn.y - needle.y).abs() < 1e-3,
                "heading {heading_deg}: chart north {drawn:?}, needle {needle:?}"
            );
        }
    }
}
