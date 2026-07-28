//! Our boat, and everyone else's.
//!
//! Signal K carries AIS traffic as deltas that are structurally identical to
//! our own — same paths, different `context`. Routing on that context is the
//! whole of the difference between a plotter and a plotter whose boat marker
//! teleports around the harbour.
//!
//! A target is stored as an ordinary [`Vessel`], so everything the instrument
//! layer already knows how to read applies to it unchanged: the same units,
//! the same staleness rules, the same catalogue of paths.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::delta::Delta;
use super::state::Vessel;

/// How long a target survives without a report before it is treated as lost.
///
/// A class A transponder reports every few seconds under way and every three
/// minutes at anchor; class B is slower still. Six minutes is long enough not
/// to drop a moored vessel, short enough that a ship which switched off does
/// not sit on the chart pretending to be there.
pub const LOST_AFTER: Duration = Duration::from_secs(360);

/// How long a lost target is kept before it is forgotten entirely, so it can
/// be drawn crossed through rather than simply vanishing — a target that
/// disappears silently is indistinguishable from one that was never there.
pub const FORGET_AFTER: Duration = Duration::from_secs(600);

/// One other vessel.
#[derive(Debug, Clone)]
pub struct Target {
    /// Its Signal K context, `vessels.urn:mrn:imo:mmsi:244060807`.
    pub context: String,
    pub vessel: Vessel,
    pub last_report: Instant,
}

impl Target {
    /// The vessel's name, if it has broadcast one.
    pub fn name(&self) -> Option<&str> {
        self.vessel.text("name")
    }

    /// MMSI, taken from the path if given and otherwise from the context,
    /// which encodes it: `vessels.urn:mrn:imo:mmsi:244060807`.
    pub fn mmsi(&self) -> Option<&str> {
        self.vessel
            .text("mmsi")
            .or_else(|| self.context.rsplit(':').next().filter(|s| s.len() >= 7))
    }

    /// What to call it on the chart: its name, else its MMSI, else nothing.
    pub fn label(&self) -> Option<&str> {
        self.name().or_else(|| self.mmsi())
    }

    pub fn position(&self) -> Option<(f64, f64)> {
        self.vessel.position()
    }

    /// Course over ground in radians, falling back to heading — a target that
    /// reports only heading still has an orientation worth drawing.
    pub fn course(&self) -> Option<f64> {
        self.vessel
            .number("navigation.courseOverGroundTrue")
            .or_else(|| self.vessel.heading_true())
    }

    pub fn speed(&self) -> Option<f64> {
        self.vessel.number("navigation.speedOverGround")
    }

    /// The hull's orientation: heading if known, else course. A vessel making
    /// leeway is not pointing where it is going, and when we know both the
    /// hull should show the former.
    pub fn heading(&self) -> Option<f64> {
        self.vessel.heading_true().or_else(|| self.course())
    }

    /// Nothing heard for long enough that the target should not be trusted.
    pub fn is_lost(&self) -> bool {
        self.last_report.elapsed() > LOST_AFTER
    }

    /// Under way, as opposed to moored, anchored or aground.
    ///
    /// Worth knowing before drawing a course vector: a moored ship with a
    /// stale course would otherwise sprout a vector across the harbour.
    pub fn under_way(&self) -> bool {
        match self.vessel.text("navigation.state") {
            Some(s) => !matches!(
                s,
                "moored" | "anchored" | "not under command" | "aground"
            ),
            None => self.speed().unwrap_or(0.0) > 0.5,
        }
    }
}

/// Everything on the water.
#[derive(Debug, Default)]
pub struct Fleet {
    /// Our own boat.
    pub own: Vessel,
    targets: BTreeMap<String, Target>,
    /// What the server calls our vessel, learned from its greeting.
    self_context: Option<String>,
}

