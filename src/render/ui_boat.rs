//! The boat window: who she is, what she needs, and where her polar
//! comes from.
//!
//! The specs are not decoration. Draft and air draft become the routers'
//! safety envelope — what water is deep enough, what bridge is high
//! enough — and the polar found here is the speed model every sailing
//! plan stands on. One window, because on a boat these are one subject.

use egui::{Context, RichText, ScrollArea, Window};

use super::ui::{BoatView, UiAction, WeatherView};

pub fn window(
    ctx: &Context,
    boat: &mut BoatView,
    weather: &WeatherView,
    actions: &mut Vec<UiAction>,
) {
    let mut open = boat.open;
    Window::new("Boat")
        .open(&mut open)
        .default_size([520.0, 420.0])
        .default_pos([120.0, 60.0])
        .constrain(true)
        .show(ctx, |ui| {
            let mut edited = false;
            ui.horizontal(|ui| {
                ui.label(RichText::new("Name:").weak());
                edited |= ui
                    .add(egui::TextEdit::singleline(&mut boat.name).desired_width(140.0))
                    .lost_focus();
                ui.label(RichText::new("Type:").weak());
                edited |= ui
                    .add(
                        egui::TextEdit::singleline(&mut boat.boat_type)
                            .hint_text("e.g. X-99")
                            .desired_width(140.0),
                    )
                    .lost_focus();
            });
            ui.horizontal(|ui| {
                let mut field = |ui: &mut egui::Ui, label: &str, v: &mut f64, max: f64| {
                    ui.label(RichText::new(label).weak());
                    edited |= ui
                        .add(
                            egui::DragValue::new(v)
                                .speed(0.05)
                                .range(0.0..=max)
                                .suffix(" m"),
                        )
                        .changed();
                };
                field(ui, "LOA", &mut boat.loa_m, 60.0);
                field(ui, "beam", &mut boat.beam_m, 15.0);
                field(ui, "draft", &mut boat.draft_m, 6.0);
                field(ui, "air draft", &mut boat.air_draft_m, 60.0);
            });
            if edited {
                // A boat's numbers are worth remembering the moment they are
                // typed; nobody re-enters their draft twice happily.
                actions.push(UiAction::SettingsChanged);
            }
            ui.label(
                RichText::new(
                    "Draft and air draft feed the route planners' safety margins. \
                     Zero means \"use the cautious default\".",
                )
                .small()
                .weak(),
            );

            ui.separator();
            ui.label(RichText::new("Find the polar (ORC certificates)").strong());
            ui.horizontal(|ui| {
                let search_edit = ui.add(
                    egui::TextEdit::singleline(&mut boat.search)
                        .hint_text("class or boat name, e.g. Luffe 40")
                        .desired_width(200.0),
                );
                egui::ComboBox::from_id_salt("orc-country")
                    .selected_text(if boat.country.is_empty() { "ALL" } else { &boat.country })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut boat.country, "ALL".to_string(), "ALL");
                        for c in crate::nav::orc::COUNTRIES {
                            ui.selectable_value(&mut boat.country, c.to_string(), *c);
                        }
                    });
                let entered = search_edit.lost_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (ui.button("Search").clicked() || entered) && !boat.busy {
                    actions.push(UiAction::BoatSearch {
                        query: boat.search.clone(),
                        country: boat.country.clone(),
                    });
                }
            });
            if boat.busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new(&boat.status).small());
                });
            } else if !boat.status.is_empty() {
                ui.label(RichText::new(&boat.status).small());
            }

            if !boat.results.is_empty() {
                ui.add_space(4.0);
                ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                    for (i, hit) in boat.results.iter().enumerate() {
                        ui.horizontal(|ui| {
                            if ui.button("Use").clicked() {
                                actions.push(UiAction::BoatUsePolar { index: i });
                            }
                            ui.label(RichText::new(&hit.class).strong());
                            let mut detail = format!(
                                "{:.1} m, draft {:.2} m",
                                hit.loa_m, hit.draft_m
                            );
                            if !hit.name.is_empty() {
                                detail = format!("\"{}\" — {detail}", hit.name);
                            }
                            if !hit.year.is_empty() && hit.year != "0" {
                                detail.push_str(&format!(", {}", hit.year));
                            }
                            ui.label(RichText::new(detail).weak().small());
                        });
                    }
                });
            }

            ui.add_space(4.0);
            let polar_label = if weather.polar_path.trim().is_empty() {
                "Polar in use: built-in ~10 m cruiser".to_string()
            } else {
                format!("Polar in use: {}", weather.polar_path)
            };
            ui.label(RichText::new(polar_label).small().weak());
        });
    boat.open = open;
}
