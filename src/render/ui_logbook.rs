//! The logbook window: the days the boat has logged, each day's numbers and
//! the sailor's notes, and the way out to GPX and CSV.
//!
//! The renderer reads the files and hands this module rows already summed
//! (see `RenderState::logbook_view`); nothing here touches the disk.

use chrono::{Local, NaiveDate, TimeZone};
use egui::{Context, RichText, Sense, Stroke};

use super::ui::UiAction;
use crate::nav::logbook::{Note, Summary};

/// The logbook's settings and the window's own state.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct LogView {
    /// Record while the boat sends data.
    pub record: bool,
    /// Draw the open day's track on the chart.
    pub show_on_chart: bool,

    #[serde(skip)]
    pub open: bool,
    /// The day being looked at; `None` is the list of days.
    #[serde(skip)]
    pub selected: Option<NaiveDate>,
    #[serde(skip)]
    pub draft: String,
    #[serde(skip)]
    pub day_draft: String,
    #[serde(skip)]
    pub delete_armed: bool,
}

impl Default for LogView {
    fn default() -> Self {
        Self {
            record: true,
            show_on_chart: true,
            open: false,
            selected: None,
            draft: String::new(),
            day_draft: String::new(),
            delete_armed: false,
        }
    }
}

/// One day in the list.
#[derive(Debug, Clone)]
pub struct DayRow {
    pub day: NaiveDate,
    pub summary: Summary,
    pub notes: usize,
    pub bytes: u64,
}

/// The open day.
#[derive(Debug, Clone)]
pub struct DayDetail {
    pub row: DayRow,
    pub notes: Vec<Note>,
}

/// What the renderer tells the window.
#[derive(Debug, Clone, Default)]
pub struct LogbookState {
    pub today: Option<DayRow>,
    pub days: Vec<DayRow>,
    pub detail: Option<DayDetail>,
    /// Where the last export went, or why it failed.
    pub message: Option<String>,
    /// The boat is sending a position now.
    pub has_fix: bool,
}

fn local(t: i64) -> chrono::DateTime<Local> {
    Local.timestamp_opt(t, 0).single().unwrap_or_else(Local::now)
}

fn duration(s: i64) -> String {
    let (h, m) = (s / 3600, (s % 3600) / 60);
    if h > 0 { format!("{h} h {m:02}") } else { format!("{m} min") }
}

fn size(bytes: u64) -> String {
    if bytes < 1024 { format!("{bytes} B") } else { format!("{:.0} KB", bytes as f64 / 1024.0) }
}

/// "21.0 NM · 4 h 12 under way · max 7.1 kn · 2 notes"
fn line(row: &DayRow) -> String {
    let s = &row.summary;
    let mut parts = Vec::new();
    if s.underway_s > 0 {
        parts.push(format!("{} under way", duration(s.underway_s)));
    } else if s.samples > 0 {
        parts.push("stopped".to_string());
    }
    if let Some(v) = s.max_sog.filter(|v| *v >= crate::nav::logbook::MOVING_KN) {
        parts.push(format!("max {v:.1} kn"));
    }
    if row.notes > 0 {
        parts.push(format!("{} note{}", row.notes, if row.notes == 1 { "" } else { "s" }));
    }
    parts.join(" · ")
}

pub fn window(ctx: &Context, view: &mut LogView, state: &LogbookState, actions: &mut Vec<UiAction>) {
    let before = (view.record, view.show_on_chart);
    let mut open = view.open;
    // Scrolls inside a window that fits the screen: a day with its notes is
    // taller than a plotter's display.
    let max_h = (ctx.available_rect().height() - 120.0).max(240.0);
    egui::Window::new("Logbook")
        .open(&mut open)
        .default_width(460.0)
        // Capped: a field that asks for the width available would otherwise
        // widen the window a little every frame.
        .max_width(520.0)
        .max_height(max_h)
        .vscroll(true)
        .show(ctx, |ui| {
            header(ui, view, state, actions);
            ui.separator();
            match (&state.detail, view.selected) {
                (Some(d), Some(_)) => detail(ui, view, d, state, actions),
                _ => list(ui, view, state, actions),
            }
            if let Some(ref m) = state.message {
                ui.separator();
                ui.label(RichText::new(m).weak().small());
            }
        });
    view.open = open;
    if !open {
        view.delete_armed = false;
    }
    if (view.record, view.show_on_chart) != before {
        actions.push(UiAction::SettingsChanged);
    }
}

