//! Safety: the first-start notice, alarms, man overboard, the anchor watch
//! and the track.
//!
//! Everything here is drawn over the chart or in its own window; the
//! renderer, which owns the camera and the boat, hands over positions already
//! projected and alarms already decided (see `RenderState::update_watch`).

use egui::{Align2, Color32, Context, FontId, Pos2, RichText, Stroke, Vec2};

use super::ui::UiAction;

/// Which version of the notice was last accepted. Raise it when the notice
/// says something new, and everyone sees it again.
pub const NOTICE_VERSION: u32 = 1;

const MOB_RED: Color32 = Color32::from_rgb(205, 30, 30);

/// The safety settings, and the window's own state.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SafetyView {
    /// [`NOTICE_VERSION`] once the notice has been read and accepted.
    pub notice_accepted: u32,
    /// Alarms make a sound. Off is for the dock, not for a passage.
    pub sound: bool,
    pub depth_alarm: bool,
    /// Depth under the boat, as the sounder reports it (below the keel if
    /// the boat sends that, else below the transducer).
    pub depth_alarm_m: f32,
    pub anchor_radius_m: f32,

    #[serde(skip)]
    pub open: bool,
    #[serde(skip)]
    pub guide_open: bool,
    /// The notice, shown again from the Safety window.
    #[serde(skip)]
    pub notice_open: bool,
    /// "Clear MOB" asks twice.
    #[serde(skip)]
    pub mob_clear_armed: bool,
}

impl Default for SafetyView {
    fn default() -> Self {
        Self {
            notice_accepted: 0,
            sound: true,
            depth_alarm: false,
            depth_alarm_m: 2.0,
            anchor_radius_m: 40.0,
            open: false,
            guide_open: false,
            notice_open: false,
            mob_clear_armed: false,
        }
    }
}

/// One raised alarm, for the banner.
#[derive(Debug, Clone)]
pub struct AlarmLine {
    pub title: &'static str,
    pub message: String,
    pub acknowledged: bool,
}

/// The man overboard mark, projected.
#[derive(Debug, Clone)]
pub struct MobView {
    /// On screen, logical points; `None` when marked without a fix.
    pub screen: Option<[f32; 2]>,
    pub bearing_deg: Option<f64>,
    pub distance_m: Option<f64>,
    pub elapsed_s: u64,
    pub own_screen: Option<[f32; 2]>,
}

/// The anchor watch, projected.
#[derive(Debug, Clone)]
pub struct AnchorView {
    pub screen: [f32; 2],
    pub radius_px: f32,
    pub radius_m: f32,
    pub distance_m: Option<f64>,
}

/// A track on the screen, logical points.
#[derive(Debug, Clone, Default)]
pub struct TrackView {
    pub screen: Vec<[f32; 2]>,
}

/// The one-time notice. Nothing else can be used until it is accepted.
pub fn notice(ctx: &Context, safety: &mut SafetyView, actions: &mut Vec<UiAction>) {
    // Automated captures (NAVCORE_SHOT) are not a first start; one can ask
    // to see the notice with NAVCORE_SHOW_NOTICE=1.
    let capture = std::env::var_os("NAVCORE_SHOT").is_some()
        && std::env::var_os("NAVCORE_SHOW_NOTICE").is_none();
    let first = safety.notice_accepted < NOTICE_VERSION && !capture;
    if !first && !safety.notice_open {
        return;
    }
    if first {
        // Dim and block the chart behind it.
        let screen = ctx.screen_rect();
        egui::Area::new(egui::Id::new("notice-shade"))
            .fixed_pos(screen.min)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                let (rect, _) = ui.allocate_exact_size(screen.size(), egui::Sense::click_and_drag());
                ui.painter().rect_filled(rect, 0.0, Color32::from_black_alpha(160));
            });
    }
    let mut open = true;
    let mut window = egui::Window::new("navcore beta — not for navigation")
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .order(egui::Order::Foreground)
        .max_width(460.0);
    if !first {
        window = window.open(&mut open);
    }
    window.show(ctx, |ui| {
        ui.label(
            "navcore is beta software. It is not a certified navigation system (it is not \
             an ECDIS) and must not be your only means of navigation.",
        );
        ui.add_space(6.0);
        for line in [
            "Charts can be out of date or wrong. Positions, depths, AIS targets and alarms \
             can be missing, late or wrong.",
            "Automatic routes are suggestions. Check every leg against the chart, the \
             pilot book and what you can see.",
            "Keep a proper lookout at all times, and carry the charts and publications \
             your boat is required to carry.",
        ] {
            ui.horizontal_wrapped(|ui| {
                ui.label("•");
                ui.label(line);
            });
        }
        ui.add_space(6.0);
        ui.label(RichText::new("You use navcore at your own risk.").strong());
        if first {
            ui.add_space(10.0);
            ui.vertical_centered(|ui| {
                if ui.button(RichText::new("  I understand  ").strong()).clicked() {
                    safety.notice_accepted = NOTICE_VERSION;
                    actions.push(UiAction::SafetyChanged);
                }
            });
        }
    });
    if !open {
        safety.notice_open = false;
    }
}

