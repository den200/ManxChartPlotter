//! The instrument strip, and the window that configures it.
//!
//! Design intent, for a screen read at arm's length on a moving boat:
//!
//! - **The number is the thing.** Large, tabular, high contrast. The label
//!   above it is small and quiet, because you learn where a reading sits and
//!   stop reading its name after the first day.
//! - **Nothing moves that need not.** Digits are laid out to a fixed width and
//!   headings are zero-padded, so a value changing does not shift its
//!   neighbours. A bar that twitches is a bar you cannot read at a glance.
//! - **Stale is louder than absent.** A number that stopped updating still
//!   looks like a number. Anything older than a few seconds is dimmed and
//!   struck through; anything never received shows an em dash.
//! - **Touch first.** Tiles are large enough to hit while holding on with the
//!   other hand, with no target smaller than a fingertip.
//! - **Pages, swiped.** The bar holds several pages of tiles — the boat's
//!   instruments on one, the route being followed on the next — and a swipe
//!   across it turns the page, as on a phone.

use egui::{Align2, Color32, Context, FontId, RichText, Sense, Stroke, Vec2};

use super::ui::{BarPosition, InstrumentPage, InstrumentView, UiAction};
use crate::nav::follow::Guidance;
use crate::signalk::{catalog, state::Vessel};

/// Height of the strip in points. Enough for a label and a large number, and
/// enough to hit.
const BAR_HEIGHT: f32 = 76.0;
const TILE_WIDTH: f32 = 108.0;
const VALUE_SIZE: f32 = 30.0;
const LABEL_SIZE: f32 = 11.0;

/// Width of the strip when it runs down a side: a tile's width and a margin.
const SIDE_WIDTH: f32 = 132.0;

/// How far a swipe must travel, in points, to turn the page.
const SWIPE: f32 = 50.0;

/// What a tile reads from: the boat, and the route when one is followed.
#[derive(Clone, Copy)]
pub struct Sources<'a> {
    pub vessel: &'a Vessel,
    pub route: Option<&'a Guidance>,
}

/// Draw the strip. Returns nothing; interaction arrives as actions.
pub fn bar(ctx: &Context, view: &mut InstrumentView, src: Sources, actions: &mut Vec<UiAction>) {
    // Read the edge before the closure borrows `view`, so the panel choice is
    // not itself part of the borrow.
    let position = view.position;
    // An empty page still shows, when there are others to swipe to — or the
    // bar would vanish and take the way back with it.
    if position == BarPosition::Hidden || (view.tiles().is_empty() && view.pages.len() < 2) {
        return;
    }

    match position {
        BarPosition::Top => {
            egui::TopBottomPanel::top("instrument-bar")
                .exact_height(BAR_HEIGHT)
                .show_separator_line(true)
                .show(ctx, |ui| strip(ui, view, src, false, actions));
        }
        BarPosition::Bottom => {
            egui::TopBottomPanel::bottom("instrument-bar")
                .exact_height(BAR_HEIGHT)
                .show_separator_line(true)
                .show(ctx, |ui| strip(ui, view, src, false, actions));
        }
        BarPosition::Left => {
            egui::SidePanel::left("instrument-bar")
                .exact_width(SIDE_WIDTH)
                .resizable(false)
                .show_separator_line(true)
                .show(ctx, |ui| strip(ui, view, src, true, actions));
        }
        BarPosition::Right => {
            egui::SidePanel::right("instrument-bar")
                .exact_width(SIDE_WIDTH)
                .resizable(false)
                .show_separator_line(true)
                .show(ctx, |ui| strip(ui, view, src, true, actions));
        }
        BarPosition::Hidden => {}
    }
}

/// The strip's contents: across the screen, or stacked down a side.
fn strip(
    ui: &mut egui::Ui,
    view: &mut InstrumentView,
    src: Sources,
    down: bool,
    actions: &mut Vec<UiAction>,
) {
    swipe(ui, view, actions);
    if down {
        // Stacked, and scrolled if there are more than the screen is tall.
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(6.0);
                let size = Vec2::new(SIDE_WIDTH - 12.0, BAR_HEIGHT - 12.0);
                tiles(ui, view, src, size, size.x);
                status_dot(ui, view);
                page_dots(ui, view, actions);
            });
        });
        return;
    }
    ui.horizontal_centered(|ui| {
        ui.add_space(6.0);
        let size = Vec2::new(TILE_WIDTH, BAR_HEIGHT - 12.0);
        tiles(ui, view, src, size, size.y);
        // The connection's health belongs on the bar, not buried in a
        // window: a stale reading and a dropped link look identical
        // otherwise.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(8.0);
            status_dot(ui, view);
            page_dots(ui, view, actions);
        });
    });
}

