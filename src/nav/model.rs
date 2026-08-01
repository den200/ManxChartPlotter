//! What a passage is made of: waypoints, routes and legs.
//!
//! This is the routing spec's §4 data model, faithfully — geographic positions
//! are `f64` degrees WGS84 everywhere, ids are UUIDs, legs are derived from
//! the waypoint sequence rather than stored facts of their own. One addition
//! the spec's interchange rule forces: waypoints and routes carry the raw XML
//! of any *foreign* GPX extensions they arrived with, so a route that came
//! from OpenCPN goes back to OpenCPN with OpenCPN's own bookkeeping intact.

use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use uuid::Uuid;

use crate::geo::{self, LatLon, METRES_PER_NM};

/// A named position.
#[derive(Debug, Clone, PartialEq)]
pub struct Waypoint {
    pub id: Uuid,
    pub name: String,
    pub position: LatLon,
    /// GPX `<sym>`, e.g. `diamond`.
    pub symbol: Option<String>,
    pub description: Option<String>,
    /// Overrides the global arrival circle for this waypoint.
    pub arrival_radius_nm: Option<f64>,
    /// Foreign GPX `<extensions>` children, verbatim — another plotter's
    /// bookkeeping, preserved so a round-trip loses nothing. Never navcore's
    /// own elements; those are parsed into the fields above and re-emitted.
    pub foreign_extensions: String,
}

impl Waypoint {
    pub fn new(name: impl Into<String>, lat: f64, lon: f64) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            position: LatLon::new(lat, lon),
            symbol: None,
            description: None,
            arrival_radius_nm: None,
            foreign_extensions: String::new(),
        }
    }
}

/// The waypoints a store knows, by id.
///
/// A `BTreeMap` rather than a hash map so iteration order is stable: the
/// waypoints file writes in the same order every save, and diffs of it mean
/// something.
#[derive(Debug, Clone, Default)]
pub struct WaypointSet {
    by_id: BTreeMap<Uuid, Waypoint>,
}

impl WaypointSet {
    pub fn insert(&mut self, wp: Waypoint) -> Uuid {
        let id = wp.id;
        self.by_id.insert(id, wp);
        id
    }
    pub fn get(&self, id: Uuid) -> Option<&Waypoint> {
        self.by_id.get(&id)
    }
    pub fn get_mut(&mut self, id: Uuid) -> Option<&mut Waypoint> {
        self.by_id.get_mut(&id)
    }
    pub fn remove(&mut self, id: Uuid) -> Option<Waypoint> {
        self.by_id.remove(&id)
    }
    pub fn iter(&self) -> impl Iterator<Item = &Waypoint> {
        self.by_id.values()
    }
    pub fn len(&self) -> usize {
        self.by_id.len()
    }
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

/// How a leg is sailed, which decides how its distance and bearing are figured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum LegKind {
    /// The shortest path. Distance and initial bearing from the geodesic.
    #[default]
    GreatCircle,
    /// Constant bearing. Over the leg lengths a small boat sails the
    /// difference is centimetres; the kind exists because a mariner sometimes
    /// *wants* the constant-bearing leg, not because the maths differ much.
    RhumbLine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TackState {
    Port,
    Starboard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PointOfSail {
    CloseHauled,
    CloseReach,
    BeamReach,
    BroadReach,
    Run,
}

/// What the weather router planned for one leg. Absent on hand-drawn routes.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LegPlan {
    pub eta: DateTime<Utc>,
    pub twa_deg: f64,
    pub tws_kt: f64,
    pub point_of_sail: PointOfSail,
    pub expected_stw_kt: f64,
    pub tack_state: TackState,
}

/// One step of a route, derived from its two endpoints.
#[derive(Debug, Clone, PartialEq)]
pub struct Leg {
    pub from: Uuid,
    pub to: Uuid,
    pub kind: LegKind,
    pub distance_nm: f64,
    pub initial_bearing_deg: f64,
    /// Populated only by the weather router.
    pub plan: Option<LegPlan>,
}

/// How an auto-generated route came to be, kept for reproducibility.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RouteProvenance {
    pub generated_at: DateTime<Utc>,
    pub grib_source: Option<String>,
    pub grib_run: Option<DateTime<Utc>>,
    pub polar: String,
    pub engine_params: RoutingConfig,
}