impl Fleet {
    pub fn set_self_context(&mut self, context: String) {
        // Anything already filed under our own context was us, misrouted
        // because the greeting had not arrived yet. Left in place it would sit
        // on the chart for ever as a target shadowing our own boat, reporting
        // our own position — and nothing would ever expire it, because it goes
        // on being updated.
        if let Some(mistaken) = self.targets.remove(&context) {
            self.own.merge(&mistaken.vessel);
        }
        self.self_context = Some(context);
    }

    pub fn self_context(&self) -> Option<&str> {
        self.self_context.as_deref()
    }

    /// Route a delta to whichever vessel it is about.
    pub fn apply(&mut self, delta: &Delta) {
        if delta.is_self(self.self_context.as_deref()) {
            self.own.apply(delta);
            return;
        }
        let Some(context) = delta.context.clone() else {
            return;
        };
        let target = self.targets.entry(context.clone()).or_insert_with(|| Target {
            context,
            vessel: Vessel::default(),
            last_report: Instant::now(),
        });
        target.vessel.apply(delta);
        target.last_report = Instant::now();
    }

    /// Targets worth drawing: everything still remembered, lost or not.
    pub fn targets(&self) -> impl Iterator<Item = &Target> {
        self.targets.values()
    }

    /// Drop targets nothing has been heard from in a long time.
    ///
    /// Called from the render loop rather than on receipt, because a fleet
    /// that only shrinks when a *different* vessel reports would keep a lone
    /// ghost for ever on a quiet sea.
    pub fn forget_stale(&mut self) {
        self.targets
            .retain(|_, t| t.last_report.elapsed() <= FORGET_AFTER);
    }

