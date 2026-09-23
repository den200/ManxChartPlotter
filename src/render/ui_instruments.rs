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

use egui::{Align2, Color32, Context, FontId, RichText, Sense, Stroke, Vec2};

use super::ui::{BarPosition, InstrumentView, UiAction};
use crate::signalk::{catalog, state::Vessel};

/// Height of the strip in points. Enough for a label and a large number, and
/// enough to hit.
const BAR_HEIGHT: f32 = 76.0;
const TILE_WIDTH: f32 = 108.0;
const VALUE_SIZE: f32 = 30.0;
const LABEL_SIZE: f32 = 11.0;

/// Draw the strip. Returns nothing; interaction arrives as actions.
pub fn bar(ctx: &Context, view: &mut InstrumentView, vessel: &Vessel) {
    // Read the edge before the closure borrows `view`, so the panel choice is
    // not itself part of the borrow.
    let position = view.position;
    if position == BarPosition::Hidden || view.tiles.is_empty() {
        return;
    }

    let draw = |ui: &mut egui::Ui| {
        ui.horizontal_centered(|ui| {
            ui.add_space(6.0);
            for path in view.tiles.clone() {
                if tile(ui, &path, vessel, view).clicked() {
                    // A tile is the shortest route to the thing that
                    // configures it — no hunting through a menu.
                    view.open = true;
                }
            }
            // The connection's health belongs on the bar, not buried in a
            // window: a stale reading and a dropped link look identical
            // otherwise.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(8.0);
                // Orange is a warning — a link that should be up and is
                // not. With no server asked for, there is nothing to warn of.
                let (colour, tip) = if view.connected {
                    (Color32::from_rgb(80, 190, 120), view.status.clone())
                } else if view.active {
                    (Color32::from_rgb(210, 130, 60), view.status.clone())
                } else {
                    (ui.visuals().weak_text_color(), view.status.clone())
                };
                let (rect, response) =
                    ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
                ui.painter().circle_filled(rect.center(), 5.0, colour);
                if response.on_hover_text(tip).clicked() {
                    view.open = true;
                }
            });
        });
    };

    match position {
        BarPosition::Top => {
            egui::TopBottomPanel::top("instrument-bar")
                .exact_height(BAR_HEIGHT)
                .show_separator_line(true)
                .show(ctx, draw);
        }
        BarPosition::Bottom => {
            egui::TopBottomPanel::bottom("instrument-bar")
                .exact_height(BAR_HEIGHT)
                .show_separator_line(true)
                .show(ctx, draw);
        }
        BarPosition::Hidden => {}
    }
}

/// One reading.
fn tile(
    ui: &mut egui::Ui,
    path: &str,
    vessel: &Vessel,
    view: &InstrumentView,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(TILE_WIDTH, BAR_HEIGHT - 12.0),
        Sense::click(),
    );
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

/// The window behind the bar: where the server is, and what to show.
pub fn settings(
    ctx: &Context,
    view: &mut InstrumentView,
    vessel: &Vessel,
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
            connection(ui, view, actions);
            ui.add_space(10.0);
            ui.separator();
            layout(ui, view, actions);
            ui.add_space(10.0);
            ui.separator();
            picker(ui, view, vessel, actions);
        });
    view.open = open;
}

fn connection(ui: &mut egui::Ui, view: &mut InstrumentView, actions: &mut Vec<UiAction>) {
    ui.label(RichText::new("Signal K server").strong());
    ui.label(
        RichText::new(
            "The address of the server on your boat. A host name is enough — \
             navcore adds the rest.",
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
            Color32::from_rgb(80, 190, 120)
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

/// Choosing what the bar shows.
///
/// The offered list is what the boat has actually sent — a menu of every path
/// Signal K defines would run to hundreds and be mostly absent on any real
/// vessel. Until something connects there is nothing honest to offer.
fn picker(
    ui: &mut egui::Ui,
    view: &mut InstrumentView,
    vessel: &Vessel,
    actions: &mut Vec<UiAction>,
) {
    ui.label(RichText::new("On the bar").strong());

    let mut changed = false;
    let mut remove: Option<usize> = None;
    let mut move_to: Option<(usize, usize)> = None;

    for (i, path) in view.tiles.iter().enumerate() {
        ui.horizontal(|ui| {
            let live = vessel.get(path).is_some();
            let dot = if live {
                Color32::from_rgb(80, 190, 120)
            } else {
                ui.visuals().weak_text_color()
            };
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 3.5, dot);

            ui.label(RichText::new(catalog::label_for(path)).strong());
            ui.label(RichText::new(path).small().weak());

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("✕").on_hover_text("Remove").clicked() {
                    remove = Some(i);
                }
                if ui
                    .add_enabled(i + 1 < view.tiles.len(), egui::Button::new("▼"))
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
                        RichText::new(if view.connected {
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
        view.tiles.remove(i);
        changed = true;
    }
    if let Some((from, to)) = move_to {
        view.tiles.swap(from, to);
        changed = true;
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
            actions.push(UiAction::SettingsChanged);
        }
        return;
    }

    let mut add: Option<String> = None;
    egui::ScrollArea::vertical()
        .max_height(200.0)
        .show(ui, |ui| {
            for path in &view.available {
                if view.tiles.iter().any(|t| t == path) {
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
        view.tiles.push(path);
        changed = true;
    }

    if changed {
        actions.push(UiAction::SettingsChanged);
    }
}