/// The alarm banner, top centre of the chart: everything raised, the
/// ringing ones red, with the one button that silences them.
pub fn banner(ctx: &Context, alarms: &[AlarmLine], actions: &mut Vec<UiAction>) {
    if alarms.is_empty() {
        return;
    }
    let area = ctx.available_rect();
    let ringing = alarms.iter().any(|a| !a.acknowledged);
    egui::Area::new(egui::Id::new("alarm-banner"))
        .fixed_pos(egui::pos2(area.center().x, area.top() + 8.0))
        .pivot(Align2::CENTER_TOP)
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            let fill = if ringing { Color32::from_rgb(170, 20, 20) } else { Color32::from_rgb(150, 95, 20) };
            egui::Frame::popup(ui.style()).fill(fill).show(ui, |ui| {
                ui.set_max_width(area.width().min(560.0) - 24.0);
                for a in alarms {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(a.title).strong().color(Color32::WHITE));
                        ui.label(RichText::new(&a.message).color(Color32::WHITE));
                    });
                }
                if ringing {
                    ui.add_space(4.0);
                    if ui
                        .button(RichText::new("  Silence  ").strong())
                        .on_hover_text("Stop the sound. The alarm stays here until it clears.")
                        .clicked()
                    {
                        actions.push(UiAction::AlarmsSilence);
                    }
                }
            });
        });
}

/// The MOB button: bottom left of the chart, red, always there. One tap
/// marks — a confirmation costs seconds nobody has; clearing asks twice.
pub fn mob_button(ctx: &Context, active: bool, actions: &mut Vec<UiAction>) {
    let area = ctx.available_rect();
    egui::Area::new(egui::Id::new("mob-button"))
        .fixed_pos(egui::pos2(area.left() + 10.0, area.bottom() - 10.0))
        .pivot(Align2::LEFT_BOTTOM)
        // Under the windows: a window opened over it must stay usable, and
        // the button is back the moment the window moves or closes.
        .order(egui::Order::Background)
        .show(ctx, |ui| {
            let size = Vec2::new(64.0, 44.0);
            let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
            let fill = if response.hovered() { Color32::from_rgb(230, 45, 45) } else { MOB_RED };
            ui.painter().rect(rect, 8.0, fill, Stroke::new(2.0_f32, Color32::WHITE));
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                "MOB",
                FontId::proportional(20.0),
                Color32::WHITE,
            );
            let response = response.on_hover_text(if active {
                "Man overboard is marked — tap to mark again at the boat's position now"
            } else {
                "Man overboard: mark this position, sound the alarm, and show the way back"
            });
            if response.clicked() {
                actions.push(UiAction::MobMark);
            }
        });
}

