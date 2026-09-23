//! The weather sheet: one surface for wind, sea, current, rain and tide.
//!
//! The shape is Orca's, and it is worth saying why it is better than what
//! navcore had. A floating "Wind" window with a step slider asks the reader
//! to hold two things in their head — which hour the slider is on, and what
//! that hour looked like everywhere else. A sheet along the bottom of the
//! chart holds a *time axis* instead, draws every quantity against it in its
//! own lane, and puts one cursor through all of them. Move the cursor and the
//! whole picture moves together, chart included: the barbs on the water are
//! showing the same instant the tide curve's dot is sitting on.
//!
//! Five lanes, top to bottom, in the order a sailor asks the questions:
//!
//! * **Wind** — columns for the mean, a paler cap for the gust, arrows above.
//! * **Sea** — the significant height as a filled curve, with its direction.
//! * **Set** — the surface current, as a colour strip with arrows on it.
//! * **Rain** — bars, because rain is a thing that happens in an hour.
//! * **Tide** — the water itself: a curve, the turns marked and labelled, and
//!   a dot riding it at the cursor.
//!
//! Colour is [`super::spectrum`], shared with the chart overlay so a colour
//! means one number on both. Night is shaded from the day's real sunrise and
//! sunset, and midnight draws a divider with the day's name — a passage is
//! planned in days and tides, not in hours since now.

use egui::{Align2, Color32, Context, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2};

use super::spectrum;
use super::ui::UiAction;
use super::ui_batch::Batch;
use crate::nav::pointfx::{self, PointForecast, Sample};

/// Time spans the axis offers. Twelve hours is "this watch"; three days is
/// "this passage".
const SPANS: [(i64, &str); 4] = [(12, "12h"), (24, "24h"), (48, "2d"), (72, "3d")];

/// The gutter down the left where the lanes are named.
const GUTTER: f32 = 58.0;
/// Heights of each lane, and of the axis under them. The wind and sea lanes
/// each carry a strip of direction arrows across the top, so they need the
/// height of that strip on top of the height of their plot.
const H_WIND: f32 = 56.0;
const H_WAVE: f32 = 50.0;
const H_CURRENT: f32 = 30.0;
const H_RAIN: f32 = 20.0;
const H_TIDE: f32 = 58.0;
/// The lanes at their full size, in drawing order.
const LANE_TALL: [f32; 5] = [H_WIND, H_WAVE, H_CURRENT, H_RAIN, H_TIDE];
/// …and squeezed as far as they may go.
///
/// A 7-inch Pi panel is 800×480, and between the menu bar and the instrument
/// strip there is not room for the lanes at their full height. egui's answer
/// to content that overflows a panel is to clip it, and what got clipped was
/// the bottom: the hour axis, which is the one thing that makes five stacked
/// curves mean anything. So the lanes give way instead — proportionally,
/// down to these floors — and the axis keeps its height whatever happens.
const LANE_SHORT: [f32; 5] = [
    ARROW_STRIP + 16.0,
    ARROW_STRIP + 14.0,
    18.0,
    12.0,
    34.0,
];
/// The strip at the top of a lane reserved for direction arrows.
const ARROW_STRIP: f32 = 15.0;
const H_AXIS: f32 = 20.0;
/// Space between one lane and the next. Generous on purpose: at seven points
/// the sea lane's arrows sat close enough under the wind columns to read as
/// part of them.
const LANE_GAP: f32 = 11.0;

/// The sheet's own state. Runtime only, like the wind field it replaced: a
/// forecast belongs to a place and a moment, and a plotter that silently
/// redisplayed yesterday's tide at boot would be worse than one that asks.
pub struct SheetView {
    pub show: bool,
    /// Collapsed shows only the header and the readings at the cursor, which
    /// is all a helm wants underway; expanded shows the lanes.
    pub expanded: bool,
    /// Where the cursor is. `None` rides the present moment, which is what it
    /// should do until somebody drags it.
    pub cursor_ms: Option<i64>,
    /// Left edge of the visible window. `None` follows the present.
    pub window_start_ms: Option<i64>,
    pub span_hours: i64,
    /// Where the point forecast was taken, lat/lon.
    pub anchor: Option<[f64; 2]>,
    pub place: String,
    pub source: String,
    pub busy: bool,
    pub status: String,
    pub data: Option<Box<PointForecast>>,
    /// Draw the surface current over the chart as arrows. The wind field's
    /// own switch lives with the field, in [`crate::render::ui::WindView`];
    /// the sheet only offers the button.
    pub current_field: bool,
}

impl Default for SheetView {
    fn default() -> Self {
        Self {
            show: false,
            expanded: true,
            cursor_ms: None,
            window_start_ms: None,
            span_hours: 24,
            anchor: None,
            place: String::new(),
            source: String::new(),
            busy: false,
            status: String::new(),
            data: None,
            current_field: false,
        }
    }
}

impl SheetView {
    /// The instant everything is showing, clamped into the forecast.
    ///
    /// One function, called by the sheet and by the chart overlay both, so
    /// the two cannot drift apart — which is the whole point of the design.
    pub fn cursor(&self, now_ms: i64) -> i64 {
        let t = self.cursor_ms.unwrap_or(now_ms);
        match self.data.as_ref().and_then(|d| d.span()) {
            Some((a, b)) => t.clamp(a, b),
            None => t,
        }
    }

    /// Is the cursor riding the present rather than parked?
    pub fn on_now(&self) -> bool {
        self.cursor_ms.is_none()
    }

    /// The visible time window, clamped to whatever the forecast covers.
    fn window(&self, now_ms: i64) -> (i64, i64) {
        let span = self.span_hours * 3_600_000;
        // A sixth of the window behind the present: enough to see what the
        // weather has just been doing, which is how you judge a forecast.
        let mut start = self.window_start_ms.unwrap_or(now_ms - span / 6);
        if let Some((a, b)) = self.data.as_ref().and_then(|d| d.span()) {
            start = start.clamp(a, (b - span).max(a));
        }
        (start, start + span)
    }
}

