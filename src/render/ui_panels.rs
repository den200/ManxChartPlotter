//! The panels themselves.
//!
//! Kept apart from the egui plumbing in [`super::ui`] so the layout of a panel
//! can be read, and changed, without wading through render passes.

use egui::{Align2, Context, RichText, ScrollArea, Window};

use super::ui::{ShopView, UiAction, UiState};
use crate::shop::protocol::choose_download;
use crate::pick::PickedObject;
use crate::senc::FeatureType;

pub fn build(
    ctx: &Context,
    state: &UiState<'_>,
    shop: &mut ShopView,
    instruments: &mut crate::render::ui::InstrumentView,
    routes: &mut crate::render::ui::RoutesView,
    weather: &mut crate::render::ui::WeatherView,
    fleet: &crate::signalk::Fleet,
    actions: &mut Vec<UiAction>,
) {
    menu_bar(ctx, shop, instruments, actions);
    // Declared before the instrument bar claims its edge, so the strip sits
    // directly above the bar rather than under it.
    if let Some(ref g) = routes.guidance {
        super::ui_routes::guidance_strip(ctx, g);
    }
    // The strip claims its edge before the chart is told how much room it has,
    // so a window opened over it still lands inside the remaining area.
    super::ui_instruments::bar(ctx, instruments, &fleet.own);
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
    if let Some(objects) = state.picked {
        object_query(ctx, objects, state.pick_anchor, actions);
    }
    if shop.open {
        chart_shop(ctx, shop, actions);
    }
    if instruments.open {
        super::ui_instruments::settings(ctx, instruments, &fleet.own, actions);
    }
    if routes.open {
        super::ui_routes::window(ctx, routes, weather, actions);
    }
}

/// A thin strip along the top. Deliberately thin: the chart is the instrument,
/// and every row of pixels the interface takes is a row of sea it does not show.
fn menu_bar(
    ctx: &Context,
    shop: &mut ShopView,
    instruments: &mut crate::render::ui::InstrumentView,
    actions: &mut Vec<UiAction>,
) {
    egui::TopBottomPanel::top("menu").show(ctx, |ui| {
        ui.horizontal(|ui| {
            if ui.button("Charts").clicked() {
                shop.open = !shop.open;
            }
            if ui.button("Instruments").clicked() {
                instruments.open = !instruments.open;
            }
            if ui.button("Routes").clicked() {
                actions.push(UiAction::RoutesOpen);
            }
            ui.separator();
            ui.label(
                RichText::new("tap the chart to identify an object")
                    .small()
                    .weak(),
            );
        });
    });
}

/// The chart shop: sign in, see what the account owns, see what is stale.
fn chart_shop(ctx: &Context, shop: &mut ShopView, actions: &mut Vec<UiAction>) {
    let mut open = shop.open;
    Window::new("Charts")
        .open(&mut open)
        .default_size([620.0, 460.0])
        .default_pos([60.0, 80.0])
        // Kept inside the screen, and never repositioned by anything but a
        // drag: an area that egui re-places from an anchor each frame cannot
        // be moved by hand, and one that is free to leave the screen can be
        // dragged somewhere it cannot be dragged back from.
        .constrain(true)
        .collapsible(false)
        .show(ctx, |ui| {
            // Numbered to match o-charts' own instructions — sign in, identify
            // this system, install the chart — so a user who has read their
            // page recognises where they are.
            if !shop.signed_in {
                ui.label(RichText::new("1. Sign in to o-charts").strong());
                ui.label(
                    RichText::new("The same account you bought the charts with.")
                        .small()
                        .weak(),
                );
                ui.add_space(4.0);
                ui.add_space(6.0);
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
                        ui.add(
                            egui::TextEdit::singleline(&mut shop.password)
                                .password(true)
                                .desired_width(280.0),
                        );
                        ui.end_row();
                    });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let ready = !shop.email.trim().is_empty() && !shop.password.is_empty();
                    if ui
                        .add_enabled(ready && !shop.busy, egui::Button::new("Sign in"))
                        .clicked()
                    {
                        actions.push(UiAction::ShopSignIn {
                            email: shop.email.trim().to_string(),
                            password: std::mem::take(&mut shop.password),
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
            } else {
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
                        if ui.button("Sign out").clicked() {
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
                // A machine has to be named before the shop will hand it a
                // chart: a slot is an assignment to a named computer, not to
                // an account.
                if shop.system_name.is_none() {
                    ui.add_space(6.0);
                    ui.label(RichText::new("2. Identify this system").strong());
                    ui.label(
                        RichText::new(
                            "A chart licence is assigned to a named computer, usually five \
                             of them. Registering a name costs nothing — a slot is spent only \
                             when a chart set is assigned to it.",
                        )
                        .small()
                        .weak(),
                    );
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
                        ui.add(
                            egui::TextEdit::singleline(&mut shop.new_system_name)
                                .hint_text("e.g. saloon-mac")
                                .desired_width(180.0),
                        );
                        let ready = !shop.new_system_name.trim().is_empty();
                        if ui
                            .add_enabled(ready && !shop.busy, egui::Button::new("Register"))
                            .clicked()
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
                            .color(egui::Color32::from_rgb(180, 120, 40)),
                        );
                    }
                }
                ui.add_space(6.0);
                ui.label(RichText::new("3. Install your charts").strong());
                ui.separator();
                chart_table(ui, shop, actions);
                if let Some(pending) = shop.pending.clone() {
                    confirm_download(ui, &pending, actions);
                }
            }

            if !shop.status.is_empty() {
                ui.add_space(6.0);
                ui.separator();
                ui.label(RichText::new(&shop.status).small());
            }
        });
    shop.open = open;
}

