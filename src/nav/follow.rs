//! Following an active route: where am I relative to the plan, and what do I
//! steer?
//!
//! Pure logic — positions in, guidance out. Nothing here touches the screen,
//! the network or the store, which is what makes the M2 verification a plain
//! unit test: feed a synthetic track, assert the numbers.
//!
//! **The sign convention, decided and documented:** cross-track error is
//! **positive when the boat is to starboard of the leg**, looking along it
//! from its start to its end. Positive XTE therefore means *steer left* to
//! regain track, and that is exactly the `L` the NMEA sentences carry. Signal
//! K's `crossTrackError` uses the same sense (negative to port).

use uuid::Uuid;

use super::model::{LegKind, Route, WaypointSet};
use crate::geo::{self, LatLon, METRES_PER_NM};

/// Following-level configuration.
#[derive(Debug, Clone, Copy)]
pub struct FollowConfig {
    /// Arrival circle for waypoints that do not set their own.
    pub default_arrival_radius_nm: f64,
}

impl Default for FollowConfig {
    fn default() -> Self {
        Self {
            // The spec's §8.3 default.
            default_arrival_radius_nm: 0.1,
        }
    }
}

/// The follower's persistent state: which route, which leg.
#[derive(Debug, Clone)]
pub struct Following {
    pub route_id: Uuid,
    /// Index into the route's legs.
    pub leg: usize,
    pub config: FollowConfig,
}

/// A route as the follower saw it before an edit. See
/// [`Following::retarget`].
#[derive(Debug, Clone, PartialEq)]
pub struct MarksAhead {
    /// The mark being steered to, then each after it.
    ahead: Vec<Uuid>,
    /// Every mark of the route, so that a new one can be told from an old.
    known: Vec<Uuid>,
}

/// One update's answer: everything a helm or an autopilot needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Guidance {
    pub leg: usize,
    pub from: Uuid,
    pub to: Uuid,
    pub to_name: String,
    /// Signed cross-track error, nautical miles. **Positive = starboard of
    /// track = steer left.**
    pub xte_nm: f64,
    /// Bearing to the active waypoint, degrees true.
    pub btw_deg: f64,
    /// Distance to the active waypoint, nautical miles.
    pub dtw_nm: f64,
    /// Bearing of the leg itself (origin → destination), degrees true —
    /// NMEA's "bearing origin to destination".
    pub bod_deg: f64,
    /// Velocity made good toward the waypoint, knots. Needs SOG and COG.
    pub vmg_kt: Option<f64>,
    /// Distance to go to the end of the route, nautical miles: to the active
    /// waypoint, then along every leg after it.
    pub dtg_nm: f64,
    /// Speed over ground, knots — what time to the destination is reckoned
    /// from, since VMG toward this mark says nothing about the legs after it.
    pub sog_kt: Option<f64>,
    /// The weather router's planned arrival at the active waypoint, when the
    /// route came from it.
    pub wp_plan_eta: Option<chrono::DateTime<chrono::Utc>>,
    /// The planned arrival at the end of the route, likewise.
    pub dest_plan_eta: Option<chrono::DateTime<chrono::Utc>>,
    /// Inside the arrival circle of the active waypoint right now.
    pub arrived: bool,
    /// The last waypoint's circle has been reached: the route is done.
    pub finished: bool,
}

impl Following {
    /// Begin following a route. `None` if it has fewer than two waypoints —
    /// there is nothing to steer along.
    pub fn start(route: &Route, config: FollowConfig) -> Option<Self> {
        (!route.legs.is_empty()).then_some(Self {
            route_id: route.id,
            leg: 0,
            config,
        })
    }

    /// The mark this follower is steering to, while the route still has it.
    pub fn target(&self, route: &Route) -> Option<Uuid> {
        route.legs.get(self.leg).map(|l| l.to)
    }

    /// The marks still ahead — the one being steered to, then each after
    /// it — with every mark the route has now. Taken before an edit and
    /// handed to [`retarget`](Self::retarget) after.
    pub fn marks_ahead(&self, route: &Route) -> MarksAhead {
        MarksAhead {
            ahead: route.legs.iter().skip(self.leg).map(|l| l.to).collect(),
            known: route.waypoints.clone(),
        }
    }

