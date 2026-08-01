//! Waypoints, routes and — in later milestones — the router that writes them.
//!
//! The layering, and where each milestone of the routing spec lands:
//!
//! - [`model`] — the §4 data model: waypoints, routes, derived legs (M1)
//! - [`gpx`] — GPX 1.1 interchange, lossless through other plotters (M1)
//! - [`store`] — a directory of GPX files that *is* the store (M1)
//! - [`follow`] — XTE, bearings, arrival advance: the active-route logic (M2)
//! - [`autoroute`] — the land-avoiding grid router; owns confined waters (M3)
//!
//! Steering data leaves navcore over **Signal K only** — Den's call, no NMEA
//! double work. The route resources side lives in `signalk::resources`.
//!
//! Nothing in here draws. The chart rendering of routes is a separate task
//! that consumes this data model, which is exactly how the spec scopes it.

pub mod autoroute;
pub mod follow;
pub mod gpx;
pub mod model;
pub mod store;

pub use model::{Leg, LegKind, LegPlan, Route, RouteProvenance, RoutingConfig, Waypoint, WaypointSet};
pub use follow::{FollowConfig, Following, Guidance};
pub use store::RouteStore;