/// The routing engine's knobs, with the defaults the spec derives in §8.3.
/// Defaults, not constants — every one is meant to be user-configurable.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RoutingConfig {
    pub dt_coastal_s: u32,
    pub dt_offshore_s: u32,
    pub heading_step_deg: f32,
    pub max_diverted_deg: f32,
    pub tack_penalty_day_s: u32,
    pub tack_penalty_night_s: u32,
    pub gybe_penalty_day_s: u32,
    pub gybe_penalty_night_s: u32,
    pub n_sectors: u32,
    pub arrival_radius_nm: f64,
    pub reroute_margin: f64,
    pub max_tws_kt: f64,
    pub max_wave_m: f64,
    pub offing_min_nm: f64,
    pub simplify_tolerance_nm: f64,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            dt_coastal_s: 3600,
            dt_offshore_s: 10800,
            heading_step_deg: 5.0,
            max_diverted_deg: 100.0,
            tack_penalty_day_s: 240,
            tack_penalty_night_s: 600,
            gybe_penalty_day_s: 240,
            gybe_penalty_night_s: 600,
            n_sectors: 256,
            arrival_radius_nm: 0.1,
            reroute_margin: 0.02,
            max_tws_kt: 35.0,
            max_wave_m: 4.0,
            offing_min_nm: 0.2,
            simplify_tolerance_nm: 0.1,
        }
    }
}

/// An ordered passage through waypoints.
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub id: Uuid,
    pub name: String,
    /// Ordered. The legs are derived from this sequence.
    pub waypoints: Vec<Uuid>,
    pub legs: Vec<Leg>,
    /// `Some` when the route was generated rather than drawn.
    pub generated: Option<RouteProvenance>,
    /// Foreign GPX route-level extensions, verbatim. See [`Waypoint`].
    pub foreign_extensions: String,
}

impl Route {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            waypoints: Vec::new(),
            legs: Vec::new(),
            generated: None,
            foreign_extensions: String::new(),
        }
    }

    /// Rebuild the legs from the waypoint sequence.
    ///
    /// Existing [`LegPlan`]s survive only where a leg still joins the same two
    /// waypoints — editing the middle of a route must not leave stale plans
    /// claiming ETAs for legs that no longer exist.
    pub fn recompute_legs(&mut self, set: &WaypointSet) {
        let old: Vec<Leg> = std::mem::take(&mut self.legs);
        self.legs = self
            .waypoints
            .windows(2)
            .filter_map(|pair| {
                let (from, to) = (pair[0], pair[1]);
                let (a, b) = (set.get(from)?, set.get(to)?);
                let kind = old
                    .iter()
                    .find(|l| l.from == from && l.to == to)
                    .map(|l| l.kind)
                    .unwrap_or_default();
                let (distance_nm, initial_bearing_deg) =
                    leg_measure(kind, a.position, b.position);
                let plan = old
                    .iter()
                    .find(|l| l.from == from && l.to == to)
                    .and_then(|l| l.plan.clone());
                Some(Leg {
                    from,
                    to,
                    kind,
                    distance_nm,
                    initial_bearing_deg,
                    plan,
                })
            })
            .collect();
    }

    pub fn total_distance_nm(&self) -> f64 {
        self.legs.iter().map(|l| l.distance_nm).sum()
    }
}

/// Distance in nautical miles and initial bearing in degrees for one leg.
pub fn leg_measure(kind: LegKind, from: LatLon, to: LatLon) -> (f64, f64) {
    match kind {
        LegKind::GreatCircle => {
            let (m, bearing) = geo::range_bearing(from, to);
            (m / METRES_PER_NM, bearing.to_degrees())
        }
        LegKind::RhumbLine => {
            let (m, bearing) = rhumb(from, to);
            (m / METRES_PER_NM, bearing.to_degrees())
        }
    }
}