    /// Re-point the follower after the route's waypoint list was edited.
    ///
    /// A follower holds a leg *index*. Reordering, reversing or removing a
    /// waypoint renumbers the legs, so the index alone would quietly start
    /// guiding towards a mark the crew never chose — the worst kind of bug
    /// on a boat, because nothing looks wrong. Given the marks that were
    /// ahead ([`marks_ahead`](Self::marks_ahead)), this steers to the first
    /// of them the route still has: the same mark if it survived, the next
    /// one if it was deleted — never skipping to the end of the route.
    ///
    /// And a mark *added* just before that one comes first: a waypoint put
    /// in between the boat and the mark she was steering to is a dog-leg
    /// round something, and steering straight past it at the old mark is
    /// the one thing the crew asked not to do.
    pub fn retarget(&mut self, route: &Route, before: &MarksAhead) {
        let mut leg = before
            .ahead
            .iter()
            .find_map(|mark| route.legs.iter().position(|l| l.to == *mark))
            .unwrap_or(route.legs.len().saturating_sub(1));
        while leg > 0 && !before.known.contains(&route.legs[leg - 1].to) {
            leg -= 1;
        }
        self.leg = leg;
    }

    /// Advance the state with a new fix and report guidance.
    ///
    /// Waypoint advance happens here: entering the active waypoint's arrival
    /// circle moves to the next leg — repeatedly, so a cluster of close
    /// waypoints cannot wedge the follower a leg behind the boat. On the last
    /// waypoint the follower stays put and reports `finished`.
    pub fn update(
        &mut self,
        route: &Route,
        set: &WaypointSet,
        position: LatLon,
        sog_kt: Option<f64>,
        cog_deg: Option<f64>,
    ) -> Option<Guidance> {
        if route.id != self.route_id || route.legs.is_empty() {
            return None;
        }
        self.leg = self.leg.min(route.legs.len() - 1);

        // Advance through any arrival circles we are already inside.
        let mut finished = false;
        loop {
            let leg = &route.legs[self.leg];
            let to = set.get(leg.to)?;
            let (dist_m, _) = geo::range_bearing(position, to.position);
            let radius_nm = to
                .arrival_radius_nm
                .unwrap_or(self.config.default_arrival_radius_nm);
            let last = self.leg + 1 >= route.legs.len();
            // `<=` written so that a distance that is not a number (a mark
            // with a broken position) never counts as arrival.
            let inside = dist_m / METRES_PER_NM <= radius_nm;
            if !inside {
                // The spec's second trigger (§8.3): the perpendicular through
                // the waypoint is crossed. A boat tacking, or set by current,
                // that passes the mark outside its circle has still passed it.
                // Not for the last mark: the route ends by arriving, and
                // sailing past the end is something the crew should see.
                let passed = !last
                    && set
                        .get(leg.from)
                        .is_some_and(|from| passed_abeam(from.position, to.position, position));
                if !passed {
                    break;
                }
            } else if last {
                finished = true;
                break;
            }
            self.leg += 1;
        }

        let leg = &route.legs[self.leg];
        let from = set.get(leg.from)?;
        let to = set.get(leg.to)?;

        let (dtw_m, btw_rad) = geo::range_bearing(position, to.position);
        let bod_rad = match leg.kind {
            LegKind::GreatCircle => geo::range_bearing(from.position, to.position).1,
            LegKind::RhumbLine => super::model::rhumb(from.position, to.position).1,
        };
        let btw_deg = btw_rad.to_degrees();
        let dtw_nm = dtw_m / METRES_PER_NM;
        let radius_nm = to
            .arrival_radius_nm
            .unwrap_or(self.config.default_arrival_radius_nm);

        let xte_nm = match leg.kind {
            LegKind::GreatCircle => cross_track_nm(from.position, to.position, position),
            LegKind::RhumbLine => rhumb_cross_track_nm(from.position, to.position, position),
        };

        let vmg_kt = match (sog_kt, cog_deg) {
            (Some(sog), Some(cog)) => Some(sog * (cog - btw_deg).to_radians().cos()),
            _ => None,
        };

        let dtg_nm = dtw_nm
            + route.legs[self.leg + 1..]
                .iter()
                .map(|l| l.distance_nm)
                .sum::<f64>();

        Some(Guidance {
            leg: self.leg,
            from: leg.from,
            to: leg.to,
            to_name: to.name.clone(),
            xte_nm,
            btw_deg,
            dtw_nm,
            bod_deg: bod_rad.to_degrees(),
            vmg_kt,
            dtg_nm,
            sog_kt,
            wp_plan_eta: leg.plan.as_ref().map(|p| p.eta),
            dest_plan_eta: route.legs.last().and_then(|l| l.plan.as_ref()).map(|p| p.eta),
            arrived: dtw_nm <= radius_nm,
            finished,
        })
    }
}

/// Whether `at` lies beyond the line through `to` at right angles to the leg
/// `from → to`: seen from the mark, the boat and the leg's start are on
/// opposite sides. A leg with no length has no such line.
fn passed_abeam(from: LatLon, to: LatLon, at: LatLon) -> bool {
    let (leg_m, back) = geo::range_bearing(to, from);
    let (off_m, out) = geo::range_bearing(to, at);
    leg_m > 1.0 && off_m.is_finite() && (out - back).cos() < 0.0
}

