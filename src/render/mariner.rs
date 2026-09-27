//! Mariner objects — own ship and AIS traffic — drawn from the S-52
//! Presentation Library.
//!
//! These are not chart features: they move every second and belong to no cell.
//! But they are S-52 objects all the same, and the library carries symbols for
//! them, so they go through the same atlas, the same palette and the same
//! shader as every buoy and beacon. The alternative — shapes drawn by hand in
//! the interface layer — gives a display that is *nearly* an ECDIS, which is
//! the one thing a chart display must not be.
//!
//! The symbols, straight from `chartsymbols.xml`:
//!
//! | Symbol     | The library's own description                              |
//! |------------|------------------------------------------------------------|
//! | `OWNSHP01` | own ship symbol, constant size                             |
//! | `AISVES01` | active AIS target showing vector and/or heading            |
//! | `AISSLP01` | sleeping AIS target                                        |
//! | `AISDEF01` | AIS target whose heading and course are both unknown       |
//! | `AISONE01` | one minute mark for AIS vector                             |
//! | `AISSIX01` | six minute mark for AIS vector                             |
//!
//! Those last two are why the vector is marked the way it is: S-52 does not
//! draw an arbitrary line into the future, it draws a line ticked at one and
//! six minutes so that time-to-go can be read off it directly.
//!
//! What stays in the interface layer, and why: target *names*, the CPA
//! read-out, and the cross over a lost target. The library has no symbol for
//! any of them — names and CPA are text an ECDIS composes itself, and a lost
//! target is shown by crossing the symbol through rather than by a symbol of
//! its own.

use crate::render::symbols::{symbol_id_from_s52_name, SymbolInstance};
use crate::signalk::Fleet;

/// Display priority for mariner objects.
///
/// S-52 puts them above every chart object: a ship must never be hidden by a
/// depth area, and 9 is the top of the range the renderer sorts on.
const MARINER_PRIO: u32 = 9;

/// Where the vector is ticked, in minutes.
const ONE_MINUTE: f64 = 60.0;
const SIX_MINUTES: f64 = 360.0;

/// The symbols this layer needs, resolved once.
#[derive(Debug, Clone, Copy)]
pub struct MarinerSymbols {
    pub own_ship: u32,
    pub active: u32,
    pub sleeping: u32,
    pub undefined: u32,
    pub one_minute: u32,
    pub six_minutes: u32,
}

impl MarinerSymbols {
    /// `None` if the presentation library lacks them, which means something is
    /// wrong with the install rather than with the boat — better to draw no
    /// mariner objects than to draw the wrong glyph for a ship.
    pub fn resolve() -> Option<Self> {
        Some(Self {
            own_ship: symbol_id_from_s52_name("OWNSHP01")?,
            active: symbol_id_from_s52_name("AISVES01")?,
            sleeping: symbol_id_from_s52_name("AISSLP01")?,
            undefined: symbol_id_from_s52_name("AISDEF01")?,
            one_minute: symbol_id_from_s52_name("AISONE01")?,
            six_minutes: symbol_id_from_s52_name("AISSIX01")?,
        })
    }
}

/// Which AIS symbol a target gets.
///
/// The three cases the library distinguishes, in the order S-52 asks them:
/// a target that cannot say which way it is pointing gets the "undefined"
/// symbol regardless of anything else, because the triangle would otherwise be
/// pointing somewhere invented.
fn ais_symbol(
    symbols: &MarinerSymbols,
    heading: Option<f64>,
    course: Option<f64>,
    under_way: bool,
) -> u32 {
    if heading.is_none() && course.is_none() {
        symbols.undefined
    } else if under_way {
        symbols.active
    } else {
        symbols.sleeping
    }
}

/// Turn lat/lon into the metres from `origin` the symbol shader expects.
///
/// Relative, not global: global Mercator in f32 is only good to 0.5–1 m here,
/// which at close zoom is several pixels of boat jumping about in her berth.
fn world(lat: f64, lon: f64, origin: [f64; 2]) -> [f32; 2] {
    let (x, y) = crate::render::projection::Projection::to_mercator(lat, lon);
    [(x - origin[0]) as f32, (y - origin[1]) as f32]
}