/// Every tile on the bar, each `size` — the wind rose excepted, which is a
/// square `rose` wide: as tall as the bar across the screen, as wide as the
/// strip down a side.
fn tiles(ui: &mut egui::Ui, view: &mut InstrumentView, src: Sources, size: Vec2, rose: f32) {
    for path in view.tiles().to_vec() {
        let response = if path == catalog::WIND_ROSE {
            wind_rose(ui, src.vessel, view, rose)
        } else if catalog::is_route_tile(&path) {
            route_tile(ui, &path, src.route, view, size)
        } else {
            tile(ui, &path, src.vessel, view, size)
        };
        if response.clicked() {
            // A tile is the shortest route to the thing that
            // configures it — no hunting through a menu.
            view.open = true;
        }
    }
}

/// Turn the page on a swipe across the bar.
///
/// Read from the raw pointer rather than from a widget's drag, because the
/// strip down a side is a scroll area that claims drags for itself. The
/// press is remembered here since egui forgets where it began on the frame
/// it ends. A swipe travels far enough that the tile under it does not also
/// count it as a tap.
fn swipe(ui: &mut egui::Ui, view: &mut InstrumentView, actions: &mut Vec<UiAction>) {
    let rect = ui.max_rect();
    let id = ui.id().with("swipe");
    let (origin, pos, down, released) = ui.input(|i| {
        (
            i.pointer.press_origin(),
            i.pointer.latest_pos(),
            i.pointer.primary_down(),
            i.pointer.primary_released(),
        )
    });
    if down {
        if let (Some(origin), Some(pos)) = (origin, pos) {
            if rect.contains(origin) {
                ui.data_mut(|d| d.insert_temp(id, (origin, pos)));
            }
        }
        return;
    }
    let Some((origin, end)) = ui.data_mut(|d| d.remove_temp::<(egui::Pos2, egui::Pos2)>(id)) else {
        return;
    };
    if !released {
        return;
    }
    let travel = end - origin;
    if travel.x.abs() < SWIPE || travel.x.abs() < 1.5 * travel.y.abs() {
        return;
    }
    let n = view.pages.len();
    if n < 2 {
        return;
    }
    // Swiping left brings the next page in from the right, and round.
    view.page = if travel.x < 0.0 { (view.page + 1) % n } else { (view.page + n - 1) % n };
    actions.push(UiAction::SettingsChanged);
}

/// Which page is showing, as a row of dots; tapping one goes to it. Absent
/// with a single page, where there is nothing to say.
fn page_dots(ui: &mut egui::Ui, view: &mut InstrumentView, actions: &mut Vec<UiAction>) {
    let n = view.pages.len();
    if n < 2 {
        return;
    }
    let dim = ui.visuals().weak_text_color();
    let strong = ui.visuals().strong_text_color();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(18.0 * n as f32, 28.0), Sense::hover());
    for i in 0..n {
        let cell = egui::Rect::from_min_size(
            rect.min + Vec2::new(18.0 * i as f32, 0.0),
            Vec2::new(18.0, 28.0),
        );
        let response = ui
            .interact(cell, ui.id().with(("page-dot", i)), Sense::click())
            .on_hover_text(&view.pages[i].name);
        let (radius, colour) = if i == view.page { (4.5, strong) } else { (3.0, dim) };
        ui.painter().circle_filled(cell.center(), radius, colour);
        if response.clicked() && i != view.page {
            view.page = i;
            actions.push(UiAction::SettingsChanged);
        }
    }
}

