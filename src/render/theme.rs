//! The colours of everything that is not the chart: panels, instruments,
//! buttons, and the marks Manx lays over the chart.
//!
//! The chart itself is drawn from the IHO S-52 colour tables (DAY_BRIGHT,
//! DUSK, NIGHT in `assets/s52/chartsymbols.xml`), and those are the standard
//! — IEC 62288 points to exactly them for readability in every ambient light.
//! What S-52 also says, and Manx used not to do, is that the interface
//! around the chart belongs to the same palette: its UI tokens (UIBCK, UINFD,
//! UINFR, UINFG, UINFO, UINFB, UINFM…) change with it. A Night chart that
//! peaks at RGB 50 beside a menu bar at RGB 200 is unreadable, and not
//! because of the chart: the eye adapts to the brightest thing in view, and
//! the chart drops below what it can see.
//!
//! So each palette carries one [`Theme`]:
//!
//! * **Day** is for sunlight. Near-white ground and near-black ink (S-52's
//!   UIBCK and UINFD, pushed to the ends of the scale, because in glare
//!   contrast is the only thing that survives). Every text colour clears 7:1
//!   against its ground, and the signal colours are saturated so a reading
//!   pops at arm's length.
//! * **Dusk** and **Night** follow S-52's black-ground tables. Ink sits a
//!   little above the chart's own brightest text — enough to read a number
//!   at a glance, not so much that it becomes the brightest thing on the
//!   bridge — and the signal colours are S-52's UI hues, lifted to the same
//!   level so that red, green and amber still read as themselves.
//!
//! One theme is live at a time, set when the palette switches. The draw
//! functions reach for it with [`current`] rather than threading a palette
//! through every call, because nearly every one of them needs it.

use std::sync::atomic::{AtomicU8, Ordering};

use egui::Color32;

/// Which of the three S-52 palettes is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Palette {
    Day,
    Dusk,
    Night,
}

impl Palette {
    /// From the S-52 colour-table name `switch_palette` is given.
    pub fn from_table(name: &str) -> Self {
        match name {
            "DUSK" => Palette::Dusk,
            "NIGHT" => Palette::Night,
            _ => Palette::Day,
        }
    }
}

/// The interface colours for one palette.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub palette: Palette,
    /// Behind text fields and the instrument strip: the extreme of the scale.
    pub ground: Color32,
    /// Panels, the menu bar, windows.
    pub panel: Color32,
    /// Buttons at rest.
    pub widget: Color32,
    /// Buttons under a finger.
    pub widget_hot: Color32,
    /// Separators and window edges.
    pub border: Color32,
    /// Text, and the figures on the instruments.
    pub ink: Color32,
    /// Labels and secondary text.
    pub ink_dim: Color32,
    /// The selected button, links, the cursor.
    pub accent: Color32,
    /// Danger: alarms, MOB, too shallow. S-52 UINFR.
    pub red: Color32,
    /// Good: arrived, connected, in range. S-52 UINFG.
    pub green: Color32,
    /// Caution: stale data, a warning. S-52 UINFO.
    pub amber: Color32,
    /// Information: your own track, water. S-52 UINFB.
    pub blue: Color32,
    /// Your own marks: logbook, routes' accents. S-52 UINFM.
    pub magenta: Color32,
    /// Text drawn *on* a red, green or blue fill.
    pub on_signal: Color32,
    /// Outline drawn round a mark laid on the chart, so a pale mark still
    /// reads over pale water and a dark one over land.
    pub halo: Color32,
    /// How bright an overlay laid on the chart may be — weather, tracks,
    /// the boat — as a fraction of its Day colour. The chart's own brightness
    /// falls this way from table to table, and an overlay that did not fall
    /// with it would glare.
    pub overlay: f32,
}

