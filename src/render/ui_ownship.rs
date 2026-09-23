//! What goes with own ship that S-52 has no symbol for.
//!
//! The boat itself is `OWNSHP01`, drawn by the chart pipeline in
//! [`super::mariner`] — same atlas, same palette, same shader as every buoy.
//! The presentation library has no symbol for a line of arbitrary length, so
//! the heading line and the course vector are drawn here, and so are the words
//! that go with a fix that has stopped arriving.
//!
//! - **The heading line** extends the hull's aim, so it can be read against a
//!   chart feature further off than the symbol itself.
//! - **The course vector** points along COG and reaches as far as the boat
//!   will travel in six minutes, ticked each minute. Heading and course differ
//!   whenever there is tide or leeway, and that difference is the single most
//!   useful thing on the display when closing a hazard.
//! - **A dead fix** greys the line and says NO FIX. The symbol is suppressed
//!   entirely upstream: a boat sitting confidently on a chart while the GPS is
//!   dead is the worst thing a plotter can draw.

use egui::{Color32, Context, FontId, Pos2, Stroke, Vec2};

/// Everything the overlay needs, resolved by the renderer which owns the camera.
#[derive(Debug, Clone, Copy)]
pub struct OwnShip {
    /// Where the boat is on screen, in logical points.
    pub screen: [f32; 2],
    /// Heading, radians clockwise from screen-up (north).
    pub heading: Option<f32>,
    /// Course over ground, same convention.
    pub cog: Option<f32>,
    /// Speed over ground in m/s, for the length of the course vector.
    pub sog: Option<f32>,
    /// Metres per logical point, to scale the vector in real distance.
    pub mpp: f32,
    /// The fix has not been refreshed lately.
    pub stale: bool,
}

/// How far ahead the course vector reaches, in minutes of travel.
const VECTOR_MINUTES: f32 = 6.0;
/// The marker's half-length in points.
const HULL: f32 = 13.0;

pub fn draw(ctx: &Context, ship: &OwnShip) {
    let pos = Pos2::new(ship.screen[0], ship.screen[1]);
    // Off-screen by more than its own size: nothing to draw, and the vector
    // would otherwise be painted across the whole display from a phantom.
    let screen = ctx.screen_rect().expand(HULL * 4.0);
    if !screen.contains(pos) {
        return;
    }

    let painter = ctx.layer_painter(egui::LayerId::background());

    // Same ink as the AIS overlay: near-black on the day chart would vanish
    // against the dusk and night palettes.
    let line = if ship.stale {
        Color32::from_gray(140)
    } else if ctx.style().visuals.dark_mode {
        Color32::from_gray(225)
    } else {
        Color32::from_rgb(30, 30, 30)
    };

    // Course vector first, so the hull sits on top of it.
    if let (Some(cog), Some(sog)) = (ship.cog, ship.sog) {
        if sog > 0.2 && ship.mpp > 0.0 {
            let metres = sog * VECTOR_MINUTES * 60.0;
            let length = metres / ship.mpp;
            if length > HULL {
                let end = pos + unit(cog) * length;
                painter.line_segment(
                    [pos, end],
                    Stroke::new(2.0_f32, line.gamma_multiply(0.8)),
                );
                // A tick at each minute, so distance-to-go can be read off the
                // vector without measuring it.
                for minute in 1..VECTOR_MINUTES as i32 {
                    let at = pos + unit(cog) * (length * minute as f32 / VECTOR_MINUTES);
                    let across = unit(cog + std::f32::consts::FRAC_PI_2) * 3.0;
                    painter.line_segment([at - across, at + across], Stroke::new(1.5_f32, line));
                }
                painter.text(
                    end + unit(cog) * 8.0,
                    egui::Align2::CENTER_CENTER,
                    format!("{VECTOR_MINUTES:.0} min"),
                    FontId::proportional(10.0),
                    line,
                );
            }
        }
    }

    // The hull itself is an S-52 symbol (OWNSHP01), drawn by the chart
    // pipeline. What is left here is the heading line — a line of arbitrary
    // length, which the presentation library has no symbol for — and the
    // words that go with a dead fix.
    if let Some(b) = ship.heading.or(ship.cog) {
        painter.line_segment(
            [pos + unit(b) * HULL, pos + unit(b) * (HULL * 3.5)],
            Stroke::new(1.5_f32, line),
        );
    }

    if ship.stale {
        painter.text(
            pos + Vec2::new(0.0, HULL + 12.0),
            egui::Align2::CENTER_CENTER,
            "NO FIX",
            FontId::proportional(11.0),
            Color32::from_rgb(210, 130, 60),
        );
    }
}

/// A unit vector for a compass bearing in radians, in screen coordinates where
/// y grows downward and 0 is north.
fn unit(bearing: f32) -> Vec2 {
    Vec2::new(bearing.sin(), -bearing.cos())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearings_map_to_screen_directions() {
        let north = unit(0.0);
        assert!(north.x.abs() < 1e-6 && north.y < 0.0, "{north:?}");
        let east = unit(std::f32::consts::FRAC_PI_2);
        assert!(east.x > 0.0 && east.y.abs() < 1e-6, "{east:?}");
        let south = unit(std::f32::consts::PI);
        assert!(south.y > 0.0, "{south:?}");
        let west = unit(3.0 * std::f32::consts::FRAC_PI_2);
        assert!(west.x < 0.0, "{west:?}");
    }

    #[test]
    fn a_unit_vector_stays_unit_length() {
        for step in 0..16 {
            let b = step as f32 * std::f32::consts::TAU / 16.0;
            assert!((unit(b).length() - 1.0).abs() < 1e-5);
        }
    }
}