/// The link's health, as a dot. Orange is a warning — a link that should be
/// up and is not. With no server asked for, there is nothing to warn of.
fn status_dot(ui: &mut egui::Ui, view: &mut InstrumentView) {
    let colour = if view.connected {
        crate::render::theme::current().green
    } else if view.active {
        crate::render::theme::current().amber
    } else {
        ui.visuals().weak_text_color()
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
    ui.painter().circle_filled(rect.center(), 5.0, colour);
    if response.on_hover_text(view.status.clone()).clicked() {
        view.open = true;
    }
}

/// One reading.
fn tile(
    ui: &mut egui::Ui,
    path: &str,
    vessel: &Vessel,
    view: &InstrumentView,
    size: Vec2,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let painter = ui.painter();
    let dim = ui.visuals().weak_text_color();
    let strong = ui.visuals().strong_text_color();

    let reading = vessel.get(path);
    let stale = reading.map(|r| r.is_stale()).unwrap_or(false);

    // Label, quiet and above.
    painter.text(
        rect.center_top() + Vec2::new(0.0, 8.0),
        Align2::CENTER_TOP,
        catalog::label_for(path).to_uppercase(),
        FontId::proportional(LABEL_SIZE),
        dim,
    );

    // Value, large and centred.
    let (text, unit, colour) = match reading.and_then(|r| r.number()) {
        Some(n) => {
            let r = catalog::quantity_for(path).format(n, &view.units);
            let colour = if stale {
                ui.visuals().warn_fg_color
            } else {
                strong
            };
            (r.value, r.unit, colour)
        }
        None => match reading.map(|r| &r.value) {
            // A text reading — a GNSS fix type, an autopilot mode — still
            // belongs on the bar if someone put it there.
            Some(crate::signalk::delta::Value::Text(t)) => (t.clone(), "", strong),
            // Received, but explicitly empty.
            Some(_) => ("—".into(), "", dim),
            // Never heard of. Not the same thing, and it must not look the same.
            None => ("–".into(), "", dim),
        },
    };

    let value_pos = rect.center() + Vec2::new(0.0, 6.0);
    let galley = painter.layout_no_wrap(
        text.clone(),
        // Monospace so digits are the same width: 11.1 and 88.8 must occupy
        // exactly the same space or the bar shivers as the boat moves.
        FontId::monospace(VALUE_SIZE),
        colour,
    );
    let text_pos = value_pos - Vec2::new(galley.size().x / 2.0, galley.size().y / 2.0);
    painter.galley(text_pos, galley.clone(), colour);

    if !unit.is_empty() {
        painter.text(
            text_pos + Vec2::new(galley.size().x + 3.0, galley.size().y - 8.0),
            Align2::LEFT_BOTTOM,
            unit,
            FontId::proportional(LABEL_SIZE + 1.0),
            dim,
        );
    }

    // A stale reading is struck through. Dimming alone is too easy to miss in
    // sunlight, and this is the difference between a depth and a memory of one.
    if stale {
        let y = value_pos.y;
        painter.line_segment(
            [
                egui::pos2(text_pos.x - 2.0, y),
                egui::pos2(text_pos.x + galley.size().x + 2.0, y),
            ],
            Stroke::new(1.5_f32, ui.visuals().warn_fg_color),
        );
        if let Some(r) = reading {
            return response.on_hover_text(format!(
                "{path}\nLast heard {:.0}s ago",
                r.age().as_secs_f32()
            ));
        }
    }

    response.on_hover_text(path)
}

/// A tile fed by the route being followed: distance, time or arrival, to
/// the next waypoint or to the end. Drawn as a Signal K tile is, so the bar
/// reads as one instrument; with no route followed it shows the dash of a
/// reading never received.
fn route_tile(
    ui: &mut egui::Ui,
    id: &str,
    route: Option<&Guidance>,
    view: &InstrumentView,
    size: Vec2,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let painter = ui.painter();
    let dim = ui.visuals().weak_text_color();
    let strong = ui.visuals().strong_text_color();

    painter.text(
        rect.center_top() + Vec2::new(0.0, 8.0),
        Align2::CENTER_TOP,
        catalog::label_for(id).to_uppercase(),
        FontId::proportional(LABEL_SIZE),
        dim,
    );

    let now = chrono::Local::now();
    let (text, unit, colour) = match route.and_then(|g| route_reading(id, g, &view.units, now)) {
        Some((value, unit)) => (value, unit, strong),
        None => ("–".to_string(), String::new(), dim),
    };

    let value_pos = rect.center() + Vec2::new(0.0, 6.0);
    let galley = painter.layout_no_wrap(text, FontId::monospace(VALUE_SIZE), colour);
    let text_pos = value_pos - galley.size() / 2.0;
    painter.galley(text_pos, galley.clone(), colour);
    if !unit.is_empty() {
        painter.text(
            text_pos + Vec2::new(galley.size().x + 3.0, galley.size().y - 8.0),
            Align2::LEFT_BOTTOM,
            unit,
            FontId::proportional(LABEL_SIZE + 1.0),
            dim,
        );
    }

    let about = catalog::ROUTE_TILES
        .iter()
        .find(|t| t.0 == id)
        .map_or("", |t| t.2);
    response.on_hover_text(match route {
        Some(g) => format!("{about}\nTo {}", g.to_name),
        None => format!("{about}\nNo route is being followed"),
    })
}

/// Below this closing speed, in knots, a time to go is not worth showing:
/// drifting, or heading away, it would promise an arrival that is not coming.
const MIN_CLOSING_KT: f64 = 0.2;

/// A route tile's value and unit, or `None` when there is nothing honest to
/// show — no speed to reckon a time from, or a route already finished.
fn route_reading(
    id: &str,
    g: &Guidance,
    units: &crate::signalk::UnitPrefs,
    now: chrono::DateTime<chrono::Local>,
) -> Option<(String, String)> {
    if g.finished {
        return None;
    }
    let distance = |nm: f64| {
        let r = crate::signalk::Quantity::Distance.format(nm * crate::geo::METRES_PER_NM, units);
        (r.value, r.unit.to_string())
    };
    // Time to the waypoint from the speed made good toward it.
    let hours = |nm: f64, kt: Option<f64>| kt.filter(|&k| k > MIN_CLOSING_KT).map(|k| nm / k);
    let until = |t: chrono::DateTime<chrono::Utc>| (t - now.to_utc()).num_seconds() as f64 / 3600.0;
    let wp_hours = hours(g.dtw_nm, g.vmg_kt);
    // Late (positive) or early against a weather route's plan: the arrival
    // at this mark as it is going now, against the one planned.
    let delta = wp_hours.zip(g.wp_plan_eta).map(|(h, plan)| h - until(plan));
    // To the end: on a weather route, the planned arrival moved by that
    // delta — the plan knows the wind ahead, SOG only the wind here.
    // Otherwise at SOG, since the legs after this one point elsewhere.
    let dest_hours = match g.dest_plan_eta {
        Some(plan) => delta.map(|d| (until(plan) + d).max(0.0)),
        None => hours(g.dtg_nm, g.sog_kt),
    };
    match id {
        "manx.route.wpDistance" => Some(distance(g.dtw_nm)),
        "manx.route.destDistance" => Some(distance(g.dtg_nm)),
        "manx.route.wpTime" => wp_hours.map(time_to_go),
        "manx.route.destTime" => dest_hours.map(time_to_go),
        "manx.route.wpEta" => wp_hours.map(|h| eta(now, h)),
        "manx.route.destEta" => dest_hours.map(|h| eta(now, h)),
        "manx.route.planDelta" => delta.map(against_plan),
        _ => None,
    }
}

/// How far off the plan, `+0:25 late` or `-0:10 early`, to the minute.
fn against_plan(hours: f64) -> (String, String) {
    let minutes = (hours * 60.0).round();
    if minutes == 0.0 {
        return ("0:00".into(), "on plan".into());
    }
    let (value, _) = time_to_go(hours.abs());
    if minutes > 0.0 {
        (format!("+{value}"), "late".into())
    } else {
        (format!("-{value}"), "early".into())
    }
}

/// Hours and minutes, `7:05`; past a hundred hours, whole days, which is all
/// a figure that far out is good for.
pub(crate) fn time_to_go(hours: f64) -> (String, String) {
    let minutes = (hours * 60.0).round() as u64;
    if minutes < 100 * 60 {
        (format!("{}:{:02}", minutes / 60, minutes % 60), String::new())
    } else {
        (format!("{:.0}", hours / 24.0), "d".to_string())
    }
}

/// The clock time of arrival, local like every other clock on screen, and
/// how many days on when it is not today.
fn eta(now: chrono::DateTime<chrono::Local>, hours: f64) -> (String, String) {
    // Capped at a year: a crawl across an ocean must not overflow the clock.
    let seconds = (hours * 3600.0).min(365.0 * 86_400.0) as i64;
    let at = now + chrono::Duration::seconds(seconds);
    let days = (at.date_naive() - now.date_naive()).num_days();
    let unit = if days > 0 { format!("+{days}d") } else { String::new() };
    (at.format("%H:%M").to_string(), unit)
}

/// The wind on a dial, bow up: where the apparent and the true wind come
/// from, and how hard each blows.
///
/// Apparent wind (AW) is drawn in the text colour and true wind (TW) in
/// blue, each needle labelled beside its head, the
/// apparent speed large in the middle and the true speed under it in blue.
/// The red and green arcs are the close-hauled sectors, 20° to 60° either
/// side of the bow, so a glance says which tack and how high. A stale needle
/// or speed goes to the warning colour, as a stale number does.
fn wind_rose(ui: &mut egui::Ui, vessel: &Vessel, view: &InstrumentView, side: f32) -> egui::Response {
    const AWA: &str = "environment.wind.angleApparent";
    const AWS: &str = "environment.wind.speedApparent";
    const TWS: &str = "environment.wind.speedTrue";
    // True wind angle relative to the bow; over the ground if that is all
    // the boat sends.
    let twa = ["environment.wind.angleTrueWater", "environment.wind.angleTrueGround"]
        .into_iter()
        .find(|p| vessel.get(p).is_some())
        .unwrap_or("environment.wind.angleTrueWater");

    let (rect, response) = ui.allocate_exact_size(Vec2::splat(side), Sense::click());
    let painter = ui.painter();
    let dim = ui.visuals().weak_text_color();
    let strong = ui.visuals().strong_text_color();
    let warn = ui.visuals().warn_fg_color;
    let blue = crate::render::theme::current().blue;
    let c = rect.center();
    let r = side * 0.5 - 3.0;
    // Angles run clockwise from the bow, starboard positive, as Signal K's do.
    let at = |angle: f32, radius: f32| c + radius * Vec2::new(angle.sin(), -angle.cos());

    painter.circle_stroke(c, r, Stroke::new(1.0_f32, dim));
    for i in 0..12 {
        let a = (i as f32 * 30.0).to_radians();
        let (inner, width) = if i == 0 { (0.75, 2.0_f32) } else { (0.88, 1.0) };
        painter.line_segment([at(a, r * inner), at(a, r)], Stroke::new(width, dim));
    }
    let arc = |from: f32, to: f32, colour: Color32| {
        let points = (0..=12)
            .map(|i| at((from + (to - from) * i as f32 / 12.0).to_radians(), r - 2.5))
            .collect();
        painter.add(egui::Shape::line(points, Stroke::new(4.0_f32, colour)));
    };
    // Port red, starboard green, as the sidelights are.
    arc(-60.0, -20.0, crate::render::theme::current().red);
    arc(20.0, 60.0, crate::render::theme::current().green);

    // A needle points to where the wind comes from, clear of the speeds in
    // the middle, and is named beside its head: two arrows on one dial are
    // otherwise told apart only by colour, which sunlight washes out.
    let needle = |path: &str, name: &str, colour: Color32, width: f32| {
        let Some(reading) = vessel.get(path) else { return };
        let Some(a) = reading.number() else { return };
        let colour = if reading.is_stale() { warn } else { colour };
        let a = a as f32;
        let tip = at(a, r - 1.0);
        painter.line_segment([at(a, r * 0.5), tip], Stroke::new(width, colour));
        let head = vec![tip, at(a + 0.14, r * 0.74), at(a - 0.14, r * 0.74)];
        painter.add(egui::Shape::convex_polygon(head, colour, Stroke::NONE));
        painter.text(
            at(a + 0.42, r * 0.66),
            Align2::CENTER_CENTER,
            name,
            FontId::proportional((r * 0.24).max(9.0)),
            colour,
        );
    };
    // True first, so the apparent needle is on top where they overlap.
    needle(twa, "TW", blue, 2.0);
    needle(AWA, "AW", strong, 3.0);

    let speed = |path: &str| {
        let reading = vessel.get(path)?;
        let n = reading.number()?;
        Some((catalog::quantity_for(path).format(n, &view.units), reading.is_stale()))
    };
    let unit = match (speed(AWS), speed(TWS)) {
        (None, None) => {
            painter.text(c, Align2::CENTER_CENTER, "–", FontId::monospace(r * 0.4), dim);
            ""
        }
        (aws, tws) => {
            let mut unit = "";
            if let Some((s, stale)) = aws {
                let colour = if stale { warn } else { strong };
                painter.text(c - Vec2::new(0.0, r * 0.08), Align2::CENTER_CENTER, s.value, FontId::monospace(r * 0.4), colour);
                unit = s.unit;
            }
            if let Some((s, stale)) = tws {
                let colour = if stale { warn } else { blue };
                painter.text(c + Vec2::new(0.0, r * 0.28), Align2::CENTER_CENTER, s.value, FontId::monospace(r * 0.27), colour);
                unit = s.unit;
            }
            unit
        }
    };
    if r > 40.0 {
        painter.text(c + Vec2::new(0.0, r * 0.52), Align2::CENTER_CENTER, unit, FontId::proportional(LABEL_SIZE), dim);
    }

    response.on_hover_text(
        "Wind rose, bow up\nAW, apparent wind: speed in the middle\n\
         TW, true wind (blue): speed below",
    )
}

/// Whether the boat sends what a tile shows — or, for a route tile, whether
/// a route is being followed.
fn is_live(path: &str, src: Sources) -> bool {
    let vessel = src.vessel;
    if catalog::is_route_tile(path) {
        return src.route.is_some();
    }
    if path == catalog::WIND_ROSE {
        return ["environment.wind.angleApparent", "environment.wind.angleTrueWater", "environment.wind.angleTrueGround"]
            .iter()
            .any(|p| vessel.get(p).is_some());
    }
    vessel.get(path).is_some()
}

/// The window behind the bar: where the server is, and what to show.
pub fn settings(
    ctx: &Context,
    view: &mut InstrumentView,
    src: Sources,
    actions: &mut Vec<UiAction>,
) {
    let mut open = view.open;
    egui::Window::new("Instruments")
        .constrain_to(ctx.available_rect())
        // No title-bar collapse: on a touchscreen it is an easy accidental
        // tap that leaves an empty title bar and no obvious way back.
        .collapsible(false)
        .open(&mut open)
        .resizable(true)
        .default_width(460.0)
        .show(ctx, |ui| {
            // Scrolled: on a plotter-sized screen the pages and the tile
            // lists run well below the bottom edge.
            egui::ScrollArea::vertical().show(ui, |ui| {
                connection(ui, view, actions);
                ui.add_space(10.0);
                ui.separator();
                layout(ui, view, actions);
                ui.add_space(10.0);
                ui.separator();
                pages(ui, view, actions);
                ui.add_space(10.0);
                picker(ui, view, src, actions);
            });
        });
    view.open = open;
}

fn connection(ui: &mut egui::Ui, view: &mut InstrumentView, actions: &mut Vec<UiAction>) {
    ui.label(RichText::new("Signal K server").strong());
    ui.label(
        RichText::new(
            "The address of the server on your boat. A host name is enough — \
             Manx adds the rest.",
        )
        .small()
        .weak(),
    );
    ui.horizontal(|ui| {
        let field = ui.add(
            egui::TextEdit::singleline(&mut view.url)
                .hint_text("openplotter.local  ·  192.168.1.50  ·  demo.signalk.org")
                .desired_width(260.0),
        );
        let entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if entered {
            // Enter connects to what was typed — reconnecting if a stream
            // is already open, since the address may have changed.
            actions.push(UiAction::SignalKConnect {
                url: view.url.clone(),
            });
        }
        if view.active {
            let label = if view.connected { "Disconnect" } else { "Stop" };
            if ui.button(label).clicked() {
                actions.push(UiAction::SignalKDisconnect);
            }
        } else if ui.button("Connect").clicked() {
            actions.push(UiAction::SignalKConnect {
                url: view.url.clone(),
            });
        }
    });
    ui.horizontal(|ui| {
        let colour = if view.connected {
            crate::render::theme::current().green
        } else {
            ui.visuals().weak_text_color()
        };
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, colour);
        ui.label(RichText::new(&view.status).small());
    });
    // One tap to the public demo, so the feature can be tried without a boat.
    if !view.active && ui.link("Try the public Signal K demo").clicked() {
        view.url = "demo.signalk.org".into();
        actions.push(UiAction::SignalKConnect {
            url: view.url.clone(),
        });
    }
}

