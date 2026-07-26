//! The panels themselves.
//!
//! Kept apart from the egui plumbing in [`super::ui`] so the layout of a panel
//! can be read, and changed, without wading through render passes.

use egui::{Align2, Context, RichText, ScrollArea, Window};

use super::ui::{UiAction, UiState};
use crate::pick::PickedObject;
use crate::senc::FeatureType;

pub fn build(ctx: &Context, state: &UiState<'_>, actions: &mut Vec<UiAction>) {
    if let Some(objects) = state.picked {
        object_query(ctx, objects, state.pick_anchor, actions);
    }
}

/// The object-query bubble.
///
/// Anchored near the tap but free to be dragged, because on a small screen the
/// thing you just tapped is exactly what the panel covers.
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
        ui.label(
            RichText::new(format!(
                "{} object{} here",
                objects.len(),
                if objects.len() == 1 { "" } else { "s" }
            ))
            .small()
            .weak(),
        );
        ui.separator();

        // Scrolls, so nothing is truncated: a tap in a harbour can find forty
        // objects and every one of them is now reachable.
        ScrollArea::vertical().show(ui, |ui| {
            for (i, o) in objects.iter().enumerate() {
                if i > 0 {
                    ui.add_space(4.0);
                    ui.separator();
                }
                object(ui, o);
            }
        });
    });

    if !open {
        actions.push(UiAction::DismissPick);
    }
}

fn object(ui: &mut egui::Ui, o: &PickedObject) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(&o.title).strong());
        ui.label(RichText::new(format!("({})", o.acronym)).weak().small());
    });
    ui.label(
        RichText::new(format!(
            "{} \u{2022} 1:{} \u{2022} {}",
            geometry_name(o.geometry),
            o.chart_scale,
            o.chart
        ))
        .small()
        .weak(),
    );

    if !o.attributes.is_empty() {
        egui::Grid::new(format!("attrs-{}-{}", o.acronym, o.chart))
            .num_columns(2)
            .spacing([12.0, 2.0])
            .show(ui, |ui| {
                for (k, v) in &o.attributes {
                    ui.label(RichText::new(k).monospace().weak());
                    ui.label(v);
                    ui.end_row();
                }
            });
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

fn geometry_name(kind: FeatureType) -> &'static str {
    match kind {
        FeatureType::Point => "point",
        FeatureType::Line => "line",
        FeatureType::Area => "area",
        FeatureType::Multipoint => "soundings",
    }
}