/// The man overboard panel: time since, the way back, and the guide.
pub fn mob_panel(ctx: &Context, mob: &MobView, safety: &mut SafetyView, actions: &mut Vec<UiAction>) {
    let area = ctx.available_rect();
    egui::Area::new(egui::Id::new("mob-panel"))
        .fixed_pos(egui::pos2(area.left() + 10.0, area.bottom() - 64.0))
        .pivot(Align2::LEFT_BOTTOM)
        .order(egui::Order::Middle)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).stroke(Stroke::new(2.0_f32, MOB_RED)).show(ui, |ui| {
                let (m, s) = (mob.elapsed_s / 60, mob.elapsed_s % 60);
                ui.label(
                    RichText::new(format!("MAN OVERBOARD  {m:02}:{s:02}"))
                        .strong()
                        .size(18.0)
                        .color(MOB_RED),
                );
                match (mob.bearing_deg, mob.distance_m) {
                    (Some(b), Some(d)) => {
                        let dist = if d < 0.1 * 1852.0 {
                            format!("{d:.0} m")
                        } else {
                            format!("{:.2} NM", d / 1852.0)
                        };
                        ui.label(
                            RichText::new(format!(
                                "BRG {}   DST {dist}",
                                crate::geo::format_bearing(b)
                            ))
                            .strong()
                            .size(18.0),
                        );
                    }
                    _ if mob.screen.is_none() => {
                        ui.label("Marked without a position fix — steer by the spotter.");
                    }
                    _ => {
                        ui.label("No position fix now — steer by the spotter.");
                    }
                }
                ui.horizontal(|ui| {
                    if ui.button("Recovery guide").clicked() {
                        safety.guide_open = true;
                    }
                    let clear = if safety.mob_clear_armed { "Tap again to clear" } else { "Clear MOB" };
                    if ui.button(clear).clicked() {
                        if safety.mob_clear_armed {
                            safety.mob_clear_armed = false;
                            actions.push(UiAction::MobClear);
                        } else {
                            safety.mob_clear_armed = true;
                        }
                    }
                });
            });
        });
}

/// On the chart: today's track, a logbook day's track and notes, the
/// anchor circle, the MOB mark and the line back to it. Under the
/// interface, over the chart.
pub fn draw_on_chart(
    ctx: &Context,
    track: &TrackView,
    log_track: &TrackView,
    log_notes: &[([f32; 2], String)],
    anchor: Option<&AnchorView>,
    mob: Option<&MobView>,
) {
    let painter = ctx.layer_painter(egui::LayerId::background());
    let log_colour = Color32::from_rgb(130, 60, 170);
    if log_track.screen.len() >= 2 {
        let pts: Vec<Pos2> = log_track.screen.iter().map(|p| Pos2::new(p[0], p[1])).collect();
        painter.add(egui::Shape::line(pts, Stroke::new(2.5_f32, log_colour)));
    }
    for (p, text) in log_notes {
        let at = Pos2::new(p[0], p[1]);
        painter.circle(at, 5.0, log_colour, Stroke::new(1.5_f32, Color32::WHITE));
        let short: String = text.chars().take(28).collect();
        let short = if short.len() < text.len() { format!("{short}…") } else { short };
        painter.text(at + Vec2::new(8.0, 0.0), Align2::LEFT_CENTER, short, FontId::proportional(12.0), log_colour);
    }
    if track.screen.len() >= 2 {
        let pts: Vec<Pos2> = track.screen.iter().map(|p| Pos2::new(p[0], p[1])).collect();
        painter.add(egui::Shape::line(pts, Stroke::new(2.0_f32, Color32::from_rgb(160, 70, 30))));
    }
    if let Some(a) = anchor {
        let c = Pos2::new(a.screen[0], a.screen[1]);
        let outside = a.distance_m.is_some_and(|d| d > a.radius_m as f64);
        let colour = if outside { MOB_RED } else { Color32::from_rgb(30, 110, 200) };
        painter.circle_stroke(c, a.radius_px.max(6.0), Stroke::new(2.0_f32, colour));
        painter.circle_filled(c, 4.0, colour);
        painter.text(c + Vec2::new(0.0, 7.0), Align2::CENTER_TOP, "anchor", FontId::proportional(11.0), colour);
    }
    if let Some(m) = mob {
        if let Some(s) = m.screen {
            let at = Pos2::new(s[0], s[1]);
            if let Some(o) = m.own_screen {
                let from = Pos2::new(o[0], o[1]);
                painter.add(egui::Shape::dashed_line(
                    &[from, at],
                    Stroke::new(2.5_f32, MOB_RED),
                    10.0,
                    6.0,
                ));
            }
            painter.circle(at, 11.0, Color32::from_rgba_unmultiplied(205, 30, 30, 200), Stroke::new(2.0_f32, Color32::WHITE));
            let r = 6.0;
            let white = Stroke::new(2.5_f32, Color32::WHITE);
            painter.line_segment([at + Vec2::new(-r, -r), at + Vec2::new(r, r)], white);
            painter.line_segment([at + Vec2::new(-r, r), at + Vec2::new(r, -r)], white);
            painter.text(
                at + Vec2::new(0.0, 16.0),
                Align2::CENTER_TOP,
                "MOB",
                FontId::proportional(13.0),
                MOB_RED,
            );
        }
    }
}