/// Draw the sheet. Returns nothing; everything it wants done goes into
/// `actions`, like every other panel here.
pub fn sheet(
    ctx: &Context,
    view: &mut SheetView,
    field: &crate::render::ui::WindView,
    units: &crate::signalk::UnitPrefs,
    actions: &mut Vec<UiAction>,
) {
    if !view.show {
        return;
    }
    let dark = ctx.style().visuals.dark_mode;
    let frame = egui::Frame {
        // Translucent, so the chart still reads through the sheet's edge and
        // the sheet reads as something laid *over* the water rather than a
        // slab of interface bolted to the bottom of the screen.
        fill: if dark {
            Color32::from_rgba_unmultiplied(18, 22, 28, 236)
        } else {
            Color32::from_rgba_unmultiplied(248, 249, 251, 238)
        },
        inner_margin: egui::Margin::symmetric(10.0, 8.0),
        rounding: egui::Rounding {
            nw: 12.0,
            ne: 12.0,
            sw: 0.0,
            se: 0.0,
        },
        stroke: Stroke::new(
            1.0_f32,
            if dark {
                Color32::from_rgb(52, 60, 70)
            } else {
                Color32::from_rgb(206, 212, 220)
            },
        ),
        ..Default::default()
    };

    egui::TopBottomPanel::bottom("weather_sheet")
        .frame(frame)
        // Its height is the lanes' height; there is nothing to drag, and the
        // frame already draws its own edge.
        .resizable(false)
        .show_separator_line(false)
        .show(ctx, |ui| {
            let now = chrono::Utc::now().timestamp_millis();
            header(ui, view, field, now, actions);
            let sample = view
                .data
                .as_ref()
                .map(|d| d.at(view.cursor(now)))
                .unwrap_or_default();
            readout(ui, view, &sample, now, units);
            if view.expanded {
                if view.data.is_some() {
                    ui.add_space(4.0);
                    lanes(ui, view, now);
                } else if !view.busy {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(if view.status.is_empty() {
                            "No forecast here yet — press Here."
                        } else {
                            &view.status
                        })
                        .small()
                        .weak(),
                    );
                    ui.add_space(4.0);
                }
            }
        });
}

/// Title, place, where in time we are looking, and the controls.
fn header(
    ui: &mut egui::Ui,
    view: &mut SheetView,
    field: &crate::render::ui::WindView,
    now: i64,
    actions: &mut Vec<UiAction>,
) {
    // The controls are placed *first*, from the right, and the title and place
    // then get whatever is left. Laid out the other way round — title first,
    // controls right-aligned after — egui hands the right-hand group the whole
    // remaining rectangle whether or not the left side has already eaten it,
    // and on a 7-inch panel the place, the field's status line and the span
    // buttons all printed on top of one another. Reserving the right side
    // first is the only ordering in which the leftover is honest.
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            controls(ui, view, field, now, actions);
            // Back to reading order for what is left of the row.
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                title(ui, view, field);
            });
        });
    });
}

/// The collapse triangle, the name, the place, and whatever the fetches are
/// saying. Given a bounded width, so every part of it must be able to shrink.
fn title(ui: &mut egui::Ui, view: &mut SheetView, field: &crate::render::ui::WindView) {
    {
        // Painted, not typed. egui's bundled fonts carry ◀ and ▶ but not the
        // downward and small triangles, so every chevron character tried here
        // drew as an empty box; a shape cannot be missing from a font.
        let (tri, tri_resp) = ui.allocate_exact_size(Vec2::new(18.0, 18.0), Sense::click());
        let c = tri.center();
        let ink = ui.visuals().text_color();
        ui.painter().add(egui::Shape::convex_polygon(
            if view.expanded {
                vec![
                    Pos2::new(c.x - 5.0, c.y - 3.0),
                    Pos2::new(c.x + 5.0, c.y - 3.0),
                    Pos2::new(c.x, c.y + 4.0),
                ]
            } else {
                vec![
                    Pos2::new(c.x - 3.0, c.y - 5.0),
                    Pos2::new(c.x + 4.0, c.y),
                    Pos2::new(c.x - 3.0, c.y + 5.0),
                ]
            },
            ink,
            Stroke::NONE,
        ));
        if tri_resp
            .on_hover_text(if view.expanded {
                "Collapse to the readings"
            } else {
                "Show the lanes"
            })
            .clicked()
        {
            view.expanded = !view.expanded;
        }
        ui.label(RichText::new("Weather").strong());
        // Truncated rather than wrapped: the header is one row, and a place
        // that grew it a line pushed the axis off the bottom of the sheet.
        if view.place.is_empty() {
            ui.label(RichText::new("nowhere yet").weak().small());
        } else {
            ui.add(egui::Label::new(RichText::new(&view.place).small()).truncate())
                .on_hover_text(&view.source);
        }
        if view.busy || field.busy {
            ui.spinner();
        }
        // The field's own progress and complaints belong here too: it is the
        // other half of the same weather, and it had nowhere else to say so
        // once its window went away.
        if field.busy || !field.status.is_empty() {
            ui.add(egui::Label::new(RichText::new(&field.status).small().weak()).truncate())
                .on_hover_text(&field.source);
        }
        // The sheet's own news — a failed current fetch, a point with no sea
        // data — once there are lanes to show. Before, it only ever appeared
        // in the empty sheet, so with a forecast loaded it was never seen.
        if view.data.is_some() && !view.status.is_empty() {
            ui.add(egui::Label::new(RichText::new(&view.status).small().weak()).truncate())
                .on_hover_text(&view.status);
        }
    }
}

/// Everything on the right of the header, in right-to-left order.
fn controls(
    ui: &mut egui::Ui,
    view: &mut SheetView,
    field: &crate::render::ui::WindView,
    now: i64,
    actions: &mut Vec<UiAction>,
) {
    {
        {
            if ui
                .add(egui::Button::new("×").frame(false))
                .on_hover_text("Hide the weather sheet")
                .clicked()
            {
                view.show = false;
            }
            ui.separator();
            if ui
                .add_enabled(
                    !view.busy && !field.busy,
                    egui::Button::new("Here").small(),
                )
                .on_hover_text(
                    "Take the whole forecast for what is on screen now — \
                     the point, and whichever fields are switched on",
                )
                .clicked()
            {
                actions.push(UiAction::WeatherRefreshHere);
            }
            if ui
                .selectable_label(field.show, "Barbs")
                .on_hover_text("Draw the wind field over the chart as barbs")
                .clicked()
            {
                actions.push(UiAction::WindToggle);
            }
            if ui
                .selectable_label(field.fill, "Fill")
                .on_hover_text("Wash the chart in the wind's own colour")
                .clicked()
            {
                actions.push(UiAction::WindFillToggle);
            }
            if ui
                .selectable_label(view.current_field, "Set")
                .on_hover_text("Draw the surface current over the chart")
                .clicked()
            {
                actions.push(UiAction::CurrentFieldToggle);
            }
            ui.separator();

            // The span picker, then the pan arrows: what you can see, then
            // where you are looking.
            for (hours, label) in SPANS.iter().rev() {
                if ui
                    .selectable_label(view.span_hours == *hours, *label)
                    .clicked()
                {
                    view.span_hours = *hours;
                }
            }
            let step = view.span_hours * 3_600_000 / 2;
            if ui.small_button("▶").on_hover_text("Later").clicked() {
                let (start, _) = view.window(now);
                view.window_start_ms = Some(start + step);
            }
            if ui
                .add_enabled(!view.on_now(), egui::Button::new("Now").small())
                .on_hover_text("Put the cursor back on the present")
                .clicked()
            {
                view.cursor_ms = None;
                view.window_start_ms = None;
            }
            if ui.small_button("◀").on_hover_text("Earlier").clicked() {
                let (start, _) = view.window(now);
                view.window_start_ms = Some(start - step);
            }
        }
    }
}