/// Build one frame's worth of mariner symbols.
///
/// `stale_own` suppresses our own ship: a boat symbol sitting confidently on a
/// chart while the fix is dead is the worst thing a plotter can draw, and the
/// interface layer marks the gap in words instead.
///
/// Positions are metres from `origin` (global Mercator); the renderer draws
/// them through a camera slot re-based on the same point.
pub fn instances(
    fleet: &Fleet,
    symbols: &MarinerSymbols,
    stale_own: bool,
    origin: [f64; 2],
) -> Vec<SymbolInstance> {
    let mut out = Vec::new();

    if !stale_own {
        if let Some((lat, lon)) = fleet.own.position() {
            let heading = fleet
                .own
                .heading_true()
                .or_else(|| fleet.own.number("navigation.courseOverGroundTrue"))
                .unwrap_or(0.0);
            out.push(SymbolInstance {
                position: world(lat, lon, origin),
                symbol_id: symbols.own_ship,
                rotation: heading as f32,
                disp_prio: MARINER_PRIO,
                scale: 1.0,
                true_bearing: 1,
            });
        }
    }

    for target in fleet.targets() {
        let Some((lat, lon)) = target.position() else {
            continue;
        };
        let heading = target.heading();
        let course = target.course();
        // A lost target keeps its symbol and is crossed through by the overlay;
        // swapping the glyph would lose the heading it was last showing.
        out.push(SymbolInstance {
            position: world(lat, lon, origin),
            symbol_id: ais_symbol(symbols, heading, course, target.under_way() && !target.is_lost()),
            rotation: heading.or(course).unwrap_or(0.0) as f32,
            disp_prio: MARINER_PRIO,
            scale: 1.0,
            true_bearing: 1,
        });

        // The time marks along the vector. Only for a target actually under
        // way and actually moving — a mark on a stationary ship would claim it
        // will be somewhere in a minute.
        if target.is_lost() || !target.under_way() {
            continue;
        }
        let (Some(course), Some(speed)) = (course, target.speed()) else {
            continue;
        };
        if speed <= 0.2 {
            continue;
        }
        for (seconds, symbol) in [
            (ONE_MINUTE, symbols.one_minute),
            (SIX_MINUTES, symbols.six_minutes),
        ] {
            // Advanced along the geodesic, not along a straight line in the
            // drawing plane — over six minutes at speed that is metres, but it
            // is the same call that gives CPA its accuracy and there is no
            // reason to use a worse one here.
            let at = crate::geo::advance(
                crate::geo::LatLon::new(lat, lon),
                course,
                speed * seconds,
            );
            out.push(SymbolInstance {
                position: world(at.lat, at.lon, origin),
                symbol_id: symbol,
                // The marks are drawn across the vector, so they turn with it.
                rotation: course as f32,
                disp_prio: MARINER_PRIO,
                scale: 1.0,
                true_bearing: 1,
            });
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbols() -> MarinerSymbols {
        MarinerSymbols {
            own_ship: 100,
            active: 1,
            sleeping: 2,
            undefined: 3,
            one_minute: 4,
            six_minutes: 5,
        }
    }

    #[test]
    fn the_library_carries_every_symbol_this_layer_needs() {
        // If this fails the presentation library is not the one navcore ships,
        // and mariner objects would silently fall back to nothing.
        assert!(
            MarinerSymbols::resolve().is_some(),
            "chartsymbols.xml is missing an OWNSHP/AIS symbol"
        );
    }

    #[test]
    fn a_target_that_cannot_say_which_way_it_points_gets_the_undefined_symbol() {
        let s = symbols();
        // Neither heading nor course: the triangle would point at an invented
        // bearing, so S-52 has a symbol that says exactly that.
        assert_eq!(ais_symbol(&s, None, None, true), s.undefined);
        assert_eq!(ais_symbol(&s, None, None, false), s.undefined);
        // Course alone is enough to orient it.
        assert_eq!(ais_symbol(&s, None, Some(1.0), true), s.active);
        // Under way or not decides active against sleeping.
        assert_eq!(ais_symbol(&s, Some(1.0), None, false), s.sleeping);
        assert_eq!(ais_symbol(&s, Some(1.0), None, true), s.active);
    }

    fn fleet_with(json: &str) -> Fleet {
        let mut fleet = Fleet::default();
        fleet.apply(&crate::signalk::delta::parse(json).expect("a delta"));
        fleet
    }

    #[test]
    fn our_own_boat_becomes_one_symbol_at_its_position() {
        let fleet = fleet_with(
            r#"{"updates":[{"values":[
                {"path":"navigation.position","value":{"latitude":56.5,"longitude":11.6}},
                {"path":"navigation.headingTrue","value":1.5}]}]}"#,
        );
        let s = symbols();
        let out = instances(&fleet, &s, false, [0.0, 0.0]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].symbol_id, s.own_ship);
        assert_eq!(out[0].rotation, 1.5);
        assert_eq!(out[0].disp_prio, MARINER_PRIO);
        assert_eq!(out[0].position, world(56.5, 11.6, [0.0, 0.0]));

        // A dead fix draws no boat at all.
        assert!(instances(&fleet, &s, true, [0.0, 0.0]).is_empty());
    }

    #[test]
    fn a_moving_target_gets_its_symbol_and_two_time_marks() {
        let fleet = fleet_with(
            r#"{"context":"vessels.urn:mrn:imo:mmsi:1","updates":[{"values":[
                {"path":"navigation.position","value":{"latitude":56.5,"longitude":11.6}},
                {"path":"navigation.courseOverGroundTrue","value":0.0},
                {"path":"navigation.speedOverGround","value":5.0}]}]}"#,
        );
        let s = symbols();
        let out = instances(&fleet, &s, false, [0.0, 0.0]);
        assert_eq!(out.len(), 3, "the target and its one- and six-minute marks");
        assert_eq!(out[0].symbol_id, s.active);
        assert_eq!(out[1].symbol_id, s.one_minute);
        assert_eq!(out[2].symbol_id, s.six_minutes);

        // Steering due north, so each mark is further north than the last.
        assert!(out[1].position[1] > out[0].position[1]);
        assert!(out[2].position[1] > out[1].position[1]);
        // Six minutes is six times one minute's travel, near enough.
        let one = out[1].position[1] - out[0].position[1];
        let six = out[2].position[1] - out[0].position[1];
        assert!((six / one - 6.0).abs() < 0.05, "{}", six / one);
    }

    #[test]
    fn a_stationary_target_gets_no_time_marks() {
        // Otherwise a moored ship claims it will be somewhere in a minute.
        let fleet = fleet_with(
            r#"{"context":"vessels.urn:mrn:imo:mmsi:2","updates":[{"values":[
                {"path":"navigation.position","value":{"latitude":56.5,"longitude":11.6}},
                {"path":"navigation.headingTrue","value":0.3},
                {"path":"navigation.speedOverGround","value":0.0},
                {"path":"navigation.state","value":"moored"}]}]}"#,
        );
        let s = symbols();
        let out = instances(&fleet, &s, false, [0.0, 0.0]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].symbol_id, s.sleeping);
    }

    #[test]
    fn a_target_with_no_position_is_not_drawn_anywhere() {
        // AIS static data arrives before the first position report; a target
        // drawn at 0,0 would appear in the Gulf of Guinea.
        let fleet = fleet_with(
            r#"{"context":"vessels.urn:mrn:imo:mmsi:3","updates":[{"values":[
                {"path":"name","value":"NORDLYS"}]}]}"#,
        );
        assert!(instances(&fleet, &symbols(), false, [0.0, 0.0]).is_empty());
    }
}