fn layout(ui: &mut egui::Ui, view: &mut InstrumentView, actions: &mut Vec<UiAction>) {
    ui.label(RichText::new("Layout").strong());
    let before = after(view);
    ui.horizontal(|ui| {
        ui.label("Bar:");
        ui.selectable_value(&mut view.position, BarPosition::Top, "Top");
        ui.selectable_value(&mut view.position, BarPosition::Bottom, "Bottom");
        ui.selectable_value(&mut view.position, BarPosition::Left, "Left");
        ui.selectable_value(&mut view.position, BarPosition::Right, "Right");
        ui.selectable_value(&mut view.position, BarPosition::Hidden, "Hidden");
    });
    ui.horizontal(|ui| {
        ui.label("Depth:");
        use crate::signalk::units::DepthUnit::*;
        ui.selectable_value(&mut view.units.depth, Metres, "m");
        ui.selectable_value(&mut view.units.depth, Feet, "ft");
        ui.selectable_value(&mut view.units.depth, Fathoms, "fm");
        ui.add_space(12.0);
        ui.label("Speed:");
        use crate::signalk::units::SpeedUnit::*;
        ui.selectable_value(&mut view.units.speed, Knots, "kn");
        ui.selectable_value(&mut view.units.speed, Kilometres, "km/h");
        ui.selectable_value(&mut view.units.speed, MetresPerSecond, "m/s");
    });
    ui.horizontal(|ui| {
        ui.label("Temperature:");
        use crate::signalk::units::TemperatureUnit::*;
        ui.selectable_value(&mut view.units.temperature, Celsius, "°C");
        ui.selectable_value(&mut view.units.temperature, Fahrenheit, "°F");
    });
    ui.checkbox(&mut view.follow, "Keep the boat centred on the chart");

    ui.add_space(8.0);
    ui.label(RichText::new("AIS traffic").strong());
    ui.checkbox(&mut view.show_ais, "Show other vessels");
    ui.label(
        RichText::new(
            "A target is called dangerous only when it passes both tests. The \
             right numbers depend on the water — a quarter-mile is prudent \
             offshore and unusable in a busy strait, where every ferry would \
             trip it.",
        )
        .small()
        .weak(),
    );
    ui.horizontal(|ui| {
        ui.label("Warn within");
        ui.add(
            egui::DragValue::new(&mut view.cpa_alarm_nm)
                .speed(0.05)
                .range(0.02..=5.0)
                .fixed_decimals(2)
                .suffix(" NM"),
        );
        ui.label("and");
        ui.add(
            egui::DragValue::new(&mut view.tcpa_alarm_min)
                .speed(1.0)
                .range(1.0..=60.0)
                .fixed_decimals(0)
                .suffix(" min"),
        );
    });

    if before != after(view) {
        actions.push(UiAction::SettingsChanged);
    }
}