/// The readings at the cursor: the line a helm reads without expanding
/// anything. A dot in the lane's own spectrum colour ties each number to its
/// lane below.
fn readout(
    ui: &mut egui::Ui,
    view: &SheetView,
    s: &Sample,
    now: i64,
    units: &crate::signalk::UnitPrefs,
) {
    // In the speed unit the instruments use. The barbs and lanes stay in
    // knots — a barb's feathers are knots by definition — and say so.
    let speed = |kt: f32, decimals: usize| {
        let kt = kt as f64;
        let r = crate::signalk::Quantity::Speed.format(kt * crate::geo::METRES_PER_NM / 3600.0, units);
        let v: f64 = r.value.parse().unwrap_or(kt);
        format!("{v:.decimals$} {}", r.unit)
    };
    ui.horizontal_wrapped(|ui| {
        let t = view.cursor(now);
        let ahead = (t - now) as f64 / 3_600_000.0;
        ui.label(
            RichText::new(format!(
                "{} {}",
                pointfx::local_day(t),
                pointfx::local_hhmm(t)
            ))
            .strong(),
        );
        ui.label(
            RichText::new(if view.on_now() || ahead.abs() < 0.05 {
                "now".to_string()
            } else {
                format!("{}{:.0} h", if ahead < 0.0 { "-" } else { "+" }, ahead.abs())
            })
            .small()
            .weak(),
        );
        ui.separator();

        let mut any = false;
        if let (Some(kt), Some(from)) = (s.wind_kt, s.wind_from_deg) {
            any = true;
            let gust = s
                .gust_kt
                .filter(|g| *g > kt + 1.0)
                .map(|g| format!(" gusts {}", speed(g, 0)))
                .unwrap_or_default();
            chip(
                ui,
                spectrum::WIND_KT.at(kt),
                &format!("{}{gust} {}", speed(kt, 0), compass(from)),
                "Wind: mean, gust, and the point it blows from",
            );
        }
        if let Some(m) = s.wave_m {
            any = true;
            let period = s
                .wave_period_s
                .map(|p| format!(" {p:.0} s"))
                .unwrap_or_default();
            chip(
                ui,
                spectrum::WAVE_M.at(m),
                &format!("{m:.1} m{period}"),
                "Sea: significant wave height and period",
            );
        }
        if let (Some(kt), Some(to)) = (s.current_kt, s.current_to_deg) {
            any = true;
            chip(
                ui,
                spectrum::CURRENT_KT.at(kt),
                &format!("{} to {}", speed(kt, 1), compass(to)),
                "Set and drift: where the water is going, not where it comes from",
            );
        }
        if let Some(mm) = s.rain_mm {
            if mm >= 0.05 {
                any = true;
                chip(
                    ui,
                    spectrum::RAIN_MM.at(mm),
                    &format!("{mm:.1} mm/h"),
                    "Precipitation in this hour",
                );
            }
        }
        if let Some(m) = s.tide_m {
            any = true;
            // Slack water printed as "-0.00 m", which reads as a fault in the
            // display rather than as still water.
            let m = if m.abs() < 0.005 { 0.0 } else { m };
            chip(
                ui,
                Color32::from_rgb(70, 150, 200),
                &format!("{m:+.2} m"),
                "Sea level against mean sea level — not a height above chart datum",
            );
        }
        if !any && !view.busy {
            ui.label(RichText::new("-").weak());
        }
    });
}

fn chip(ui: &mut egui::Ui, colour: Color32, text: &str, hover: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(9.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, colour);
        ui.label(RichText::new(text).small());
    })
    .response
    .on_hover_text(hover);
}