/// Rhumb-line distance (metres) and constant bearing (radians).
///
/// Spherical, on the IUGG mean radius — within ~0.3 % of the ellipsoid, which
/// is fine for a leg table; the geodesic in [`crate::geo`] stays the measure
/// wherever accuracy matters. The standard loxodrome formulas: the bearing
/// comes from the meridional-parts stretch, and the east–west term uses the
/// mean-latitude correction `q`.
fn rhumb(a: LatLon, b: LatLon) -> (f64, f64) {
    const R: f64 = 6_371_008.8;
    let phi1 = a.lat.to_radians();
    let phi2 = b.lat.to_radians();
    let dphi = phi2 - phi1;
    // Wrap Δλ to ±π: a rhumb line never goes the long way round.
    let mut dlambda = (b.lon - a.lon).to_radians();
    if dlambda > std::f64::consts::PI {
        dlambda -= 2.0 * std::f64::consts::PI;
    } else if dlambda < -std::f64::consts::PI {
        dlambda += 2.0 * std::f64::consts::PI;
    }

    let dpsi = ((std::f64::consts::FRAC_PI_4 + phi2 / 2.0).tan()
        / (std::f64::consts::FRAC_PI_4 + phi1 / 2.0).tan())
    .ln();
    // Along a parallel Δψ → 0 and the ratio degenerates; cos φ is its limit.
    let q = if dpsi.abs() > 1e-12 { dphi / dpsi } else { phi1.cos() };

    let distance = (dphi * dphi + q * q * dlambda * dlambda).sqrt() * R;
    let bearing = dlambda.atan2(dpsi).rem_euclid(std::f64::consts::TAU);
    (distance, bearing)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_of(points: &[(&str, f64, f64)]) -> (WaypointSet, Vec<Uuid>) {
        let mut set = WaypointSet::default();
        let ids = points
            .iter()
            .map(|(n, lat, lon)| set.insert(Waypoint::new(*n, *lat, *lon)))
            .collect();
        (set, ids)
    }

    #[test]
    fn legs_derive_from_the_waypoint_sequence() {
        let (set, ids) = set_of(&[
            ("start", 56.0, 11.0),
            ("mid", 56.5, 11.5),
            ("finish", 57.0, 11.0),
        ]);
        let mut route = Route::new("test");
        route.waypoints = ids.clone();
        route.recompute_legs(&set);

        assert_eq!(route.legs.len(), 2);
        assert_eq!(route.legs[0].from, ids[0]);
        assert_eq!(route.legs[0].to, ids[1]);
        // Each leg is roughly 35 nm (half a degree of lat and lon at 56 N),
        // and the total is their sum.
        assert!((30.0..40.0).contains(&route.legs[0].distance_nm));
        let total = route.total_distance_nm();
        assert!((total - route.legs[0].distance_nm - route.legs[1].distance_nm).abs() < 1e-9);
        // First leg heads north-east, second north-west.
        assert!((0.0..90.0).contains(&route.legs[0].initial_bearing_deg));
        assert!((270.0..360.0).contains(&route.legs[1].initial_bearing_deg));
    }

    #[test]
    fn editing_the_route_does_not_leave_stale_plans() {
        let (set, ids) = set_of(&[("a", 56.0, 11.0), ("b", 56.5, 11.5), ("c", 57.0, 11.0)]);
        let mut route = Route::new("test");
        route.waypoints = ids.clone();
        route.recompute_legs(&set);
        route.legs[0].plan = Some(LegPlan {
            eta: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
            twa_deg: 45.0,
            tws_kt: 12.0,
            point_of_sail: PointOfSail::CloseHauled,
            expected_stw_kt: 6.0,
            tack_state: TackState::Starboard,
        });

        // Removing the middle waypoint destroys both old legs; the plan for
        // a leg that no longer exists must not survive onto the new one.
        route.waypoints.remove(1);
        route.recompute_legs(&set);
        assert_eq!(route.legs.len(), 1);
        assert!(route.legs[0].plan.is_none());

        // But an untouched leg keeps its plan.
        let mut route = Route::new("again");
        route.waypoints = ids.clone();
        route.recompute_legs(&set);
        route.legs[1].plan = Some(LegPlan {
            eta: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
            twa_deg: 90.0,
            tws_kt: 10.0,
            point_of_sail: PointOfSail::BeamReach,
            expected_stw_kt: 6.5,
            tack_state: TackState::Port,
        });
        route.recompute_legs(&set);
        assert!(route.legs[1].plan.is_some());
    }

    #[test]
    fn rhumb_agrees_with_the_geodesic_where_they_must() {
        // Along a meridian the rhumb line IS the geodesic.
        let a = LatLon::new(55.0, 11.0);
        let b = LatLon::new(57.0, 11.0);
        let (rm, rb) = rhumb(a, b);
        let (gm, _) = geo::range_bearing(a, b);
        assert!((rm - gm).abs() / gm < 0.005, "rhumb {rm} vs geodesic {gm}");
        assert!(rb.to_degrees() < 0.01 || rb.to_degrees() > 359.99);

        // Due east along a parallel: bearing exactly 090, distance the
        // parallel's arc, and the Δψ→0 degenerate branch exercised.
        let a = LatLon::new(56.0, 11.0);
        let b = LatLon::new(56.0, 12.0);
        let (rm, rb) = rhumb(a, b);
        assert!((rb.to_degrees() - 90.0).abs() < 1e-9);
        let expected = 6_371_008.8 * (56f64).to_radians().cos() * (1f64).to_radians();
        assert!((rm - expected).abs() < 1.0, "{rm} vs {expected}");

        // Crossing the antimeridian goes the short way, west.
        let (_, rb) = rhumb(LatLon::new(0.0, 179.5), LatLon::new(0.0, -179.5));
        assert!((rb.to_degrees() - 90.0).abs() < 1e-9, "{}", rb.to_degrees());
    }

    #[test]
    fn the_routing_defaults_are_the_specs() {
        let c = RoutingConfig::default();
        assert_eq!(c.dt_coastal_s, 3600);
        assert_eq!(c.heading_step_deg, 5.0);
        assert_eq!(c.n_sectors, 256);
        assert_eq!(c.arrival_radius_nm, 0.1);
        assert_eq!(c.reroute_margin, 0.02);
        // And the whole thing serializes, because provenance embeds it.
        let json = serde_json::to_string(&c).unwrap();
        let back: RoutingConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }
}