/// Signed cross-track distance from the leg `from → to`, in nautical miles.
/// Positive to starboard of the leg.
///
/// The classic spherical cross-track formula on the mean radius, fed with
/// *geodesic* ranges and azimuths. XTE is a small quantity — miles, not
/// hundreds of miles — and at these scales the spherical transform of two
/// geodesic measurements is accurate to well under a metre; the leg azimuth
/// itself, where the real accuracy lives, is Karney's.
fn cross_track_nm(from: LatLon, to: LatLon, at: LatLon) -> f64 {
    const R: f64 = 6_371_008.8;
    let (d13_m, theta13) = geo::range_bearing(from, at);
    let (_, theta12) = geo::range_bearing(from, to);
    let xte_m = ((d13_m / R).sin() * (theta13 - theta12).sin()).asin() * R;
    xte_m / METRES_PER_NM
}

/// Signed distance from the rhumb line `from → to`, in nautical miles.
/// Positive to starboard of the leg.
///
/// A rhumb line is straight on the Mercator chart, so the distance is
/// measured there and brought back to the ground by the Mercator scale at
/// the boat, cos(latitude). Exact for the small distances XTE is about.
fn rhumb_cross_track_nm(from: LatLon, to: LatLon, at: LatLon) -> f64 {
    const R: f64 = 6_371_008.8;
    let north = |p: LatLon| (std::f64::consts::FRAC_PI_4 + p.lat.to_radians() / 2.0).tan().ln();
    // East of `from`, the short way round.
    let east = |p: LatLon| (p.lon - from.lon + 180.0).rem_euclid(360.0).to_radians() - std::f64::consts::PI;
    let (lx, ly) = (east(to), north(to) - north(from));
    let (px, py) = (east(at), north(at) - north(from));
    let length = lx.hypot(ly);
    if length < 1e-12 {
        return 0.0;
    }
    // x east, y north: a positive cross product puts the boat to port.
    let off = -(lx * py - ly * px) / length;
    off * at.lat.to_radians().cos() * R / METRES_PER_NM
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::model::Waypoint;

    /// A due-north leg in the Kattegat: from 56°N to 57°N along 11°E.
    fn north_leg() -> (Route, WaypointSet) {
        let mut set = WaypointSet::default();
        let mut route = Route::new("north");
        route
            .waypoints
            .push(set.insert(Waypoint::new("start", 56.0, 11.0)));
        route
            .waypoints
            .push(set.insert(Waypoint::new("end", 57.0, 11.0)));
        route.recompute_legs(&set);
        (route, set)
    }

    /// A four-mark route up the coast, for the editing cases.
    fn four_marks() -> (Route, WaypointSet) {
        let mut set = WaypointSet::default();
        let mut route = Route::new("four");
        for (i, name) in ["a", "b", "c", "d"].iter().enumerate() {
            route
                .waypoints
                .push(set.insert(Waypoint::new(*name, 56.0 + i as f64 * 0.2, 11.0)));
        }
        route.recompute_legs(&set);
        (route, set)
    }

    /// A 30 nm leg due east along 56°N. The rhumb line is the parallel
    /// itself; the great circle bulges north of it by d²·tan φ / 8R at the
    /// middle — 55 560² × tan 56° / (8 × 6 371 009) = 90 m. A boat on the
    /// parallel at mid-leg is on a rhumb leg, and 90 m to starboard of a
    /// great-circle one.
    #[test]
    fn a_rhumb_leg_is_followed_along_the_rhumb_line() {
        let dlon = 30.0 / (60.0 * 56f64.to_radians().cos());
        let mut set = WaypointSet::default();
        let mut route = Route::new("east");
        route.waypoints.push(set.insert(Waypoint::new("w", 56.0, 11.0)));
        route.waypoints.push(set.insert(Waypoint::new("e", 56.0, 11.0 + dlon)));
        route.recompute_legs(&set);
        let mid = LatLon::new(56.0, 11.0 + dlon / 2.0);

        let mut f = Following::start(&route, FollowConfig::default()).unwrap();
        let g = f.update(&route, &set, mid, None, None).unwrap();
        let great_m = g.xte_nm * METRES_PER_NM;
        assert!((great_m - 90.0).abs() < 2.0, "great circle: {great_m} m");

        route.make_legs_rhumb(&set);
        assert_eq!(route.legs[0].kind, LegKind::RhumbLine);
        let g = f.update(&route, &set, mid, None, None).unwrap();
        assert!((g.xte_nm * METRES_PER_NM).abs() < 0.5, "rhumb: {} m", g.xte_nm * METRES_PER_NM);
        assert!((g.bod_deg - 90.0).abs() < 1e-6, "{}", g.bod_deg);

        // And the sign and size hold: 100 m south of the parallel is 100 m
        // to starboard of an eastbound leg.
        let south = geo::advance(mid, 180f64.to_radians(), 100.0);
        let g = f.update(&route, &set, south, None, None).unwrap();
        assert!((g.xte_nm * METRES_PER_NM - 100.0).abs() < 0.5, "{}", g.xte_nm * METRES_PER_NM);
    }

    /// Spec §8.3: advance "when inside the arrival circle or when the
    /// perpendicular through the waypoint is crossed". Marks a-b-c-d lie on
    /// 11°E; b is at 56.2°N. The boat passes b 0.15 nm to the west, outside
    /// the default 0.1 nm circle.
    #[test]
    fn passing_a_mark_outside_its_circle_still_advances() {
        let (route, set) = four_marks();
        let b = LatLon::new(56.2, 11.0);
        let abeam = geo::advance(b, 270f64.to_radians(), 0.15 * METRES_PER_NM);

        // 0.05 nm short of the line through b: still steering to b.
        let mut f = Following::start(&route, FollowConfig::default()).unwrap();
        let before = geo::advance(abeam, 180f64.to_radians(), 0.05 * METRES_PER_NM);
        let g = f.update(&route, &set, before, None, None).unwrap();
        assert_eq!(g.to_name, "b");

        // 0.05 nm beyond it: b is astern, the mark is now c.
        let after = geo::advance(abeam, 0f64.to_radians(), 0.05 * METRES_PER_NM);
        let g = f.update(&route, &set, after, None, None).unwrap();
        assert_eq!(g.to_name, "c");
        assert_eq!(f.leg, 1);
        assert!(!g.finished);
    }

    /// Sailing past the last mark outside its circle does not end the route:
    /// only arriving does.
    #[test]
    fn passing_the_last_mark_outside_its_circle_does_not_finish() {
        let (route, set) = north_leg();
        let mut f = Following::start(&route, FollowConfig::default()).unwrap();
        let end = LatLon::new(57.0, 11.0);
        let past = geo::advance(
            geo::advance(end, 270f64.to_radians(), 0.15 * METRES_PER_NM),
            0.0,
            0.05 * METRES_PER_NM,
        );
        let g = f.update(&route, &set, past, None, None).unwrap();
        assert_eq!(g.to_name, "end");
        assert!(!g.finished && !g.arrived);
    }

    /// Editing the route under a follower must not silently retarget it.
    /// The crew chose a mark, not a leg number.
    #[test]
    fn editing_the_route_keeps_the_follower_on_its_mark() {
        let (mut route, set) = four_marks();
        let mut f = Following::start(&route, FollowConfig::default()).unwrap();
        f.leg = 2; // steering to "d", the last mark
        let target = f.target(&route).expect("a target");
        assert_eq!(set.get(target).unwrap().name, "d");
        let ahead = f.marks_ahead(&route);

        // Drop "b" from the middle: "d" is now the end of leg 1.
        route.waypoints.remove(1);
        route.recompute_legs(&set);
        f.retarget(&route, &ahead);
        assert_eq!(f.leg, 1);
        assert_eq!(set.get(f.target(&route).unwrap()).unwrap().name, "d");

        // Reverse the whole route: "d" leads it, so no leg ends at it and
        // the follower lands on the last leg rather than off the end.
        route.waypoints.reverse();
        route.recompute_legs(&set);
        f.retarget(&route, &ahead);
        assert_eq!(f.leg, route.legs.len() - 1, "leg {} of {}", f.leg, route.legs.len());

        // Delete the mark we were steering to: land on the last leg, and
        // never index past it.
        let (mut route, set) = four_marks();
        let mut f = Following::start(&route, FollowConfig::default()).unwrap();
        f.leg = 1;
        let target = f.target(&route).unwrap();
        let ahead = f.marks_ahead(&route);
        let gone = route.waypoints.iter().position(|w| *w == target).unwrap();
        route.waypoints.remove(gone);
        route.recompute_legs(&set);
        f.retarget(&route, &ahead);
        assert_eq!(f.leg, route.legs.len() - 1);
        assert!(f.update(&route, &set, LatLon::new(56.1, 11.0), None, None).is_some());
    }

    /// On a-b-c-d, steering to b, the crew puts X between a and b to go
    /// round something. The boat steers to X, then b — not straight at b.
    #[test]
    fn a_mark_added_before_the_target_is_steered_to_first() {
        let (mut route, mut set) = four_marks();
        let mut f = Following::start(&route, FollowConfig::default()).unwrap();
        assert_eq!(set.get(f.target(&route).unwrap()).unwrap().name, "b");
        let before = f.marks_ahead(&route);
        let x = set.insert(Waypoint::new("x", 56.1, 11.05));
        route.waypoints.insert(1, x);
        route.recompute_legs(&set);
        f.retarget(&route, &before);
        assert_eq!(set.get(f.target(&route).unwrap()).unwrap().name, "x");

        // Two added in a row: the first of them.
        let before = f.marks_ahead(&route);
        let y = set.insert(Waypoint::new("y", 56.05, 11.05));
        let z = set.insert(Waypoint::new("z", 56.07, 11.05));
        route.waypoints.insert(1, z);
        route.waypoints.insert(1, y);
        route.recompute_legs(&set);
        f.retarget(&route, &before);
        assert_eq!(set.get(f.target(&route).unwrap()).unwrap().name, "y");

        // A mark added *after* the target changes nothing…
        let (mut route, mut set) = four_marks();
        let mut f = Following::start(&route, FollowConfig::default()).unwrap();
        let before = f.marks_ahead(&route);
        let later = set.insert(Waypoint::new("later", 56.3, 11.05));
        route.waypoints.insert(2, later);
        route.recompute_legs(&set);
        f.retarget(&route, &before);
        assert_eq!(set.get(f.target(&route).unwrap()).unwrap().name, "b");

        // …and nor does one put ahead of the whole route while steering to
        // c: the marks between are old ones, already passed.
        let (mut route, mut set) = four_marks();
        let mut f = Following::start(&route, FollowConfig::default()).unwrap();
        f.leg = 1;
        let before = f.marks_ahead(&route);
        let first = set.insert(Waypoint::new("first", 55.9, 11.0));
        route.waypoints.insert(0, first);
        route.recompute_legs(&set);
        f.retarget(&route, &before);
        assert_eq!(set.get(f.target(&route).unwrap()).unwrap().name, "c");
    }

    /// Deleting the mark being steered to moves on to the *next* mark, not
    /// the last one: on a-b-c-d, heading for b, dropping b steers to c.
    #[test]
    fn deleting_the_target_steers_to_the_next_mark_not_the_last() {
        let (mut route, set) = four_marks();
        let mut f = Following::start(&route, FollowConfig::default()).unwrap();
        assert_eq!(set.get(f.target(&route).unwrap()).unwrap().name, "b");
        let ahead = f.marks_ahead(&route);
        route.waypoints.remove(1);
        route.recompute_legs(&set);
        f.retarget(&route, &ahead);
        assert_eq!(set.get(f.target(&route).unwrap()).unwrap().name, "c");
    }

    /// Down to a single mark there are no legs at all; the follower must not
    /// index into an empty list.
    #[test]
    fn a_route_edited_down_to_one_mark_does_not_panic() {
        let (mut route, set) = four_marks();
        let mut f = Following::start(&route, FollowConfig::default()).unwrap();
        f.leg = 2;
        let ahead = f.marks_ahead(&route);
        route.waypoints.truncate(1);
        route.recompute_legs(&set);
        f.retarget(&route, &ahead);
        assert_eq!(f.leg, 0);
        assert!(f.update(&route, &set, LatLon::new(56.0, 11.0), None, None).is_none());
    }

    /// The spec's own M2 verification: a track 0.2 nm to port of the leg
    /// reads XTE = 0.2 nm with the documented sign.
    #[test]
    fn a_boat_to_port_reads_negative_two_tenths() {
        let (route, set) = north_leg();
        let mut f = Following::start(&route, FollowConfig::default()).unwrap();

        // Halfway up the leg, displaced 0.2 nm due west. The leg runs north,
        // so west is the port side.
        let on_track = LatLon::new(56.5, 11.0);
        let displaced = geo::advance(on_track, 270f64.to_radians(), 0.2 * METRES_PER_NM);
        let g = f.update(&route, &set, displaced, None, None).unwrap();

        assert!(
            (g.xte_nm.abs() - 0.2).abs() < 0.002,
            "magnitude: {}",
            g.xte_nm
        );
        assert!(g.xte_nm < 0.0, "port must be negative, got {}", g.xte_nm);

        // And the mirror image: starboard positive.
        let displaced = geo::advance(on_track, 90f64.to_radians(), 0.2 * METRES_PER_NM);
        let g = f.update(&route, &set, displaced, None, None).unwrap();
        assert!(g.xte_nm > 0.0, "starboard must be positive");
        assert!((g.xte_nm - 0.2).abs() < 0.002);

        // Dead on track reads zero.
        let g = f.update(&route, &set, on_track, None, None).unwrap();
        assert!(g.xte_nm.abs() < 1e-6, "{}", g.xte_nm);
    }

    #[test]
    fn the_arrival_circle_advances_the_waypoint() {
        let mut set = WaypointSet::default();
        let mut route = Route::new("three");
        for (n, lat) in [("a", 56.0), ("b", 56.5), ("c", 57.0)] {
            route.waypoints.push(set.insert(Waypoint::new(n, lat, 11.0)));
        }
        route.recompute_legs(&set);
        let mut f = Following::start(&route, FollowConfig::default()).unwrap();

        // Approaching b, just outside the 0.1 nm circle: still leg 0. (Close
        // to the edge on purpose — a circle measured in kilometres or
        // metres instead of miles would put the boat on the wrong side.)
        let near_b = geo::advance(
            LatLon::new(56.5, 11.0),
            180f64.to_radians(),
            0.11 * METRES_PER_NM,
        );
        let g = f.update(&route, &set, near_b, None, None).unwrap();
        assert_eq!(g.leg, 0);
        assert_eq!(g.to_name, "b");
        assert!(!g.arrived && !g.finished);

        // Just inside the circle: the follower moves to the next leg in the
        // same update — the guidance already points at c.
        let at_b = geo::advance(
            LatLon::new(56.5, 11.0),
            180f64.to_radians(),
            0.09 * METRES_PER_NM,
        );
        let g = f.update(&route, &set, at_b, None, None).unwrap();
        assert_eq!(g.leg, 1);
        assert_eq!(g.to_name, "c");
        assert!(!g.arrived && !g.finished, "arrival is judged against c now");
        // Drifting back out of b's circle does not undo the advance.
        let g = f.update(&route, &set, near_b, None, None).unwrap();
        assert_eq!(g.to_name, "c");

        // Inside the final circle: finished, and it stays finished.
        let at_c = LatLon::new(57.0, 11.0);
        let g = f.update(&route, &set, at_c, None, None).unwrap();
        assert!(g.finished);
        assert!(g.arrived);
        assert_eq!(g.leg, 1, "the last leg remains the last leg");
    }

    #[test]
    fn a_cluster_of_waypoints_is_advanced_through_in_one_update() {
        // Three marks within 0.05 nm of each other, then a real leg. One fix
        // inside all three circles must land the follower on the real leg,
        // not one leg per second for three seconds.
        let mut set = WaypointSet::default();
        let mut route = Route::new("cluster");
        let base = LatLon::new(56.0, 11.0);
        route.waypoints.push(set.insert(Waypoint::new("s", 55.9, 11.0)));
        for (i, d) in [0.0, 0.02, 0.04].iter().enumerate() {
            let p = geo::advance(base, 0.0, d * METRES_PER_NM);
            route
                .waypoints
                .push(set.insert(Waypoint::new(format!("c{i}"), p.lat, p.lon)));
        }
        route.waypoints.push(set.insert(Waypoint::new("far", 56.5, 11.0)));
        route.recompute_legs(&set);

        let mut f = Following::start(&route, FollowConfig::default()).unwrap();
        let g = f.update(&route, &set, base, None, None).unwrap();
        assert_eq!(g.to_name, "far", "advanced through the whole cluster");
    }

    #[test]
    fn a_per_waypoint_radius_overrides_the_default() {
        let mut set = WaypointSet::default();
        let mut route = Route::new("wide");
        route.waypoints.push(set.insert(Waypoint::new("s", 56.0, 11.0)));
        let mut wide = Waypoint::new("wide", 56.5, 11.0);
        wide.arrival_radius_nm = Some(0.5);
        route.waypoints.push(set.insert(wide));
        route.waypoints.push(set.insert(Waypoint::new("e", 57.0, 11.0)));
        route.recompute_legs(&set);

        let mut f = Following::start(&route, FollowConfig::default()).unwrap();
        // 0.4 nm short of the mark: outside the default circle, inside the
        // widened one — must advance.
        let near = geo::advance(
            LatLon::new(56.5, 11.0),
            180f64.to_radians(),
            0.4 * METRES_PER_NM,
        );
        let g = f.update(&route, &set, near, None, None).unwrap();
        assert_eq!(g.to_name, "e");
    }

    #[test]
    fn vmg_is_the_closing_speed() {
        let (route, set) = north_leg();
        let mut f = Following::start(&route, FollowConfig::default()).unwrap();
        let at = LatLon::new(56.5, 11.0);

        // Sailing straight at the waypoint: VMG = SOG.
        let g = f.update(&route, &set, at, Some(6.0), Some(0.0)).unwrap();
        assert!((g.vmg_kt.unwrap() - 6.0).abs() < 0.01);

        // Sailing at 60° to it: half.
        let g = f.update(&route, &set, at, Some(6.0), Some(60.0)).unwrap();
        assert!((g.vmg_kt.unwrap() - 3.0).abs() < 0.01);

        // Sailing away: negative.
        let g = f.update(&route, &set, at, Some(6.0), Some(180.0)).unwrap();
        assert!((g.vmg_kt.unwrap() + 6.0).abs() < 0.01);

        // No SOG/COG: no VMG, not a made-up zero.
        let g = f.update(&route, &set, at, None, None).unwrap();
        assert!(g.vmg_kt.is_none());
    }

    #[test]
    fn bod_is_the_legs_own_bearing_not_the_boats() {
        let (route, set) = north_leg();
        let mut f = Following::start(&route, FollowConfig::default()).unwrap();
        // Displaced east: BTW points north-west-ish, BOD stays north.
        let at = geo::advance(
            LatLon::new(56.5, 11.0),
            90f64.to_radians(),
            2.0 * METRES_PER_NM,
        );
        let g = f.update(&route, &set, at, None, None).unwrap();
        assert!((g.bod_deg).abs() < 0.5 || (g.bod_deg - 360.0).abs() < 0.5);
        assert!(g.btw_deg > 270.0 && g.btw_deg < 360.0, "{}", g.btw_deg);
    }

    #[test]
    fn distance_to_go_runs_to_the_end_of_the_route() {
        let (route, set) = four_marks();
        let mut f = Following::start(&route, FollowConfig::default()).unwrap();
        // Halfway from a to b; each leg is 0.2° of latitude, about 12 NM.
        let g = f
            .update(&route, &set, LatLon::new(56.1, 11.0), Some(5.0), Some(0.0))
            .unwrap();
        // 0.1° to b, then 0.2° and 0.2° more: half a degree of latitude.
        assert!((g.dtw_nm - 6.0).abs() < 0.05, "{}", g.dtw_nm);
        assert!((g.dtg_nm - 30.0).abs() < 0.2, "{}", g.dtg_nm);
        assert_eq!(g.sog_kt, Some(5.0));

        // On the last leg, distance to go is distance to the waypoint.
        f.leg = 2;
        let g = f
            .update(&route, &set, LatLon::new(56.5, 11.0), None, None)
            .unwrap();
        assert!((g.dtg_nm - g.dtw_nm).abs() < 1e-9);
    }

    /// A leg index left past the end — the route was shortened and nobody
    /// retargeted — steers the last leg, not off the end of the list.
    #[test]
    fn a_leg_index_past_the_end_steers_the_last_leg() {
        let (route, set) = four_marks();
        let mut f = Following::start(&route, FollowConfig::default()).unwrap();
        f.leg = 7;
        let g = f
            .update(&route, &set, LatLon::new(56.1, 11.0), None, None)
            .expect("guidance on the last leg");
        assert_eq!(g.leg, 2);
        assert_eq!(g.to_name, "d");
        assert_eq!(f.leg, 2);
    }

    /// Exactly on the arrival circle is inside it: `arrived` is
    /// `dtw <= radius`, and on the last mark arriving is finishing. The two
    /// flags must not disagree on the circle's edge.
    #[test]
    fn exactly_on_the_final_circle_is_arrived_and_finished() {
        let mut set = WaypointSet::default();
        let mut route = Route::new("edge");
        route.waypoints.push(set.insert(Waypoint::new("start", 56.0, 11.0)));
        let fix = LatLon::new(56.9, 11.02);
        let (d_m, _) = geo::range_bearing(fix, LatLon::new(57.0, 11.0));
        let mut end = Waypoint::new("end", 57.0, 11.0);
        // The circle drawn through the fix itself.
        end.arrival_radius_nm = Some(d_m / METRES_PER_NM);
        route.waypoints.push(set.insert(end));
        route.recompute_legs(&set);

        let mut f = Following::start(&route, FollowConfig::default()).unwrap();
        let g = f.update(&route, &set, fix, None, None).unwrap();
        assert!(g.arrived, "on the circle is inside it");
        assert!(g.finished, "arrived at the last mark but not finished");
    }

    /// An eastbound leg along the equator, from 0°E to 1°E, so the mark
    /// bears exactly 090° from anywhere on it.
    fn east_leg() -> (Route, WaypointSet) {
        let mut set = WaypointSet::default();
        let mut route = Route::new("east");
        route.waypoints.push(set.insert(Waypoint::new("west", 0.0, 0.0)));
        route.waypoints.push(set.insert(Waypoint::new("east", 0.0, 1.0)));
        route.recompute_legs(&set);
        (route, set)
    }

    /// VMG is SOG times the cosine of the angle between the course and the
    /// bearing to the mark. With the mark at 090° (not 000°, where adding
    /// and subtracting the bearing agree): course 090° closes at full speed,
    /// 030° and 150° at half, 270° opens at full speed.
    #[test]
    fn vmg_measures_the_course_against_the_bearing_to_the_mark() {
        let (route, set) = east_leg();
        let mut f = Following::start(&route, FollowConfig::default()).unwrap();
        let at = LatLon::new(0.0, 0.5);
        let g = f.update(&route, &set, at, Some(6.0), Some(90.0)).unwrap();
        assert!((g.btw_deg - 90.0).abs() < 1e-6, "btw {}", g.btw_deg);
        for (cog, want) in [(90.0, 6.0), (30.0, 3.0), (150.0, 3.0), (270.0, -6.0)] {
            let g = f.update(&route, &set, at, Some(6.0), Some(cog)).unwrap();
            let vmg = g.vmg_kt.unwrap();
            assert!((vmg - want).abs() < 0.01, "cog {cog}: vmg {vmg}, want {want}");
        }
    }

    /// Looking east along an eastbound leg, north is to port: negative XTE.
    /// The distance from the equator to 0.1°N is the meridian arc, 0.1° on
    /// a meridian radius of a(1 − e²) = 6 335 439 m: 11 057 m, 5.971 NM.
    /// (A due-north leg cannot tell the leg's bearing added from subtracted;
    /// an eastbound one can.)
    #[test]
    fn north_of_an_eastbound_leg_is_port() {
        let (route, set) = east_leg();
        let mut f = Following::start(&route, FollowConfig::default()).unwrap();
        let arc_nm = 6_335_439.0 * 0.1f64.to_radians() / METRES_PER_NM;
        let g = f.update(&route, &set, LatLon::new(0.1, 0.5), None, None).unwrap();
        assert!((g.xte_nm + arc_nm).abs() < 0.01, "north: {} want {}", g.xte_nm, -arc_nm);
        let g = f.update(&route, &set, LatLon::new(-0.1, 0.5), None, None).unwrap();
        assert!((g.xte_nm - arc_nm).abs() < 0.01, "south: {} want {}", g.xte_nm, arc_nm);
    }

    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Anywhere on the globe (away from the poles), on any leg: a boat
        /// put a known distance to starboard of a point on the leg reads
        /// that distance, positive; the same distance to port reads it
        /// negative; and the distance off a line can never exceed the
        /// distance to either of the line's ends.
        #[test]
        fn xte_changes_sign_across_the_track_and_never_exceeds_the_range_to_either_end(
            lat in -60.0f64..60.0,
            lon in -170.0f64..170.0,
            leg_deg in 0.0f64..360.0,
            leg_nm in 1.0f64..50.0,
            frac in 0.1f64..0.9,
            off_nm in 0.01f64..2.0,
        ) {
            use std::f64::consts::FRAC_PI_2;
            let from = LatLon::new(lat, lon);
            let to = geo::advance(from, leg_deg.to_radians(), leg_nm * METRES_PER_NM);
            let on = geo::advance(from, leg_deg.to_radians(), frac * leg_nm * METRES_PER_NM);
            // The leg's own direction at this point, which on a geodesic
            // is not the direction it started in.
            let (_, track) = geo::range_bearing(on, to);
            let stbd = geo::advance(on, track + FRAC_PI_2, off_nm * METRES_PER_NM);
            let port = geo::advance(on, track - FRAC_PI_2, off_nm * METRES_PER_NM);
            let xs = cross_track_nm(from, to, stbd);
            let xp = cross_track_nm(from, to, port);
            prop_assert!(xs > 0.0, "starboard read {xs}");
            prop_assert!(xp < 0.0, "port read {xp}");
            prop_assert!((xs - off_nm).abs() < 0.01 * off_nm + 1e-4, "{xs} for {off_nm}");
            prop_assert!((xp + off_nm).abs() < 0.01 * off_nm + 1e-4, "{xp} for {off_nm}");
            for (p, x) in [(stbd, xs), (port, xp)] {
                let d_from = geo::distance_m(from, p) / METRES_PER_NM;
                let d_to = geo::distance_m(to, p) / METRES_PER_NM;
                // 1% for the sphere-versus-ellipsoid difference in scale.
                prop_assert!(x.abs() <= d_from.min(d_to) * 1.01, "|{x}| vs {d_from}, {d_to}");
            }
        }
    }
}