    pub fn len(&self) -> usize {
        self.targets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    pub fn clear(&mut self) {
        self.own.clear();
        self.targets.clear();
        self.self_context = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signalk::delta;

    fn delta_for(context: Option<&str>, body: &str) -> Delta {
        let json = match context {
            Some(c) => format!(r#"{{"context":"{c}","updates":[{{"values":[{body}]}}]}}"#),
            None => format!(r#"{{"updates":[{{"values":[{body}]}}]}}"#),
        };
        delta::parse(&json).expect("a delta")
    }

    #[test]
    fn our_boat_and_theirs_go_to_different_places() {
        let mut fleet = Fleet::default();
        fleet.set_self_context("vessels.urn:mrn:signalk:uuid:mine".into());

        fleet.apply(&delta_for(
            Some("vessels.urn:mrn:signalk:uuid:mine"),
            r#"{"path":"navigation.speedOverGround","value":3.0}"#,
        ));
        fleet.apply(&delta_for(
            Some("vessels.urn:mrn:imo:mmsi:244060807"),
            r#"{"path":"navigation.speedOverGround","value":9.0}"#,
        ));

        assert_eq!(fleet.own.number("navigation.speedOverGround"), Some(3.0));
        assert_eq!(fleet.len(), 1);
        let target = fleet.targets().next().expect("one target");
        assert_eq!(target.speed(), Some(9.0));
    }

    #[test]
    fn a_delta_with_no_context_is_ours() {
        let mut fleet = Fleet::default();
        fleet.apply(&delta_for(
            None,
            r#"{"path":"navigation.speedOverGround","value":3.0}"#,
        ));
        assert_eq!(fleet.own.number("navigation.speedOverGround"), Some(3.0));
        assert!(fleet.is_empty());
    }

    #[test]
    fn a_target_is_named_by_its_name_then_its_mmsi() {
        let mut fleet = Fleet::default();
        let ctx = "vessels.urn:mrn:imo:mmsi:244060807";
        fleet.apply(&delta_for(
            Some(ctx),
            r#"{"path":"navigation.position","value":{"latitude":56.5,"longitude":11.6}}"#,
        ));
        let target = fleet.targets().next().unwrap();
        // With nothing else, the MMSI comes out of the context itself.
        assert_eq!(target.mmsi(), Some("244060807"));
        assert_eq!(target.label(), Some("244060807"));
        assert_eq!(target.position(), Some((56.5, 11.6)));

        fleet.apply(&delta_for(Some(ctx), r#"{"path":"name","value":"NORDLYS"}"#));
        let target = fleet.targets().next().unwrap();
        assert_eq!(target.name(), Some("NORDLYS"));
        assert_eq!(target.label(), Some("NORDLYS"));
    }

    #[test]
    fn under_way_prefers_what_the_vessel_says_over_its_speed() {
        let mut fleet = Fleet::default();
        let ctx = "vessels.urn:mrn:imo:mmsi:1";
        // Moored, but with a stale speed that would otherwise draw a vector.
        fleet.apply(&delta_for(
            Some(ctx),
            r#"{"path":"navigation.speedOverGround","value":4.0},
               {"path":"navigation.state","value":"moored"}"#,
        ));
        assert!(!fleet.targets().next().unwrap().under_way());

        // Nothing said: fall back to whether it is actually moving.
        let mut fleet = Fleet::default();
        fleet.apply(&delta_for(
            Some(ctx),
            r#"{"path":"navigation.speedOverGround","value":4.0}"#,
        ));
        assert!(fleet.targets().next().unwrap().under_way());

        let mut fleet = Fleet::default();
        fleet.apply(&delta_for(
            Some(ctx),
            r#"{"path":"navigation.speedOverGround","value":0.1}"#,
        ));
        assert!(!fleet.targets().next().unwrap().under_way());
    }

    #[test]
    fn a_hull_points_along_heading_and_falls_back_to_course() {
        let mut fleet = Fleet::default();
        let ctx = "vessels.urn:mrn:imo:mmsi:2";
        fleet.apply(&delta_for(
            Some(ctx),
            r#"{"path":"navigation.courseOverGroundTrue","value":1.0}"#,
        ));
        let t = fleet.targets().next().unwrap();
        assert_eq!(t.course(), Some(1.0));
        assert_eq!(t.heading(), Some(1.0));

        fleet.apply(&delta_for(
            Some(ctx),
            r#"{"path":"navigation.headingTrue","value":1.2}"#,
        ));
        let t = fleet.targets().next().unwrap();
        assert_eq!(t.heading(), Some(1.2), "heading wins for the hull");
        assert_eq!(t.course(), Some(1.0), "course is still the course");
    }

    #[test]
    fn a_fresh_target_is_neither_lost_nor_forgotten() {
        let mut fleet = Fleet::default();
        fleet.apply(&delta_for(
            Some("vessels.urn:mrn:imo:mmsi:3"),
            r#"{"path":"navigation.speedOverGround","value":1.0}"#,
        ));
        assert!(!fleet.targets().next().unwrap().is_lost());
        fleet.forget_stale();
        assert_eq!(fleet.len(), 1);
    }

    #[test]
    fn a_greeting_that_arrives_late_reclaims_our_own_boat() {
        // The greeting is normally the first message, but a delta that beats
        // it would file our own boat as a target — one that shadows us on the
        // chart for ever, because it goes on being updated and so never
        // expires.
        let mut fleet = Fleet::default();
        let ours = "vessels.urn:mrn:signalk:uuid:mine";
        fleet.apply(&delta_for(
            Some(ours),
            r#"{"path":"navigation.speedOverGround","value":3.0}"#,
        ));
        assert_eq!(fleet.len(), 1, "misfiled, as expected, before the greeting");

        fleet.set_self_context(ours.into());
        assert!(fleet.is_empty(), "the ghost is gone");
        assert_eq!(
            fleet.own.number("navigation.speedOverGround"),
            Some(3.0),
            "and its readings came with it"
        );
    }

    #[test]
    fn clearing_takes_the_self_context_with_it() {
        // Otherwise a reconnection to a different server would route our own
        // boat's deltas into the target list.
        let mut fleet = Fleet::default();
        fleet.set_self_context("vessels.a".into());
        fleet.apply(&delta_for(
            Some("vessels.b"),
            r#"{"path":"navigation.speedOverGround","value":1.0}"#,
        ));
        fleet.clear();
        assert!(fleet.is_empty());
        assert_eq!(fleet.self_context(), None);
        assert!(fleet.own.is_empty());
    }
}