/// What the Safety window needs to show about the boat right now.
pub struct WatchState<'a> {
    pub anchor: Option<&'a AnchorView>,
    pub has_fix: bool,
    pub depth_m: Option<f64>,
}

/// The Safety window: alarms, anchor watch, the guide, the notice.
pub fn window(
    ctx: &Context,
    safety: &mut SafetyView,
    instruments: &mut crate::render::ui::InstrumentView,
    watch: &WatchState<'_>,
    actions: &mut Vec<UiAction>,
) {
    let before = safety.clone();
    let cpa_before = (instruments.cpa_alarm_nm, instruments.tcpa_alarm_min);
    let mut open = safety.open;
    egui::Window::new("Safety")
        .open(&mut open)
        .default_width(340.0)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Man overboard — recovery guide").strong()).clicked() {
                    safety.guide_open = true;
                }
            });
            ui.separator();

            ui.label(RichText::new("Alarms").strong());
            ui.checkbox(&mut safety.sound, "Alarms make a sound");
            ui.horizontal(|ui| {
                ui.label("Collision: closer than");
                ui.add(egui::DragValue::new(&mut instruments.cpa_alarm_nm).speed(0.05).range(0.02..=5.0).fixed_decimals(2).suffix(" NM"));
                ui.label("within");
                ui.add(egui::DragValue::new(&mut instruments.tcpa_alarm_min).speed(1.0).range(1.0..=60.0).fixed_decimals(0).suffix(" min"));
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut safety.depth_alarm, "Shallow: less than");
                ui.add(egui::DragValue::new(&mut safety.depth_alarm_m).speed(0.1).range(0.3..=50.0).fixed_decimals(1).suffix(" m"));
            });
            ui.label(
                RichText::new(match watch.depth_m {
                    Some(d) => format!("Sounder now: {d:.1} m under the boat"),
                    None => "No depth from the boat — the shallow alarm needs a sounder on Signal K".into(),
                })
                .weak(),
            );
            ui.separator();

            ui.label(RichText::new("Anchor watch").strong());
            match watch.anchor {
                Some(a) => {
                    ui.label(match a.distance_m {
                        Some(d) => format!("Watching: {d:.0} m from the anchor, circle {:.0} m", a.radius_m),
                        None => format!("Watching a {:.0} m circle — no position fix", a.radius_m),
                    });
                    if ui.button("Stop the anchor watch").clicked() {
                        actions.push(UiAction::AnchorUp);
                    }
                }
                None => {
                    ui.horizontal(|ui| {
                        ui.label("Circle");
                        ui.add(egui::DragValue::new(&mut safety.anchor_radius_m).speed(1.0).range(10.0..=500.0).fixed_decimals(0).suffix(" m"));
                        let drop = ui.add_enabled(watch.has_fix, egui::Button::new("Anchor here"));
                        if drop
                            .on_hover_text("Watch a circle round the boat's position now. Allow for the scope of the chain.")
                            .on_disabled_hover_text("Needs a position fix")
                            .clicked()
                        {
                            actions.push(UiAction::AnchorDrop);
                        }
                    });
                }
            }
            ui.separator();

            if ui.link("About navcore beta — not for navigation").clicked() {
                safety.notice_open = true;
            }
        });
    safety.open = open;
    if *safety != before || (instruments.cpa_alarm_nm, instruments.tcpa_alarm_min) != cpa_before {
        actions.push(UiAction::SafetyChanged);
    }
}