/// Recording, today, and the quick note.
fn header(ui: &mut egui::Ui, view: &mut LogView, state: &LogbookState, actions: &mut Vec<UiAction>) {
    ui.horizontal(|ui| {
        let (dot, text) = if view.record {
            (crate::render::theme::current().red, "Recording")
        } else {
            (crate::render::theme::current().ink_dim, "Paused")
        };
        // Drawn, not a "●": the interface font has no such glyph.
        let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 5.0, dot);
        let r = ui
            .selectable_label(view.record, RichText::new(text).strong())
            .on_hover_text(if view.record {
                "Tap to pause the log"
            } else {
                "Tap to record position, speeds, wind and depth"
            });
        if r.clicked() {
            view.record = !view.record;
        }
        if let Some(ref t) = state.today {
            ui.label(
                RichText::new(format!(
                    "Today {:.1} NM · {}",
                    t.summary.distance_nm,
                    if t.summary.underway_s > 0 { duration(t.summary.underway_s) } else { "stopped".into() }
                ))
                .weak(),
            );
        }
    });
    ui.horizontal(|ui| {
        let edit = ui.add(
            egui::TextEdit::singleline(&mut view.draft)
                .hint_text(if state.has_fix {
                    "Add a note — time and position go with it"
                } else {
                    "Add a note — time goes with it"
                })
                .desired_width(ui.available_width() - 60.0),
        );
        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (ui.button("Add").clicked() || enter) && !view.draft.trim().is_empty() {
            actions.push(UiAction::LogNoteAdd { day: None, text: view.draft.trim().to_string() });
            view.draft.clear();
        }
    });
}

fn list(ui: &mut egui::Ui, view: &mut LogView, state: &LogbookState, actions: &mut Vec<UiAction>) {
    if state.days.is_empty() {
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "Nothing logged yet. The log records while Recording is on and the boat sends \
                 its position over Signal K.",
            )
            .weak(),
        );
        return;
    }
    for row in &state.days {
        if day_row(ui, row) {
            view.selected = Some(row.day);
            view.delete_armed = false;
            actions.push(UiAction::LogSelect { day: Some(row.day) });
        }
    }
}

/// One day in the list, as a card. Returns whether it was tapped.
fn day_row(ui: &mut egui::Ui, row: &DayRow) -> bool {
    let visuals = ui.visuals().clone();
    let inner = egui::Frame::none()
        .inner_margin(egui::Margin::symmetric(8.0, 6.0))
        .rounding(6.0)
        .fill(visuals.faint_bg_color)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(row.day.format("%a %-d %b %Y").to_string()).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(format!("{:.1} NM", row.summary.distance_nm)).strong());
                });
            });
            let l = line(row);
            if !l.is_empty() {
                ui.label(RichText::new(l).weak());
            }
        });
    let response = ui.interact(inner.response.rect, ui.id().with(("log-day", row.day)), Sense::click());
    if response.hovered() {
        ui.painter().rect_stroke(
            inner.response.rect,
            6.0,
            Stroke::new(1.0_f32, visuals.selection.stroke.color),
        );
    }
    ui.add_space(4.0);
    response.clicked()
}