/// The lanes, all sharing one axis and one cursor.
fn lanes(ui: &mut egui::Ui, view: &mut SheetView, now: i64) {
    if view.data.is_none() {
        return;
    }
    let (t0, t1) = view.window(now);
    let heights = lane_heights(ui.available_height());
    let total = heights.iter().sum::<f32>() + H_AXIS + LANE_GAP * 5.0;
    let width = ui.available_width().max(GUTTER + 80.0);
    let (resp, painter) = ui.allocate_painter(Vec2::new(width, total), Sense::click_and_drag());
    let block = resp.rect;
    let plot_left = block.left() + GUTTER;
    let plot_right = block.right();
    let px_per_ms = (plot_right - plot_left) / (t1 - t0).max(1) as f32;
    let ax = Axis {
        t0,
        t1,
        left: plot_left,
        px_per_ms,
        step: arrow_step(px_per_ms),
    };
    let x_of = |t: i64| ax.x_of(t);

    // Dragging anywhere in the block scrubs; a double-click lets go of the
    // cursor and hands it back to the present. Both happen before the
    // forecast is borrowed below, so the mutation and the reading of `view`
    // never overlap.
    if resp.double_clicked() {
        view.cursor_ms = None;
    } else if resp.dragged() || resp.clicked() {
        if let Some(p) = resp.interact_pointer_pos() {
            view.cursor_ms = Some(ax.t_of(p.x.clamp(plot_left, plot_right)));
        }
    }
    let cursor = view.cursor(now);
    let Some(data) = view.data.as_deref() else { return };

    let dark = ui.style().visuals.dark_mode;
    let ink = if dark {
        Color32::from_rgb(196, 206, 218)
    } else {
        Color32::from_rgb(56, 64, 74)
    };
    let faint = ink.gamma_multiply(0.35);
    let plot = Rect::from_min_max(
        Pos2::new(plot_left, block.top()),
        Pos2::new(plot_right, block.bottom()),
    );
    let p = painter.with_clip_rect(plot);

    // Every lane's geometry goes into one mesh. Drawn shape by shape this was
    // several hundred separate tessellations a frame, all of them redone
    // whenever the cursor moved a pixel — which is precisely when the sheet
    // has to feel light. Its place in the paint list is booked now so the
    // labels drawn lane by lane below still land on top of it.
    let mut b = Batch::new(ui.ctx());
    b.reserve(400);
    let slot = Batch::slot(&p);

    // Night first, under everything: the shading is background, not data.
    let lanes_bottom = block.bottom() - H_AXIS;
    for (a, z) in night_bands(data) {
        let (a, z) = (a.max(t0), z.min(t1));
        if z <= a {
            continue;
        }
        b.rect(
            Rect::from_min_max(
                Pos2::new(x_of(a), block.top()),
                Pos2::new(x_of(z), lanes_bottom),
            ),
            if dark {
                Color32::from_rgba_unmultiplied(0, 0, 0, 46)
            } else {
                Color32::from_rgba_unmultiplied(40, 60, 90, 20)
            },
        );
    }
    // Then the day dividers, which run the full height so the eye can cut the
    // sheet into days without counting hours.
    for t in midnights(t0, t1) {
        b.line(
            Pos2::new(x_of(t), block.top()),
            Pos2::new(x_of(t), lanes_bottom),
            1.0,
            faint,
        );
    }

    let mut y = block.top();
    let mut lane = |h: f32| {
        let r = Rect::from_min_max(
            Pos2::new(plot_left, y),
            Pos2::new(plot_right, y + h),
        );
        let label = Rect::from_min_max(
            Pos2::new(block.left(), y),
            Pos2::new(plot_left - 4.0, y + h),
        );
        y += h + LANE_GAP;
        (r, label)
    };

    let (wind_r, wind_l) = lane(heights[0]);
    let (wave_r, wave_l) = lane(heights[1]);
    let (cur_r, cur_l) = lane(heights[2]);
    let (rain_r, rain_l) = lane(heights[3]);
    let (tide_r, tide_l) = lane(heights[4]);

    // One line, right-aligned, centred on the lane. Two lines collided with
    // the lane below on the short ones, and stacking a unit under a name was
    // never worth a row of pixels.
    let name = |r: Rect, text: &str| {
        painter.text(
            Pos2::new(r.right(), r.center().y),
            Align2::RIGHT_CENTER,
            text,
            FontId::proportional(10.5),
            ink,
        );
    };
    name(wind_l, "Wind (kn)");
    name(wave_l, "Sea m");
    name(cur_l, "Set (kn)");
    name(rain_l, "Rain mm");
    name(tide_l, "Tide m");

    draw_wind(&mut b, &p, wind_r, data, ax, ink, faint);
    draw_wave(&mut b, &p, wave_r, data, ax, faint);
    draw_current(&mut b, cur_r, data, ax);
    draw_rain(&mut b, rain_r, data, ax);
    draw_tide(&mut b, &p, tide_r, data, ax, ink, faint);
    draw_axis(
        &mut b,
        &painter,
        Rect::from_min_max(
            Pos2::new(plot_left, lanes_bottom),
            Pos2::new(plot_right, block.bottom()),
        ),
        ax,
        ink,
        faint,
    );
    b.paint_at(&p, slot);

    // The present, then the cursor over it: a thin bright line the eye can
    // find without it competing with the data.
    if now >= t0 && now <= t1 {
        p.line_segment(
            [
                Pos2::new(x_of(now), block.top()),
                Pos2::new(x_of(now), lanes_bottom),
            ],
            Stroke::new(1.0_f32, Color32::from_rgb(120, 190, 130).gamma_multiply(0.8)),
        );
    }
    if cursor >= t0 && cursor <= t1 {
        let x = x_of(cursor);
        let accent = if dark {
            Color32::from_rgb(255, 214, 110)
        } else {
            Color32::from_rgb(190, 110, 20)
        };
        p.line_segment(
            [Pos2::new(x, block.top()), Pos2::new(x, lanes_bottom)],
            Stroke::new(1.6_f32, accent),
        );
        // The dot riding the tide is the sheet's signature: the one place
        // where the cursor touches the data instead of crossing it.
        if let Some((lo, hi)) = data.tide_range() {
            if let Some(m) = data.at(cursor).tide_m {
                let yv = tide_y(tide_r, lo, hi, m);
                p.circle(
                    Pos2::new(x, yv),
                    4.0,
                    accent,
                    Stroke::new(1.5_f32, Color32::from_rgba_unmultiplied(0, 0, 0, 90)),
                );
            }
        }
    }
}

/// How tall each lane may be, given the room the panel actually has.
///
/// One factor moves every lane together between its floor and its full
/// height, so the sheet keeps its proportions as it shrinks rather than
/// collapsing one lane at a time. Room for everything means everything at
/// full size; the arithmetic below only ever runs on a small screen.
fn lane_heights(available: f32) -> [f32; 5] {
    let fixed = H_AXIS + LANE_GAP * 5.0;
    let tall: f32 = LANE_TALL.iter().sum();
    let short: f32 = LANE_SHORT.iter().sum();
    // A panel that has not been measured yet reports nothing useful; full
    // size is the right guess, and the next frame corrects it.
    if !available.is_finite() || available <= 0.0 || available >= tall + fixed {
        return LANE_TALL;
    }
    let t = ((available - fixed - short) / (tall - short)).clamp(0.0, 1.0);
    std::array::from_fn(|i| LANE_SHORT[i] + (LANE_TALL[i] - LANE_SHORT[i]) * t)
}

/// The one time axis every lane draws against.
///
/// Passed around whole rather than as a handful of loose numbers and a
/// closure: five lanes and the axis strip all map the same instants to the
/// same pixels, and the moment one of them is handed a slightly different
/// `t0` the cursor stops lining up with the data underneath it.
#[derive(Clone, Copy)]
struct Axis {
    /// The window, ms UTC.
    t0: i64,
    t1: i64,
    /// The left edge of the plotting area, in points.
    left: f32,
    px_per_ms: f32,
    /// Hours between direction arrows, so they never collide.
    step: i64,
}

impl Axis {
    fn x_of(&self, t: i64) -> f32 {
        self.left + (t - self.t0) as f32 * self.px_per_ms
    }

    fn t_of(&self, x: f32) -> i64 {
        self.t0 + ((x - self.left) / self.px_per_ms.max(1e-9)) as i64
    }

    fn px_per_hour(&self) -> f32 {
        self.px_per_ms * 3_600_000.0
    }

    /// The hours in the window that fall on a multiple of `step` hours UTC.
    fn arrow_hours(&self) -> impl Iterator<Item = i64> {
        stepped_hours(self.t0, self.t1, self.step)
    }
}

/// How many hours between direction arrows, so they never collide.
fn arrow_step(px_per_ms: f32) -> i64 {
    let px_per_hour = px_per_ms * 3_600_000.0;
    for step in [1i64, 2, 3, 6, 12] {
        if px_per_hour * step as f32 >= 26.0 {
            return step;
        }
    }
    24
}

/// The hours in the window that fall on a multiple of `step` hours UTC —
/// a stable grid that does not shift as the window pans.
fn stepped_hours(t0: i64, t1: i64, step: i64) -> impl Iterator<Item = i64> {
    let h = 3_600_000i64;
    // A zero step would divide by zero building the grid; callers never send
    // one, and this is cheaper than trusting them.
    let span = step.max(1) * h;
    let first = (t0.div_euclid(span) + 1) * span;
    (0i64..)
        .map(move |i| first + i * span)
        .take_while(move |t| *t <= t1)
}