/// The recovery guide: what to do in the first seconds, three ways back to
/// the person, and getting them aboard.
pub fn guide(ctx: &Context, safety: &mut SafetyView) {
    if !safety.guide_open {
        return;
    }
    let mut open = true;
    egui::Window::new("Man overboard — recovery")
        .open(&mut open)
        .default_width(560.0)
        .default_height(560.0)
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            let mut scroll = egui::ScrollArea::vertical();
            // NAVCORE_GUIDE_SCROLL=<points> opens it scrolled, for captures.
            if let Some(y) = std::env::var("NAVCORE_GUIDE_SCROLL").ok().and_then(|v| v.parse().ok()) {
                scroll = scroll.vertical_scroll_offset(y);
            }
            scroll.show(ui, |ui| {
                heading(ui, "At once");
                steps(ui, &[
                    "Shout \"Man overboard!\" and throw the lifebuoy, danbuoy or Lifesling towards the person. At 6 knots the gap grows by 3 m every second.",
                    "Name one spotter. Their only job: point at the person, and never look away.",
                    "Press MOB (here, and on the VHF if it has one). The bearing and distance are your way back if the spotter loses sight.",
                    "Send a Mayday on VHF channel 16, or press the red DSC Distress button — at once if you are short-handed, in cold water, at night or in heavy weather. It can be cancelled.",
                    "Before starting the engine, check that no sheet or line is over the side.",
                ]);

                heading(ui, "Quick-stop — under sail, best short-handed");
                method(ui, Diagram::QuickStop, &[
                    "Head up into the wind at once and tack, leaving the headsail sheeted: it backs and slows the boat.",
                    "Keep turning, away from the wind, until it is on or aft of the beam. Sail two or three boat lengths.",
                    "Furl or drop the headsail if you can.",
                    "Gybe.",
                    "Come back on a close reach, easing the mainsheet to slow down, and stop alongside the person.",
                ]);

                heading(ui, "Reach–tack–reach (figure of eight) — under sail");
                method(ui, Diagram::FigureEight, &[
                    "Bear away onto a beam reach and sail away six to ten boat lengths — no further than the spotter can keep pointing.",
                    "Tack. Do not gybe.",
                    "Bear away onto a broad reach, heading back to pass a little downwind of the person.",
                    "Three or four boat lengths downwind, head up onto a close reach towards them. If the sails still draw with the sheets fully eased, you are too far off the wind: bear away and come again.",
                    "Stop alongside with the sails flapping, the person level with the shrouds — never at the stern.",
                ]);

                heading(ui, "Under engine — and the Williamson turn");
                method(ui, Diagram::Williamson, &[
                    "Sails down or furled and every line on board first.",
                    "Approach into the wind or current, so the boat stops easily.",
                    "Engine in neutral before the person is near the stern: a turning propeller is the worst danger in a recovery.",
                    "Williamson turn (at night, or when the person is out of sight): helm hard over; when the heading has changed 60°, helm hard over the other way; come round onto the reciprocal of your original course. It brings the boat back down her own track.",
                ]);

                heading(ui, "Getting them aboard");
                steps(ui, &[
                    "Get a line on them first — Lifesling, throwing line or heaving line. Once they are attached they cannot be lost.",
                    "Lift with a halyard on a winch, clipped to their harness or the Lifesling; or the boarding ladder if they can climb.",
                    "Parbuckle: roll them up the side in a small sail or a net, lines from the deck under and back to a winch.",
                    "After a long time in cold water, lift them horizontally if you can, and treat for hypothermia. Get medical advice by radio.",
                ]);

                ui.add_space(8.0);
                ui.label(
                    RichText::new(
                        "Practise both sailing methods with your crew in calm water, with a fender and a bucket as the person. \
                         Your own training (RYA, US Sailing, your national federation) comes before this summary.",
                    )
                    .weak()
                    .italics(),
                );
            });
        });
    if !open {
        safety.guide_open = false;
    }
}

fn heading(ui: &mut egui::Ui, text: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(text).strong().size(16.0));
    ui.add_space(2.0);
}

fn steps(ui: &mut egui::Ui, lines: &[&str]) {
    for (i, line) in lines.iter().enumerate() {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("{}.", i + 1)).strong());
            ui.label(*line);
        });
    }
}

/// Steps beside their course diagram; the diagram goes above on a narrow
/// window.
fn method(ui: &mut egui::Ui, diagram: Diagram, lines: &[&str]) {
    const DIAGRAM: Vec2 = Vec2::new(170.0, 150.0);
    if ui.available_width() > 420.0 {
        ui.horizontal_top(|ui| {
            draw_diagram(ui, diagram, DIAGRAM);
            ui.vertical(|ui| steps(ui, lines));
        });
    } else {
        draw_diagram(ui, diagram, DIAGRAM);
        steps(ui, lines);
    }
}

#[derive(Clone, Copy)]
enum Diagram {
    QuickStop,
    FigureEight,
    Williamson,
}