const DAY: Theme = Theme {
    palette: Palette::Day,
    ground: Color32::from_rgb(255, 255, 255),
    panel: Color32::from_rgb(244, 246, 247),
    widget: Color32::from_rgb(222, 227, 230),
    widget_hot: Color32::from_rgb(200, 208, 213),
    border: Color32::from_rgb(120, 130, 136),
    ink: Color32::from_rgb(8, 8, 8),
    ink_dim: Color32::from_rgb(64, 71, 76),
    accent: Color32::from_rgb(0, 90, 200),
    red: Color32::from_rgb(208, 16, 32),
    green: Color32::from_rgb(0, 128, 58),
    amber: Color32::from_rgb(166, 82, 0),
    blue: Color32::from_rgb(0, 92, 214),
    magenta: Color32::from_rgb(170, 22, 160),
    on_signal: Color32::from_rgb(255, 255, 255),
    halo: Color32::from_rgb(8, 8, 8),
    overlay: 1.0,
};

const DUSK: Theme = Theme {
    palette: Palette::Dusk,
    ground: Color32::from_rgb(7, 7, 7),
    panel: Color32::from_rgb(16, 17, 18),
    widget: Color32::from_rgb(34, 38, 40),
    widget_hot: Color32::from_rgb(50, 56, 58),
    border: Color32::from_rgb(60, 66, 68),
    ink: Color32::from_rgb(150, 160, 162),
    ink_dim: Color32::from_rgb(104, 113, 115),
    accent: Color32::from_rgb(70, 118, 200),
    red: Color32::from_rgb(214, 70, 82),
    green: Color32::from_rgb(86, 178, 72),
    amber: Color32::from_rgb(206, 132, 56),
    blue: Color32::from_rgb(84, 132, 226),
    magenta: Color32::from_rgb(176, 96, 196),
    on_signal: Color32::from_rgb(8, 8, 8),
    halo: Color32::from_rgb(0, 0, 0),
    overlay: 0.6,
};

const NIGHT: Theme = Theme {
    palette: Palette::Night,
    ground: Color32::from_rgb(4, 4, 4),
    panel: Color32::from_rgb(8, 9, 9),
    widget: Color32::from_rgb(20, 22, 23),
    widget_hot: Color32::from_rgb(30, 33, 34),
    border: Color32::from_rgb(34, 38, 39),
    ink: Color32::from_rgb(88, 95, 97),
    ink_dim: Color32::from_rgb(60, 65, 66),
    accent: Color32::from_rgb(40, 64, 128),
    red: Color32::from_rgb(150, 42, 26),
    green: Color32::from_rgb(56, 104, 30),
    amber: Color32::from_rgb(128, 82, 26),
    blue: Color32::from_rgb(52, 74, 160),
    magenta: Color32::from_rgb(116, 44, 116),
    on_signal: Color32::from_rgb(4, 4, 4),
    halo: Color32::from_rgb(0, 0, 0),
    overlay: 0.32,
};