/// Local midnights inside the window.
fn midnights(t0: i64, t1: i64) -> Vec<i64> {
    use chrono::TimeZone as _;
    let mut out = Vec::new();
    let mut day = local_of(t0).date_naive();
    // Start a day early so a window opening just after midnight still finds
    // its own divider.
    day = day.pred_opt().unwrap_or(day);
    // A window is at most three days wide, so five is room to spare and a
    // guarantee against looping on a date arithmetic surprise.
    for _ in 0..5 {
        if let Some(midnight) = day.and_hms_opt(0, 0, 0) {
            // Ambiguous or skipped local midnights — a clock change — get no
            // divider rather than an arbitrary one.
            if let Some(t) = chrono::Local.from_local_datetime(&midnight).single() {
                let ms = t.timestamp_millis();
                if ms > t1 {
                    break;
                }
                if ms >= t0 {
                    out.push(ms);
                }
            }
        }
        let Some(next) = day.succ_opt() else { break };
        day = next;
    }
    out
}

fn local_of(ms: i64) -> chrono::DateTime<chrono::Local> {
    use chrono::TimeZone as _;
    chrono::Utc
        .timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(chrono::Utc::now)
        .with_timezone(&chrono::Local)
}

/// The stretches of darkness the forecast knows about.
fn night_bands(d: &PointForecast) -> Vec<(i64, i64)> {
    let mut out = Vec::new();
    if d.sun.is_empty() {
        return out;
    }
    out.push((i64::MIN / 4, d.sun[0].sunrise_ms));
    for w in d.sun.windows(2) {
        out.push((w[0].sunset_ms, w[1].sunrise_ms));
    }
    if let Some(last) = d.sun.last() {
        out.push((last.sunset_ms, i64::MAX / 4));
    }
    out
}

/// Round a scale maximum up to something a person would have chosen.
fn nice_max(v: f32, floor: f32) -> f32 {
    let v = v.max(floor);
    for step in [0.5f32, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0] {
        let up = (v / step).ceil() * step;
        if up / step <= 8.0 {
            return up;
        }
    }
    v
}

fn draw_wind(
    b: &mut Batch,
    p: &egui::Painter,
    r: Rect,
    d: &PointForecast,
    ax: Axis,
    ink: Color32,
    faint: Color32,
) {
    let peak = PointForecast::peak(&d.gust_kt)
        .into_iter()
        .chain(PointForecast::peak(&d.wind_kt))
        .fold(0.0f32, f32::max);
    if peak <= 0.0 {
        return;
    }
    let top = nice_max(peak, 12.0);
    // The arrows live in the top strip, the columns below them.
    let bars = Rect::from_min_max(Pos2::new(r.left(), r.top() + ARROW_STRIP), r.max);
    let h_of = |v: f32| bars.height() * (v / top).clamp(0.0, 1.0);

    let hour = 3_600_000i64;
    let bar_w = (ax.px_per_hour() * 0.66).clamp(1.5, 13.0);
    for (i, &t) in d.times.iter().enumerate() {
        if t < ax.t0 - hour || t > ax.t1 + hour {
            continue;
        }
        let Some(kt) = d.wind_kt.get(i).copied().flatten() else { continue };
        let x = ax.x_of(t);
        let colour = spectrum::WIND_KT.at(kt);
        // The gust is drawn first and paler, so the mean column sits inside
        // it: the gap between them *is* the gustiness, at a glance.
        if let Some(g) = d.gust_kt.get(i).copied().flatten() {
            if g > kt {
                b.rect(
                    Rect::from_min_max(
                        Pos2::new(x - bar_w / 2.0, bars.bottom() - h_of(g)),
                        Pos2::new(x + bar_w / 2.0, bars.bottom()),
                    ),
                    spectrum::WIND_KT.at(g).gamma_multiply(0.38),
                );
            }
        }
        b.rect(
            Rect::from_min_max(
                Pos2::new(x - bar_w / 2.0, bars.bottom() - h_of(kt)),
                Pos2::new(x + bar_w / 2.0, bars.bottom()),
            ),
            colour,
        );
    }

    for t in ax.arrow_hours() {
        let s = d.at(t);
        let (Some(kt), Some(from)) = (s.wind_kt, s.wind_from_deg) else {
            continue;
        };
        // The arrow flies with the wind — the way it is going, which is where
        // it will push the boat. The barbs on the chart point the other way
        // because that is the barb's own convention, and both are labelled.
        arrow(
            b,
            Pos2::new(ax.x_of(t), r.top() + 7.0),
            from + 180.0,
            11.0,
            spectrum::WIND_KT.at(kt),
        );
    }

    b.line(
        Pos2::new(r.left(), bars.top()),
        Pos2::new(r.right(), bars.top()),
        1.0,
        faint.gamma_multiply(0.5),
    );
    p.text(
        Pos2::new(r.left() + 3.0, bars.top() + 1.0),
        Align2::LEFT_TOP,
        format!("{top:.0}"),
        FontId::proportional(9.0),
        ink.gamma_multiply(0.7),
    );
}

fn draw_wave(
    b: &mut Batch,
    p: &egui::Painter,
    r: Rect,
    d: &PointForecast,
    ax: Axis,
    faint: Color32,
) {
    let Some(peak) = PointForecast::peak(&d.wave_m) else { return };
    if peak <= 0.0 {
        return;
    }
    let top = nice_max(peak, 1.0);
    let plot = Rect::from_min_max(Pos2::new(r.left(), r.top() + ARROW_STRIP), r.max);
    let base = plot.bottom();
    let y_of = |m: f32| base - plot.height() * (m / top).clamp(0.0, 1.0);

    // Filled as a run of trapezoids rather than one polygon: the curve is not
    // convex, and egui fills only convex shapes correctly.
    for w in 0..d.times.len().saturating_sub(1) {
        let (ta, tb) = (d.times[w], d.times[w + 1]);
        if tb < ax.t0 || ta > ax.t1 {
            continue;
        }
        let (Some(lo), Some(hi)) = (
            d.wave_m.get(w).copied().flatten(),
            d.wave_m.get(w + 1).copied().flatten(),
        ) else {
            continue;
        };
        let (xa, xb) = (ax.x_of(ta), ax.x_of(tb));
        let (ya, yb) = (y_of(lo), y_of(hi));
        let colour = spectrum::WAVE_M.at((lo + hi) * 0.5);
        b.quad(
            Pos2::new(xa, ya),
            Pos2::new(xb, yb),
            Pos2::new(xb, base),
            Pos2::new(xa, base),
            colour.gamma_multiply(0.55),
        );
        b.line(Pos2::new(xa, ya), Pos2::new(xb, yb), 1.6, colour);
    }

    for t in ax.arrow_hours() {
        let s = d.at(t);
        let (Some(m), Some(from)) = (s.wave_m, s.wave_from_deg) else {
            continue;
        };
        arrow(
            b,
            Pos2::new(ax.x_of(t), r.top() + ARROW_STRIP * 0.5),
            from + 180.0,
            9.0,
            spectrum::WAVE_M.at(m),
        );
    }
    b.line(
        Pos2::new(r.left(), plot.top()),
        Pos2::new(r.right(), plot.top()),
        1.0,
        faint.gamma_multiply(0.5),
    );
    p.text(
        Pos2::new(r.left() + 3.0, plot.top() + 1.0),
        Align2::LEFT_TOP,
        format!("{top:.1}"),
        FontId::proportional(9.0),
        faint,
    );
}