/// Put a lapsed set's download to the user before sending it.
///
/// The shop will not grant the edition it currently publishes to a licence
/// that expired before that edition existed. navcore can ask for an older one
/// — the last this machine actually received — but which edition to claim is
/// the user's business, so it is shown, named, and confirmed.
fn confirm_download(
    ui: &mut egui::Ui,
    pending: &crate::render::ui::PendingDownload,
    actions: &mut Vec<UiAction>,
) {
    ui.add_space(8.0);
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.label(
            RichText::new(format!("{} — subscription lapsed", pending.chart_name)).strong(),
        );
        ui.label(RichText::new(&pending.because).small());
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            match &pending.edition {
                Some(edition) => {
                    if ui
                        .button(format!("Ask for edition {edition}"))
                        .clicked()
                    {
                        actions.push(UiAction::ShopDownload {
                            chart_id: pending.chart_id.clone(),
                            edition: Some(edition.clone()),
                        });
                    }
                }
                None => {
                    // Nothing on disk and nothing on the slot: there is no
                    // older edition to name. Asking for the current one will
                    // almost certainly be refused, but the shop's answer is
                    // more use than navcore's guess about it.
                    if ui
                        .button("Ask for the current edition anyway")
                        .on_hover_text(
                            "Expect a refusal — the licence expired before this edition \
                             was published. The shop's exact answer is worth having.",
                        )
                        .clicked()
                    {
                        actions.push(UiAction::ShopDownload {
                            chart_id: pending.chart_id.clone(),
                            edition: None,
                        });
                    }
                }
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
                        RichText::new("Expired").color(egui::Color32::from_rgb(180, 60, 60))
                    } else {
                        RichText::new(target.label())
                    };
                    ui.label(state);
                    // What this machine's claim on the chart is. A slot is
                    // spent per machine, so the count has to be the shop's
                    // real one: saying five are free when three are invites
                    // the user to hand out slots they do not have.
                    let mine = shop.system_name.as_deref().and_then(|n| c.slot_for(n));
                    let on_key = c.dongle_slot();
                    let cell = match (mine.is_some(), on_key) {
                        (true, _) => "assigned here".to_string(),
                        (false, Some(s)) => format!("on USB key {}", s.assigned_system),
                        (false, None) => {
                            format!("{} of {} free", c.free_slots(), c.total_slots())
                        }
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
                    // Offered even for an expired subscription. Whether the
                    // last edition it covered may still be fetched is the
                    // shop's decision, and the only way to learn it is to ask.
                    let can_ask =
                        shop.system_name.is_some() && !shop.busy && !c.is_fully_assigned();
                    let button = ui
                        .add_enabled(can_ask, egui::Button::new("Download"))
                        .on_disabled_hover_text(if shop.system_name.is_none() {
                            "register this machine first"
                        } else if c.is_fully_assigned() {
                            "every slot on this licence is assigned to another machine"
                        } else {
                            "busy"
                        });
                    let button = if c.expired {
                        button.on_hover_text(
                            "This subscription has lapsed. o-charts decides whether the \
                             last edition it covered may still be fetched — pressing this \
                             asks, and shows the answer.",
                        )
                    } else {
                        button
                    };
                    if button.clicked() {
                        // A lapsed set cannot have the shop's current edition,
                        // so navcore must ask for a different one. That is a
                        // decision about the user's licence, so it is put to
                        // them rather than made silently.
                        actions.push(if c.expired {
                            UiAction::ShopConfirmDownload {
                                chart_id: c.id.clone(),
                            }
                        } else {
                            UiAction::ShopDownload {
                                chart_id: c.id.clone(),
                                edition: None,
                            }
                        });
                    }
                    ui.end_row();
                    // The provenance of the licence, on its own row: what was
                    // bought, when it lapsed, and which edition this machine's
                    // slot actually holds. For an expired set that last figure
                    // is the whole question — it is the edition the licence
                    // paid for, and the only one worth asking the shop for.
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
    actions: &mut Vec<UiAction>,
) {
    let mut open = true;
    let screen = ctx.screen_rect();
    // Beside the tap, and never more than half the screen, so the chart stays
    // visible while the panel is up.
    let max_h = (screen.height() * 0.6).max(160.0);
    let max_w = (screen.width() * 0.45).clamp(260.0, 460.0);

    let mut window = Window::new("Chart object")
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

    window.show(ctx, |ui| {
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
                    .small_button("Copy all")
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
                .small_button("Copy")
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
                    ui.label(v);
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
