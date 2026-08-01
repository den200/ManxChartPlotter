//! Signal K: where navcore gets everything that is not a chart.
//!
//! Position, heading, depth, wind, tanks and batteries all arrive over one
//! WebSocket as a stream of deltas. navcore reads that stream and nothing
//! else — no NMEA parsing, no driver per instrument — because Signal K has
//! already done that work and every modern boat network can speak it.
//!
//! The layering:
//!
//! - [`delta`] turns a message into path/value pairs
//! - [`state`] folds those into the vessel's current state, with arrival times
//! - [`fleet`] routes them to our boat or to an AIS target, by context
//! - [`catalog`] says what the well-known paths mean
//! - [`units`] turns SI into something a mariner reads
//! - [`service`] owns the socket, on its own thread, and reconnects
//! - [`resources`] is the HTTP side: route resources published and consumed
//! - [`sim`] is a boat that isn't there, for testing away from the water
//!
//! Nothing here knows about egui or wgpu; the instrument bar is a view of
//! [`state::Vessel`] and could be replaced without touching any of it.

pub mod catalog;
pub mod delta;
pub mod fleet;
pub mod resources;
pub mod service;
pub mod sim;
pub mod state;
pub mod units;

pub use service::{SignalKService, Status};
pub use fleet::{Fleet, Target};
pub use state::Vessel;
pub use units::{Quantity, UnitPrefs};