fn draw_current(b: &mut Batch, r: Rect, d: &PointForecast, ax: Axis) {
    if PointForecast::peak(&d.current_kt).is_none() {
        return;
    }
    // A continuous strip of colour, one thin rect per hour, with the arrows
    // riding on it. Orca colours whole columns this way and it reads far
    // faster than a line chart for something whose *direction* is the point.
    let strip = Rect::from_min_max(Pos2::new(r.left(), r.bottom() - 7.0), r.max);
    let hour = 3_600_000i64;
    for (i, &t) in d.times.iter().enumerate() {
        if t < ax.t0 - hour || t > ax.t1 + hour {
            continue;
        }
        let Some(kt) = d.current_kt.get(i).copied().flatten() else { continue };
        let (left, right) = (ax.x_of(t - hour / 2), ax.x_of(t + hour / 2));
        b.rect(
            Rect::from_min_max(
                Pos2::new(left, strip.top()),
                Pos2::new(right, strip.bottom()),
            ),
            spectrum::CURRENT_KT.at(kt),
        );
    }
    for t in ax.arrow_hours() {
        let s = d.at(t);
        let (Some(kt), Some(to)) = (s.current_kt, s.current_to_deg) else {
            continue;
        };
        // Set is quoted the way the water goes, so the arrow needs no turning.
        arrow(
            b,
            Pos2::new(ax.x_of(t), r.top() + 7.0),
            to,
            10.0,
            spectrum::CURRENT_KT.at(kt),
        );
    }
}

fn draw_rain(b: &mut Batch, r: Rect, d: &PointForecast, ax: Axis) {
    let Some(peak) = PointForecast::peak(&d.rain_mm) else { return };
    if peak < 0.05 {
        return;
    }
    // Square-rooted: a shower and a downpour differ by two orders of
    // magnitude, and a linear bar would hide every shower there ever was.
    let top = nice_max(peak, 1.0).sqrt();
    let hour = 3_600_000i64;
    // Narrower than the other lanes' columns: this lane is short, and at the
    // same width the bars came out square and read as blocks rather than as
    // an amount of rain.
    let w = (ax.px_per_hour() * 0.5).clamp(1.5, 8.0);
    for (i, &t) in d.times.iter().enumerate() {
        if t < ax.t0 - hour || t > ax.t1 + hour {
            continue;
        }
        let Some(mm) = d.rain_mm.get(i).copied().flatten() else { continue };
        if mm < 0.05 {
            continue;
        }
        let x = ax.x_of(t);
        let h = r.height() * (mm.sqrt() / top).clamp(0.08, 1.0);
        b.rect(
            Rect::from_min_max(
                Pos2::new(x - w / 2.0, r.bottom() - h),
                Pos2::new(x + w / 2.0, r.bottom()),
            ),
            spectrum::RAIN_MM.at(mm),
        );
    }
}

/// Where a height sits inside the tide lane.
fn tide_y(r: Rect, lo: f32, hi: f32, m: f32) -> f32 {
    // Room above for the high-water labels and below for the low ones —
    // proportional, so a squeezed lane keeps some curve between them.
    let pad = (r.height() * 0.2).clamp(4.0, 12.0);
    let inner = Rect::from_min_max(
        Pos2::new(r.left(), r.top() + pad),
        Pos2::new(r.right(), r.bottom() - pad),
    );
    let span = (hi - lo).max(0.2);
    inner.bottom() - inner.height() * ((m - lo) / span).clamp(0.0, 1.0)
}

fn draw_tide(
    b: &mut Batch,
    p: &egui::Painter,
    r: Rect,
    d: &PointForecast,
    ax: Axis,
    ink: Color32,
    faint: Color32,
) {
    let Some((lo, hi)) = d.tide_range() else { return };
    let water = Color32::from_rgb(70, 150, 200);

    // Mean sea level, dashed, so the sign of the number in the readout has
    // something to mean.
    if lo < 0.0 && hi > 0.0 {
        let y0 = tide_y(r, lo, hi, 0.0);
        let mut x = r.left();
        while x < r.right() {
            b.line(
                Pos2::new(x, y0),
                Pos2::new((x + 4.0).min(r.right()), y0),
                1.0,
                faint,
            );
            x += 8.0;
        }
    }

    for w in 0..d.times.len().saturating_sub(1) {
        let (ta, tb) = (d.times[w], d.times[w + 1]);
        if tb < ax.t0 || ta > ax.t1 {
            continue;
        }
        let (Some(ma), Some(mb)) = (
            d.tide_m.get(w).copied().flatten(),
            d.tide_m.get(w + 1).copied().flatten(),
        ) else {
            continue;
        };
        let (xa, xb) = (ax.x_of(ta), ax.x_of(tb));
        let (ya, yb) = (tide_y(r, lo, hi, ma), tide_y(r, lo, hi, mb));
        b.quad(
            Pos2::new(xa, ya),
            Pos2::new(xb, yb),
            Pos2::new(xb, r.bottom()),
            Pos2::new(xa, r.bottom()),
            water.gamma_multiply(0.22),
        );
        b.line(Pos2::new(xa, ya), Pos2::new(xb, yb), 1.8, water);
    }

    // The turns, which is what anyone actually came to the tide lane for.
    //
    // The dot always goes on; the label only if it has somewhere to go. In a
    // near-tideless place — the Sound, where this is written — the curve is
    // flat, the model turns it every few hours, and the labels printed
    // straight through one another into an unreadable smear. A label that
    // would land on the last one is dropped: the dot still marks the turn,
    // and the cursor still reads the height out in full.
    let mut taken: [f32; 2] = [f32::NEG_INFINITY; 2];
    for e in d.tide_extremes() {
        if e.time_ms < ax.t0 || e.time_ms > ax.t1 {
            continue;
        }
        let x = ax.x_of(e.time_ms);
        let y = tide_y(r, lo, hi, e.height_m);
        b.disc(Pos2::new(x, y), 2.6, water);
        let text = format!(
            "{} {} {:+.1}",
            if e.high { "HW" } else { "LW" },
            pointfx::local_hhmm(e.time_ms),
            e.height_m
        );
        // Measured, not guessed: the width decides both whether the label
        // fits beside its neighbour and how far it must be held off the ends.
        let galley = p.layout_no_wrap(text, FontId::proportional(9.5), ink);
        let half = galley.size().x * 0.5;
        // Held clear of both ends: a turn of the tide near the edge of the
        // window had its label sliced in half by the clip rect, and half a
        // time is worse than none.
        let lx = x.clamp(r.left() + half, (r.right() - half).max(r.left() + half));
        // Highs are labelled above the curve and lows below, so the two rows
        // are policed separately.
        let row = usize::from(e.high);
        if lx - half < taken[row] {
            continue;
        }
        taken[row] = lx + half + 6.0;
        // `galley` draws from a top-left corner, so the anchoring the text
        // helper would have done is done here: highs sit above their dot,
        // lows below theirs.
        let size = galley.size();
        let top_left = if e.high {
            Pos2::new(lx - half, y - 4.0 - size.y)
        } else {
            Pos2::new(lx - half, y + 4.0)
        };
        p.galley(top_left, galley, ink);
    }
}

