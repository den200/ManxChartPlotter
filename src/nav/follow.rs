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

use super::model::{Route, WaypointSet};
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
            if dist_m / METRES_PER_NM > radius_nm {
                break;
            }
            if self.leg + 1 >= route.legs.len() {
                finished = true;
                break;
            }
            self.leg += 1;
        }

        let leg = &route.legs[self.leg];
        let from = set.get(leg.from)?;
        let to = set.get(leg.to)?;

        let (dtw_m, btw_rad) = geo::range_bearing(position, to.position);
        let (_, bod_rad) = geo::range_bearing(from.position, to.position);
        let btw_deg = btw_rad.to_degrees();
        let dtw_nm = dtw_m / METRES_PER_NM;
        let radius_nm = to
            .arrival_radius_nm
            .unwrap_or(self.config.default_arrival_radius_nm);

        let xte_nm = cross_track_nm(from.position, to.position, position);

        let vmg_kt = match (sog_kt, cog_deg) {
            (Some(sog), Some(cog)) => Some(sog * (cog - btw_deg).to_radians().cos()),
            _ => None,
        };

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
            arrived: dtw_nm <= radius_nm,
            finished,
        })
    }
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

        // Approaching b but outside 0.1 nm: still leg 0.
        let near_b = geo::advance(
            LatLon::new(56.5, 11.0),
            180f64.to_radians(),
            0.3 * METRES_PER_NM,
        );
        let g = f.update(&route, &set, near_b, None, None).unwrap();
        assert_eq!(g.leg, 0);
        assert_eq!(g.to_name, "b");
        assert!(!g.arrived && !g.finished);

        // Inside the circle: the follower moves to the next leg in the same
        // update — the guidance already points at c.
        let at_b = geo::advance(
            LatLon::new(56.5, 11.0),
            180f64.to_radians(),
            0.05 * METRES_PER_NM,
        );
        let g = f.update(&route, &set, at_b, None, None).unwrap();
        assert_eq!(g.leg, 1);
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
}