/// The settings worth writing to disk, as one comparable value.
fn after(view: &InstrumentView) -> Settings {
    (
        view.position,
        view.units,
        view.follow,
        view.show_ais,
        view.cpa_alarm_nm.to_bits(),
        view.tcpa_alarm_min.to_bits(),
    )
}

type Settings = (
    BarPosition,
    crate::signalk::UnitPrefs,
    bool,
    bool,
    u32,
    u32,
);

/// The bar's pages: which one is being edited — the same one the bar shows,
/// so the edit is seen as it is made — adding, naming and removing them.
fn pages(ui: &mut egui::Ui, view: &mut InstrumentView, actions: &mut Vec<UiAction>) {
    ui.label(RichText::new("Pages").strong());
    ui.label(
        RichText::new("Swipe across the bar to turn the page, or tap its dots.")
            .small()
            .weak(),
    );
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        for i in 0..view.pages.len() {
            if ui
                .selectable_label(i == view.page, &view.pages[i].name)
                .clicked()
                && i != view.page
            {
                view.page = i;
                changed = true;
            }
        }
        if ui.button("+").on_hover_text("Add a page").clicked() {
            view.pages.push(InstrumentPage {
                name: format!("Page {}", view.pages.len() + 1),
                tiles: Vec::new(),
            });
            view.page = view.pages.len() - 1;
            changed = true;
        }
    });
    if view.page < view.pages.len() {
        ui.horizontal(|ui| {
            ui.label("Name:");
            let page = view.page;
            changed |= ui
                .add(egui::TextEdit::singleline(&mut view.pages[page].name).desired_width(140.0))
                .changed();
            // The last page stays: a bar with no pages has nowhere to put a tile.
            let removable = view.pages.len() > 1;
            if ui
                .add_enabled(removable, egui::Button::new("Remove page"))
                .clicked()
            {
                view.pages.remove(view.page);
                view.page = view.page.min(view.pages.len() - 1);
                changed = true;
            }
        });
    }
    if changed {
        actions.push(UiAction::SettingsChanged);
    }
}