fn draw_axis(b: &mut Batch, p: &egui::Painter, r: Rect, ax: Axis, ink: Color32, faint: Color32) {
    b.line(
        Pos2::new(r.left(), r.top()),
        Pos2::new(r.right(), r.top()),
        1.0,
        faint,
    );
    // The labels want more room than the arrows do, so the axis picks its own
    // step rather than borrowing the one the lanes use.
    let px_per_hour = ax.px_per_hour().max(0.01);
    let step = [1i64, 2, 3, 6, 12]
        .into_iter()
        .find(|s| px_per_hour * *s as f32 >= 34.0)
        .unwrap_or(24);

    use chrono::Timelike as _;
    for t in stepped_hours(ax.t0, ax.t1, step) {
        let x = ax.x_of(t);
        let local = local_of(t);
        let midnight = local.hour() == 0;
        b.line(
            Pos2::new(x, r.top()),
            Pos2::new(x, r.top() + if midnight { 6.0 } else { 3.0 }),
            1.0,
            faint,
        );
        let (text, colour) = if midnight {
            (pointfx::local_day(t), ink)
        } else {
            (local.format("%H").to_string(), ink.gamma_multiply(0.75))
        };
        p.text(
            Pos2::new(x, r.top() + 7.0),
            Align2::CENTER_TOP,
            text,
            FontId::proportional(if midnight { 10.0 } else { 9.5 }),
            colour,
        );
    }
}

/// A small filled arrow pointing along a true bearing.
fn arrow(b: &mut Batch, at: Pos2, toward_deg: f32, len: f32, colour: Color32) {
    let rad = toward_deg.to_radians();
    // North is up and east is right; screen y grows downwards.
    let dir = Vec2::new(rad.sin(), -rad.cos());
    let side = Vec2::new(-dir.y, dir.x);
    let tip = at + dir * (len * 0.5);
    let tail = at - dir * (len * 0.5);
    b.line(tail, tip - dir * 2.5, 1.5, colour);
    b.convex(
        &[
            tip,
            tip - dir * 4.0 + side * 2.6,
            tip - dir * 4.0 - side * 2.6,
        ],
        colour,
    );
}

/// A bearing as a point of the compass, which is how a forecast is spoken.
fn compass(deg: f32) -> &'static str {
    const POINTS: [&str; 16] = [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
        "NW", "NNW",
    ];
    let i = (((deg.rem_euclid(360.0)) / 22.5).round() as usize) % 16;
    POINTS[i]
}

// ─── What the sheet puts on the chart itself ────────────────────────────────

/// One current arrow, already projected by the renderer.
#[derive(Debug, Clone, PartialEq)]
pub struct CurrentArrow {
    /// Logical points, the same space as the barbs and the route overlay.
    pub screen: [f32; 2],
    /// Where the water sets *towards*, degrees true.
    pub to_deg: f32,
    pub kt: f32,
}

/// Draw the surface current over the chart.
///
/// Arrows, not barbs: a current has no Beaufort and no convention of its own,
/// and the one thing a skipper must not get wrong is which way it is taking
/// the boat. An arrow that flies the way the water goes cannot be misread the
/// way a barb's shaft can. Length grows with the set as well as colour, so a
/// foul tide is visible even in a palette the eye has not learned yet.
pub fn draw_current_field(ctx: &Context, arrows: &[CurrentArrow]) {
    if arrows.is_empty() {
        return;
    }
    let painter = ctx.layer_painter(egui::LayerId::background());
    // One mesh for the whole field, like the barbs: see [`super::ui_batch`].
    let mut batch = Batch::new(ctx);
    batch.reserve(arrows.len() * 2);
    for a in arrows {
        // Below a tenth of a knot there is nothing to steer for, and drawing
        // it would carpet the chart in noise.
        if a.kt < 0.1 {
            continue;
        }
        let len = (10.0 + a.kt * 9.0).min(34.0);
        arrow(
            &mut batch,
            Pos2::new(a.screen[0], a.screen[1]),
            a.to_deg,
            len,
            spectrum::CURRENT_KT.at(a.kt),
        );
    }
    batch.paint(&painter);
}