/// A course pattern, seen from above: the wind from the top (except the
/// Williamson turn, which does not depend on it), the person as a red dot,
/// the boat's track as a line with an arrowhead, and the manoeuvres named
/// where they happen.
fn draw_diagram(ui: &mut egui::Ui, diagram: Diagram, size: Vec2) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals();
    painter.rect_filled(rect, 6.0, visuals.extreme_bg_color);
    let ink = visuals.text_color();
    let track = Color32::from_rgb(30, 110, 200);
    let at = |x: f32, y: f32| rect.min + Vec2::new(x * rect.width(), y * rect.height());

    // (the path, the person, labels, wind)
    let (path, person, labels, wind): (Vec<[f32; 2]>, [f32; 2], Vec<(&str, [f32; 2])>, bool) = match diagram {
        Diagram::QuickStop => (
            vec![
                [0.12, 0.60], [0.40, 0.60], [0.58, 0.54], [0.66, 0.40], [0.60, 0.28],
                [0.46, 0.30], [0.34, 0.44], [0.30, 0.62], [0.36, 0.80], [0.52, 0.86],
                [0.64, 0.80], [0.62, 0.70], [0.50, 0.64],
            ],
            [0.44, 0.60],
            vec![("tack", [0.70, 0.30]), ("gybe", [0.68, 0.88])],
            true,
        ),
        Diagram::FigureEight => (
            vec![
                [0.06, 0.52], [0.24, 0.52], [0.50, 0.52], [0.76, 0.52], [0.88, 0.44],
                [0.86, 0.33], [0.76, 0.34], [0.60, 0.50], [0.44, 0.66], [0.30, 0.72],
                [0.22, 0.66], [0.22, 0.57],
            ],
            [0.24, 0.52],
            vec![("tack", [0.84, 0.25]), ("close reach", [0.30, 0.82])],
            true,
        ),
        Diagram::Williamson => (
            vec![
                [0.46, 0.95], [0.46, 0.75], [0.46, 0.58], [0.52, 0.44], [0.62, 0.34],
                [0.62, 0.22], [0.52, 0.14], [0.42, 0.18], [0.42, 0.32], [0.46, 0.46],
                [0.46, 0.62],
            ],
            [0.46, 0.75],
            vec![("60°", [0.70, 0.44]), ("hard over", [0.34, 0.08])],
            false,
        ),
    };

    if wind {
        let (a, b) = (at(0.10, 0.06), at(0.10, 0.22));
        painter.arrow(a, b - a, Stroke::new(2.0_f32, ink));
        painter.text(at(0.15, 0.08), Align2::LEFT_TOP, "wind", FontId::proportional(11.0), ink);
    }
    // A smooth line through the points (Catmull-Rom), then its arrowhead.
    let pts: Vec<Pos2> = path.iter().map(|p| at(p[0], p[1])).collect();
    let mut curve = Vec::new();
    for i in 0..pts.len() - 1 {
        let p0 = pts[i.saturating_sub(1)];
        let (p1, p2) = (pts[i], pts[i + 1]);
        let p3 = pts[(i + 2).min(pts.len() - 1)];
        for s in 0..8 {
            let t = s as f32 / 8.0;
            let (t2, t3) = (t * t, t * t * t);
            let v = (p1.to_vec2() * 2.0
                + (p2 - p0) * t
                + (p0.to_vec2() * 2.0 - p1.to_vec2() * 5.0 + p2.to_vec2() * 4.0 - p3.to_vec2()) * t2
                + (p1.to_vec2() * 3.0 - p0.to_vec2() - p2.to_vec2() * 3.0 + p3.to_vec2()) * t3)
                * 0.5;
            curve.push(v.to_pos2());
        }
    }
    curve.push(*pts.last().unwrap());
    painter.add(egui::Shape::line(curve.clone(), Stroke::new(2.0_f32, track)));
    let n = curve.len();
    let dir = (curve[n - 1] - curve[n - 3]).normalized();
    painter.arrow(curve[n - 1] - dir * 8.0, dir * 8.0, Stroke::new(2.0_f32, track));

    painter.circle(at(person[0], person[1]), 5.0, MOB_RED, Stroke::new(1.0_f32, Color32::WHITE));
    for (text, p) in labels {
        painter.text(at(p[0], p[1]), Align2::CENTER_CENTER, text, FontId::proportional(11.0), ink);
    }
}