/// Choosing what the bar shows.
///
/// The offered list is what the boat has actually sent — a menu of every path
/// Signal K defines would run to hundreds and be mostly absent on any real
/// vessel. Until something connects there is nothing honest to offer.
fn picker(
    ui: &mut egui::Ui,
    view: &mut InstrumentView,
    src: Sources,
    actions: &mut Vec<UiAction>,
) {
    let vessel = src.vessel;
    let Some(page) = view.pages.get(view.page) else {
        return;
    };
    ui.label(RichText::new(format!("On this page — {}", page.name)).strong());
    let mut tiles = page.tiles.clone();

    let mut changed = false;
    let mut remove: Option<usize> = None;
    let mut move_to: Option<(usize, usize)> = None;

    for (i, path) in tiles.iter().enumerate() {
        ui.horizontal(|ui| {
            let live = is_live(path, src);
            let dot = if live {
                crate::render::theme::current().green
            } else {
                ui.visuals().weak_text_color()
            };
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 3.5, dot);

            ui.label(RichText::new(catalog::label_for(path)).strong());
            let about = if path == catalog::WIND_ROSE {
                "apparent and true wind"
            } else if let Some(t) = catalog::ROUTE_TILES.iter().find(|t| t.0 == path) {
                t.2
            } else {
                path
            };
            ui.label(RichText::new(about).small().weak());

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("✕").on_hover_text("Remove").clicked() {
                    remove = Some(i);
                }
                if ui
                    .add_enabled(i + 1 < tiles.len(), egui::Button::new("▼"))
                    .clicked()
                {
                    move_to = Some((i, i + 1));
                }
                if ui.add_enabled(i > 0, egui::Button::new("▲")).clicked() {
                    move_to = Some((i, i - 1));
                }
                if !live {
                    // Offline, nothing is sent at all; saying the boat does
                    // not send this path would read as a data fault.
                    ui.label(
                        RichText::new(if catalog::is_route_tile(path) {
                            "no route followed"
                        } else if view.connected {
                            "not sent by this boat"
                        } else {
                            "not connected"
                        })
                        .small()
                        .weak(),
                    );
                }
            });
        });
    }

    if let Some(i) = remove {
        tiles.remove(i);
        changed = true;
    }
    if let Some((from, to)) = move_to {
        tiles.swap(from, to);
        changed = true;
    }

    // The rose is drawn from several paths rather than being one, so it is
    // offered on its own rather than in the list of what the boat sends.
    if !tiles.iter().any(|t| t == catalog::WIND_ROSE) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("+").clicked() {
                tiles.push(catalog::WIND_ROSE.to_string());
                changed = true;
            }
            ui.label(RichText::new("Wind rose").strong());
            ui.label(RichText::new("apparent and true wind, angle and speed").small().weak());
        });
    }

    // The route's figures come from Manx, not the boat, so they are on offer
    // whether or not anything is connected or followed.
    let route_left: Vec<_> = catalog::ROUTE_TILES
        .iter()
        .filter(|t| !tiles.iter().any(|p| p == t.0))
        .collect();
    if !route_left.is_empty() {
        ui.add_space(8.0);
        ui.label(RichText::new("From the route being followed").strong());
        for (id, label, about) in route_left {
            ui.horizontal(|ui| {
                if ui.button("+").clicked() {
                    tiles.push(id.to_string());
                    changed = true;
                }
                ui.label(RichText::new(*label).strong());
                ui.label(RichText::new(*about).small().weak());
            });
        }
    }

    ui.add_space(8.0);
    ui.label(RichText::new("Available from this boat").strong());
    if view.available.is_empty() {
        ui.label(
            RichText::new(if view.connected {
                "Connected — waiting for the first readings…"
            } else {
                "Connect to a server to see what it offers."
            })
            .small()
            .weak(),
        );
        // Removing or reordering tiles offline is still a change to keep.
        if changed {
            view.pages[view.page].tiles = tiles;
            actions.push(UiAction::SettingsChanged);
        }
        return;
    }

    let mut add: Option<String> = None;
    egui::ScrollArea::vertical()
        .max_height(200.0)
        .show(ui, |ui| {
            for path in &view.available {
                if tiles.iter().any(|t| t == path) {
                    continue;
                }
                ui.horizontal(|ui| {
                    if ui.button("+").clicked() {
                        add = Some(path.clone());
                    }
                    ui.label(RichText::new(catalog::label_for(path)).strong());
                    ui.label(RichText::new(path).small().weak());
                    // Show it live, so you can tell which of four depth paths
                    // is the one your sounder actually feeds.
                    if let Some(n) = vessel.number(path) {
                        let r = catalog::quantity_for(path).format(n, &view.units);
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.label(
                                    RichText::new(format!("{} {}", r.value, r.unit)).small(),
                                );
                            },
                        );
                    }
                });
            }
        });
    if let Some(path) = add {
        tiles.push(path);
        changed = true;
    }

    if changed {
        view.pages[view.page].tiles = tiles;
        actions.push(UiAction::SettingsChanged);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn guidance() -> Guidance {
        Guidance {
            leg: 0,
            from: uuid::Uuid::nil(),
            to: uuid::Uuid::nil(),
            to_name: "b".into(),
            xte_nm: 0.0,
            btw_deg: 0.0,
            dtw_nm: 3.0,
            bod_deg: 0.0,
            vmg_kt: Some(6.0),
            dtg_nm: 15.0,
            sog_kt: Some(5.0),
            wp_plan_eta: None,
            dest_plan_eta: None,
            arrived: false,
            finished: false,
        }
    }

    fn at_noon() -> chrono::DateTime<chrono::Local> {
        chrono::Local.with_ymd_and_hms(2026, 9, 30, 12, 0, 0).unwrap()
    }

    fn read(id: &str, g: &Guidance) -> Option<(String, String)> {
        route_reading(id, g, &Default::default(), at_noon())
    }

    #[test]
    fn waypoint_time_is_reckoned_at_vmg_and_the_destination_at_sog() {
        let g = guidance();
        assert_eq!(read("manx.route.wpTime", &g).unwrap().0, "0:30");
        assert_eq!(read("manx.route.wpEta", &g).unwrap().0, "12:30");
        assert_eq!(read("manx.route.destTime", &g).unwrap().0, "3:00");
        assert_eq!(read("manx.route.destEta", &g).unwrap().0, "15:00");
        assert_eq!(read("manx.route.destDistance", &g).unwrap(), ("15.0".into(), "NM".into()));
    }

    #[test]
    fn no_time_is_promised_without_closing_speed_or_after_the_end() {
        let mut g = guidance();
        g.vmg_kt = Some(-2.0);
        g.sog_kt = None;
        assert!(read("manx.route.wpTime", &g).is_none());
        assert!(read("manx.route.destEta", &g).is_none());
        // Distance needs no speed.
        assert!(read("manx.route.wpDistance", &g).is_some());
        g.finished = true;
        assert!(read("manx.route.wpDistance", &g).is_none());
    }

    #[test]
    fn a_long_passage_counts_days() {
        assert_eq!(time_to_go(7.0 + 5.0 / 60.0), ("7:05".into(), String::new()));
        assert_eq!(time_to_go(240.0), ("10".into(), "d".into()));
        // 30 hours from noon is six in the evening, the day after next.
        assert_eq!(eta(at_noon(), 30.0), ("18:00".into(), "+1d".into()));
    }

    #[test]
    fn a_weather_route_is_read_against_its_plan() {
        let mut g = guidance();
        let noon = at_noon().to_utc();
        // Planned at the mark by 12:20 and the end by 16:00; going now, the
        // mark is half an hour off, so ten minutes late.
        g.wp_plan_eta = Some(noon + chrono::Duration::minutes(20));
        g.dest_plan_eta = Some(noon + chrono::Duration::hours(4));
        assert_eq!(read("manx.route.planDelta", &g).unwrap(), ("+0:10".into(), "late".into()));
        // The end follows the plan, ten minutes late — not DTG at SOG.
        assert_eq!(read("manx.route.destEta", &g).unwrap().0, "16:10");
        assert_eq!(read("manx.route.destTime", &g).unwrap().0, "4:10");

        g.wp_plan_eta = Some(noon + chrono::Duration::minutes(45));
        assert_eq!(read("manx.route.planDelta", &g).unwrap(), ("-0:15".into(), "early".into()));
        g.wp_plan_eta = Some(noon + chrono::Duration::minutes(30));
        assert_eq!(read("manx.route.planDelta", &g).unwrap().1, "on plan");

        // Not closing on the mark: no delta, and so no planned end either.
        g.vmg_kt = Some(0.0);
        assert!(read("manx.route.planDelta", &g).is_none());
        assert!(read("manx.route.destEta", &g).is_none());
    }

    #[test]
    fn a_route_without_a_plan_has_no_delta() {
        assert!(read("manx.route.planDelta", &guidance()).is_none());
    }

    #[test]
    fn a_single_saved_bar_becomes_the_first_page() {
        let json = r#"{"url":"","position":"Bottom","tiles":["navcore.windRose","navigation.speedOverGround"],
                       "units":{},"follow":true}"#;
        let v: InstrumentView = serde_json::from_str(json).unwrap();
        let v = v.upgraded();
        assert_eq!(v.pages.len(), 2);
        assert_eq!(v.pages[0].tiles, [catalog::WIND_ROSE, "navigation.speedOverGround"]);
        assert_eq!(v.pages[1], InstrumentPage::route());
        // Written back, the pages replace the old single list.
        let out: serde_json::Value = serde_json::to_value(&v).unwrap();
        assert!(out.get("tiles").is_none());
        assert!(out.get("pages").is_some());
    }
}