impl Theme {
    pub fn of(palette: Palette) -> &'static Theme {
        match palette {
            Palette::Day => &DAY,
            Palette::Dusk => &DUSK,
            Palette::Night => &NIGHT,
        }
    }

    pub fn dark(&self) -> bool {
        self.palette != Palette::Day
    }

    /// A colour chosen for the Day chart, brought down to this palette's
    /// overlay level. Scaled towards black rather than made transparent: on
    /// a black-ground chart the two look alike, and scaling keeps an opaque
    /// mark opaque.
    pub fn dim(&self, c: Color32) -> Color32 {
        if self.overlay >= 1.0 {
            return c;
        }
        let k = self.overlay.clamp(0.0, 1.0);
        let s = |x: u8| (x as f32 * k).round() as u8;
        Color32::from_rgba_unmultiplied(s(c.r()), s(c.g()), s(c.b()), c.a())
    }

    /// egui's visuals, built from the theme rather than from its stock light
    /// and dark looks, which know nothing of sunlight or night vision.
    pub fn visuals(&self) -> egui::Visuals {
        use egui::Stroke;
        let mut v = if self.dark() {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        v.panel_fill = self.panel;
        v.window_fill = self.panel;
        v.window_stroke = Stroke::new(1.0_f32, self.border);
        v.extreme_bg_color = self.ground;
        v.faint_bg_color = mix(self.panel, self.ink, 0.04);
        v.code_bg_color = self.widget;
        v.hyperlink_color = self.accent;
        v.warn_fg_color = self.amber;
        v.error_fg_color = self.red;
        v.selection.bg_fill = self.accent;
        v.selection.stroke = Stroke::new(1.0_f32, self.on_signal_for(self.accent));
        v.text_cursor.stroke = Stroke::new(2.0_f32, self.accent);
        if self.dark() {
            // A shadow is a dark smear on a dark ground: it only costs.
            v.window_shadow.color = Color32::TRANSPARENT;
            v.popup_shadow.color = Color32::TRANSPARENT;
        }

        let w = &mut v.widgets;
        w.noninteractive.bg_fill = self.panel;
        w.noninteractive.bg_stroke = Stroke::new(1.0_f32, self.border);
        w.noninteractive.fg_stroke = Stroke::new(1.0_f32, self.ink);
        // egui's weak text is the midpoint of the text colour and this fill
        // (`Visuals::weak_text_color` → `gray_out`), so setting it to
        // 2·dim − ink makes every weak label land exactly on `ink_dim`.
        w.noninteractive.weak_bg_fill = extrapolate(self.ink, self.ink_dim);
        for (state, fill) in [
            (&mut w.inactive, self.widget),
            (&mut w.hovered, self.widget_hot),
            (&mut w.active, self.widget_hot),
            (&mut w.open, self.widget),
        ] {
            state.bg_fill = fill;
            state.weak_bg_fill = fill;
            state.fg_stroke = Stroke::new(1.0_f32, self.ink);
        }
        w.inactive.bg_stroke = Stroke::new(0.0_f32, Color32::TRANSPARENT);
        w.hovered.bg_stroke = Stroke::new(1.0_f32, self.border);
        w.active.bg_stroke = Stroke::new(1.0_f32, self.ink_dim);
        w.open.bg_stroke = Stroke::new(1.0_f32, self.border);
        // Strong text is the active widget's; strongest there is.
        w.active.fg_stroke = Stroke::new(1.5_f32, self.ink);
        v
    }

    /// Ink for text on a filled colour: whichever end of this theme's scale
    /// stands out from it more.
    pub fn on_signal_for(&self, fill: Color32) -> Color32 {
        if self.dark() {
            self.on_signal
        } else if luminance(fill) > 0.35 {
            self.ink
        } else {
            self.on_signal
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// Make `palette` the live theme.
pub fn set(palette: Palette) {
    CURRENT.store(palette as u8, Ordering::Relaxed);
}

/// The live theme.
pub fn current() -> &'static Theme {
    Theme::of(match CURRENT.load(Ordering::Relaxed) {
        1 => Palette::Dusk,
        2 => Palette::Night,
        _ => Palette::Day,
    })
}

fn mix(a: Color32, b: Color32, f: f32) -> Color32 {
    let c = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f).round() as u8;
    Color32::from_rgb(c(a.r(), b.r()), c(a.g(), b.g()), c(a.b(), b.b()))
}

/// The colour `f` for which the channel-wise midpoint of `ink` and `f` is
/// `dim`.
fn extrapolate(ink: Color32, dim: Color32) -> Color32 {
    let c = |i: u8, d: u8| (2 * d as i32 - i as i32).clamp(0, 255) as u8;
    Color32::from_rgb(c(ink.r(), dim.r()), c(ink.g(), dim.g()), c(ink.b(), dim.b()))
}

/// WCAG relative luminance, 0..1.
pub fn luminance(c: Color32) -> f32 {
    let lin = |x: u8| {
        let s = x as f32 / 255.0;
        if s <= 0.040_45 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
}

/// WCAG contrast ratio, 1..21.
pub fn contrast(a: Color32, b: Color32) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In sunlight contrast is everything. WCAG's AAA level (7:1) for text,
    /// 4.5:1 for the signal colours used as text — and all of it on the
    /// panel, which is a shade darker than the ground and so the harder case.
    #[test]
    fn day_text_reads_in_sunlight() {
        let t = Theme::of(Palette::Day);
        for (name, c) in [("ink", t.ink), ("ink_dim", t.ink_dim)] {
            let r = contrast(c, t.panel);
            assert!(r >= 7.0, "{name} on panel is {r:.1}:1");
        }
        for (name, c) in [
            ("red", t.red),
            ("green", t.green),
            ("amber", t.amber),
            ("blue", t.blue),
            ("magenta", t.magenta),
            ("accent", t.accent),
        ] {
            let r = contrast(c, t.panel);
            assert!(r >= 4.5, "{name} on panel is {r:.1}:1");
            // …and on a button, where a coloured label also sits.
            let r = contrast(c, t.widget);
            assert!(r >= 3.5, "{name} on a button is {r:.1}:1");
        }
        assert!(contrast(t.on_signal, t.red) >= 4.5, "white on the MOB red");
    }

    /// The interface must not be the brightest thing on a dark bridge. Its
    /// ink is held within a small multiple of the chart's own brightest text
    /// (S-52 CHWHT: Dusk 71,78,79; Night 37,41,41) — above it, so a number
    /// reads, but not so far above that the chart vanishes beside it.
    #[test]
    fn dark_palettes_keep_the_interface_near_the_chart() {
        let chart_text = |p| match p {
            Palette::Dusk => Color32::from_rgb(71, 78, 79),
            _ => Color32::from_rgb(37, 41, 41),
        };
        for p in [Palette::Dusk, Palette::Night] {
            let t = Theme::of(p);
            let chart = luminance(chart_text(p));
            let ink = luminance(t.ink);
            assert!(ink > chart * 1.5, "{p:?}: ink {ink:.4} barely above the chart's {chart:.4}");
            assert!(ink < chart * 6.0, "{p:?}: ink {ink:.4} glares beside the chart's {chart:.4}");
            // Nothing on the interface outshines its ink.
            for c in [t.panel, t.widget, t.widget_hot, t.border, t.red, t.green, t.amber, t.blue, t.magenta] {
                assert!(luminance(c) <= ink * 1.05, "{p:?}: {c:?} brighter than the ink");
            }
            // Text still separates from its ground — measured without WCAG's
            // flare term, which assumes a lit room this is not.
            assert!(ink / luminance(t.panel).max(1e-4) >= 10.0, "{p:?}: ink on panel");
            assert!(luminance(t.ink_dim) / luminance(t.panel).max(1e-4) >= 5.0, "{p:?}: dim on panel");
        }
    }

    /// Red, green and amber must stay red, green and amber in the dark:
    /// the dominant channel is the one the name promises.
    #[test]
    fn signal_colours_keep_their_hue_in_every_palette() {
        for p in [Palette::Day, Palette::Dusk, Palette::Night] {
            let t = Theme::of(p);
            assert!(t.red.r() > t.red.g() * 2 && t.red.r() > t.red.b() * 2, "{p:?} red");
            assert!(t.green.g() > t.green.r() && t.green.g() > t.green.b(), "{p:?} green");
            assert!(t.amber.r() > t.amber.g() && t.amber.g() > t.amber.b(), "{p:?} amber");
            assert!(t.blue.b() > t.blue.r() && t.blue.b() > t.blue.g(), "{p:?} blue");
        }
    }

    /// The weak-text trick: egui's grey-out of the ink must come out as the
    /// theme's dim colour, give or take the integer halving.
    #[test]
    fn weak_text_lands_on_the_dim_colour() {
        for p in [Palette::Day, Palette::Dusk, Palette::Night] {
            let t = Theme::of(p);
            let weak = t.visuals().weak_text_color();
            let close = |a: u8, b: u8| (a as i32 - b as i32).abs() <= 2;
            assert!(
                close(weak.r(), t.ink_dim.r()) && close(weak.g(), t.ink_dim.g()) && close(weak.b(), t.ink_dim.b()),
                "{p:?}: weak text {weak:?}, wanted {:?}",
                t.ink_dim
            );
        }
    }

    #[test]
    fn dimming_falls_with_the_palette_and_spares_the_day() {
        let c = Color32::from_rgb(200, 100, 50);
        assert_eq!(Theme::of(Palette::Day).dim(c), c);
        let dusk = Theme::of(Palette::Dusk).dim(c);
        let night = Theme::of(Palette::Night).dim(c);
        assert!(luminance(night) < luminance(dusk) && luminance(dusk) < luminance(c));
    }
}