fn detail(
    ui: &mut egui::Ui,
    view: &mut LogView,
    d: &DayDetail,
    state: &LogbookState,
    actions: &mut Vec<UiAction>,
) {
    let s = &d.row.summary;
    ui.horizontal(|ui| {
        if ui.button("‹ All days").clicked() {
            view.selected = None;
            view.delete_armed = false;
            actions.push(UiAction::LogSelect { day: None });
        }
        ui.label(RichText::new(d.row.day.format("%A %-d %B %Y").to_string()).strong().size(16.0));
    });
    ui.add_space(4.0);

    // Two columns of plain text, not a grid: grid rows take the touch-sized
    // interaction height, which spread nine short lines over a whole screen.
    let mut rows: Vec<(&str, String)> = Vec::new();
    rows.push(("Distance", format!("{:.1} NM", s.distance_nm)));
    rows.push(("Under way", if s.underway_s > 0 { duration(s.underway_s) } else { "—".into() }));
    if let Some(a) = s.avg_sog() {
        rows.push(("Speed", format!("{a:.1} kn average, {:.1} kn max", s.max_sog.unwrap_or(a))));
    }
    if let (Some(f), Some(l)) = (s.first, s.last) {
        let place = |p: Option<crate::geo::LatLon>| {
            p.map(|p| format!("  {}", crate::geo::format_latlon(p.lat, p.lon))).unwrap_or_default()
        };
        rows.push(("From", format!("{}{}", local(f.t).format("%H:%M"), place(s.start()))));
        rows.push(("To", format!("{}{}", local(l.t).format("%H:%M"), place(s.end()))));
    }
    match (s.max_tws, s.max_aws) {
        (Some(t), _) => rows.push(("Wind", format!("up to {t:.0} kn true"))),
        (None, Some(a)) => rows.push(("Wind", format!("up to {a:.0} kn apparent"))),
        _ => {}
    }
    if let Some(dep) = s.min_depth {
        rows.push(("Least depth", format!("{dep:.1} m")));
    }
    rows.push(("Log", format!("{} samples, {}", s.samples, size(d.row.bytes))));
    ui.horizontal_top(|ui| {
        let keys: Vec<&str> = rows.iter().map(|(k, _)| *k).collect();
        let values: Vec<&str> = rows.iter().map(|(_, v)| v.as_str()).collect();
        ui.label(RichText::new(keys.join("\n")).weak());
        ui.add_space(12.0);
        ui.label(values.join("\n"));
    });

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.checkbox(&mut view.show_on_chart, "Show on chart");
        if ui.add_enabled(s.start().is_some(), egui::Button::new("Fit to screen")).clicked() {
            actions.push(UiAction::LogFit);
        }
    });

    ui.add_space(6.0);
    ui.label(RichText::new("Notes").strong());
    if d.notes.is_empty() {
        ui.label(RichText::new("No notes this day.").weak());
    }
    for n in &d.notes {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(local(n.t).format("%H:%M").to_string()).monospace().weak());
            ui.label(&n.text);
            if ui.small_button("×").on_hover_text("Delete this note").clicked() {
                actions.push(UiAction::LogNoteDelete(n.clone()));
            }
        });
    }
    let today = state.today.as_ref().is_some_and(|t| t.day == d.row.day);
    if !today {
        // A note added later to a past day: stamped at the day's end, no
        // position — it is a remark on the day, not a fix.
        ui.horizontal(|ui| {
            let edit = ui.add(
                egui::TextEdit::singleline(&mut view.day_draft)
                    .hint_text("Add a note to this day")
                    .desired_width(ui.available_width() - 60.0),
            );
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.button("Add").clicked() || enter) && !view.day_draft.trim().is_empty() {
                actions.push(UiAction::LogNoteAdd {
                    day: Some(d.row.day),
                    text: view.day_draft.trim().to_string(),
                });
                view.day_draft.clear();
            }
        });
    }

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label("Export");
        if ui.button("GPX").on_hover_text("The track and the notes, for another plotter — saved to Downloads (on Android, then shared)").clicked() {
            actions.push(UiAction::LogExport { day: d.row.day, csv: false });
        }
        if ui.button("CSV").on_hover_text("Every sample, for a spreadsheet — saved to Downloads (on Android, then shared)").clicked() {
            actions.push(UiAction::LogExport { day: d.row.day, csv: true });
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let text = if view.delete_armed { "Tap again to delete" } else { "Delete day" };
            if ui.button(RichText::new(text).color(crate::render::theme::current().red)).clicked() {
                if view.delete_armed {
                    view.delete_armed = false;
                    view.selected = None;
                    actions.push(UiAction::LogDelete(d.row.day));
                } else {
                    view.delete_armed = true;
                }
            }
        });
    });
}