/// Mark where the point forecast was taken.
///
/// Without this the sheet is a set of numbers from nowhere in particular.
/// Orca marks its tide station the same way, and for the same reason: a tide
/// forty miles up the coast is a different tide.
pub fn draw_anchor(ctx: &Context, at: [f32; 2], live: bool) {
    let painter = ctx.layer_painter(egui::LayerId::background());
    let p = Pos2::new(at[0], at[1]);
    let colour = if live {
        Color32::from_rgb(70, 150, 200)
    } else {
        Color32::from_rgb(140, 146, 152)
    };
    painter.circle(
        p,
        7.0,
        Color32::from_rgba_unmultiplied(255, 255, 255, 40),
        Stroke::new(2.0_f32, colour),
    );
    painter.circle_filled(p, 2.2, colour);
    // Two short ticks either side, so the mark cannot be mistaken for a
    // waypoint or a chart object.
    for dx in [-11.0f32, 11.0] {
        painter.line_segment(
            [
                Pos2::new(p.x + dx * 0.72, p.y),
                Pos2::new(p.x + dx, p.y),
            ],
            Stroke::new(2.0_f32, colour),
        );
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3_600_000;

    fn view_with_span(span_hours: i64) -> SheetView {
        SheetView {
            span_hours,
            ..Default::default()
        }
    }

    /// The whole point of the squeeze: on a panel too short for the lanes at
    /// full size, they give way and the axis keeps its room. Before this the
    /// overflow was clipped, and what fell off the bottom was the hour axis.
    #[test]
    fn a_short_panel_squeezes_the_lanes_and_keeps_the_axis() {
        let fixed = H_AXIS + LANE_GAP * 5.0;
        let full: f32 = LANE_TALL.iter().sum::<f32>() + fixed;

        // Room to spare changes nothing.
        assert_eq!(lane_heights(full + 200.0), LANE_TALL);
        assert_eq!(lane_heights(full), LANE_TALL);

        // A 7-inch Pi panel, once the menu bar and instruments have taken
        // theirs: the lanes must fit inside what is left, axis included.
        let cramped = 250.0;
        let h = lane_heights(cramped);
        assert!(
            h.iter().sum::<f32>() + fixed <= cramped + 0.01,
            "lanes still overflow: {:?}",
            h
        );
        // Every lane gives way, none collapses, and the order holds.
        for i in 0..5 {
            assert!(h[i] >= LANE_SHORT[i] - 0.01, "lane {i} below its floor");
            assert!(h[i] <= LANE_TALL[i] + 0.01, "lane {i} above full size");
        }
        // Squeezed further, they stop at their floors rather than vanishing.
        assert_eq!(lane_heights(10.0), LANE_SHORT);
        // An unmeasured panel guesses full size rather than nothing.
        assert_eq!(lane_heights(0.0), LANE_TALL);
        assert_eq!(lane_heights(f32::NAN), LANE_TALL);
    }

    #[test]
    fn the_cursor_rides_now_until_it_is_dragged() {
        let mut v = view_with_span(24);
        assert!(v.on_now());
        assert_eq!(v.cursor(1_000), 1_000);
        v.cursor_ms = Some(5_000);
        assert!(!v.on_now());
        assert_eq!(v.cursor(1_000), 5_000);
    }

    /// The cursor is what the chart overlay reads. Letting it wander outside
    /// the forecast would make the barbs show an hour nobody has data for.
    #[test]
    fn the_cursor_is_held_inside_the_forecast() {
        let mut v = view_with_span(24);
        let mut d = PointForecast::default();
        d.times = vec![10 * HOUR, 20 * HOUR];
        v.data = Some(Box::new(d));
        v.cursor_ms = Some(0);
        assert_eq!(v.cursor(0), 10 * HOUR);
        v.cursor_ms = Some(99 * HOUR);
        assert_eq!(v.cursor(0), 20 * HOUR);
    }

    #[test]
    fn the_window_opens_a_little_way_behind_the_present() {
        let v = view_with_span(24);
        let now = 100 * HOUR;
        let (a, b) = v.window(now);
        assert_eq!(b - a, 24 * HOUR);
        assert!(a < now && now < b, "the present must be in view");
        assert_eq!(a, now - 4 * HOUR);
    }

    #[test]
    fn the_window_cannot_pan_off_the_end_of_the_forecast() {
        let mut v = view_with_span(24);
        let mut d = PointForecast::default();
        d.times = vec![0, 48 * HOUR];
        v.data = Some(Box::new(d));
        v.window_start_ms = Some(-500 * HOUR);
        assert_eq!(v.window(0).0, 0);
        v.window_start_ms = Some(500 * HOUR);
        assert_eq!(v.window(0), (24 * HOUR, 48 * HOUR));
    }

    /// A forecast shorter than the chosen span must not produce a window that
    /// starts before it — that used to be a negative-width plot.
    #[test]
    fn a_short_forecast_still_yields_a_sane_window() {
        let mut v = view_with_span(72);
        let mut d = PointForecast::default();
        d.times = vec![0, 6 * HOUR];
        v.data = Some(Box::new(d));
        v.window_start_ms = Some(3 * HOUR);
        let (a, b) = v.window(0);
        assert_eq!(a, 0);
        assert_eq!(b - a, 72 * HOUR);
    }

    #[test]
    fn arrows_thin_out_as_the_span_widens() {
        // 24 h across 900 px: about 37 px an hour, so every hour.
        assert_eq!(arrow_step(900.0 / (24.0 * 3_600_000.0)), 1);
        // 72 h across the same width: about 12 px an hour, so every third.
        assert_eq!(arrow_step(900.0 / (72.0 * 3_600_000.0)), 3);
        // A very narrow sheet must still not overlap its arrows.
        assert!(arrow_step(120.0 / (72.0 * 3_600_000.0)) >= 6);
    }

    #[test]
    fn the_hour_grid_is_anchored_to_the_clock_not_to_the_window() {
        // Whatever the window's edge, ticks land on multiples of the step.
        for offset in [0i64, 7 * 60_000, 59 * 60_000] {
            let ticks: Vec<i64> =
                stepped_hours(offset, offset + 12 * HOUR, 3).collect();
            assert!(!ticks.is_empty());
            for t in ticks {
                assert_eq!(t % (3 * HOUR), 0, "tick {t} is off the grid");
            }
        }
    }

    #[test]
    fn a_scale_top_is_a_number_a_person_would_pick() {
        assert_eq!(nice_max(11.0, 12.0), 12.0, "the floor holds a calm day open");
        assert_eq!(nice_max(17.0, 12.0), 20.0);
        assert_eq!(nice_max(0.8, 1.0), 1.0);
        assert_eq!(nice_max(2.2, 1.0), 2.5);
        assert!(nice_max(43.0, 12.0) >= 43.0);
    }

    #[test]
    fn night_is_every_stretch_between_a_sunset_and_the_next_sunrise() {
        let mut d = PointForecast::default();
        d.sun = vec![
            pointfx::SunDay { sunrise_ms: 6 * HOUR, sunset_ms: 20 * HOUR },
            pointfx::SunDay { sunrise_ms: 30 * HOUR, sunset_ms: 44 * HOUR },
        ];
        let bands = night_bands(&d);
        assert_eq!(bands.len(), 3);
        assert_eq!(bands[1], (20 * HOUR, 30 * HOUR));
        assert!(bands[0].1 == 6 * HOUR);
        assert!(bands[2].0 == 44 * HOUR);
    }

    #[test]
    fn a_forecast_without_sun_times_shades_nothing() {
        assert!(night_bands(&PointForecast::default()).is_empty());
    }

    #[test]
    fn the_tide_lane_puts_low_water_below_high_water() {
        let r = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(100.0, 58.0));
        let hi_y = tide_y(r, -1.0, 1.0, 1.0);
        let lo_y = tide_y(r, -1.0, 1.0, -1.0);
        assert!(hi_y < lo_y, "high water must draw higher on the screen");
        // And a flat series must not divide by zero into infinity.
        assert!(tide_y(r, 0.5, 0.5, 0.5).is_finite());
    }

    #[test]
    fn a_bearing_reads_as_a_point_of_the_compass() {
        assert_eq!(compass(0.0), "N");
        assert_eq!(compass(359.0), "N");
        assert_eq!(compass(90.0), "E");
        assert_eq!(compass(225.0), "SW");
        assert_eq!(compass(-90.0), "W");
    }
}
