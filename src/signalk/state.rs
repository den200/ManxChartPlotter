//! What the boat is doing, as last heard.
//!
//! Every path the server sends is kept, whether navcore knows what it means or
//! not: an unrecognised path is still something a user may want on the bar,
//! and a plotter that silently discards half a boat's instruments is worse
//! than one that shows them raw.
//!
//! Each reading carries when it arrived. Stale data on a chart is dangerous in
//! a way that missing data is not — a heading frozen ten minutes ago still
//! looks like a heading — so age is recorded at the point of receipt and the
//! display is expected to say when a value has gone quiet.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::delta::{Delta, Value};

/// How long before a reading is treated as stale.
///
/// Position and heading update at least once a second on any real installation;
/// several seconds of silence means the sensor, the bus or the server has
/// stopped, and the number on screen is a memory rather than a measurement.
pub const STALE_AFTER: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Reading {
    pub value: Value,
    pub at: Instant,
}

impl Reading {
    pub fn age(&self) -> Duration {
        self.at.elapsed()
    }
    pub fn is_stale(&self) -> bool {
        self.age() > STALE_AFTER
    }
    pub fn number(&self) -> Option<f64> {
        self.value.as_number()
    }
}

/// The vessel's current state, by path.
#[derive(Debug, Clone, Default)]
pub struct Vessel {
    /// Sorted, so the path list a user picks from does not reshuffle itself
    /// between frames as readings arrive.
    readings: BTreeMap<String, Reading>,
}

impl Vessel {
    pub fn apply(&mut self, delta: &Delta) {
        let now = Instant::now();
        for update in &delta.updates {
            self.readings.insert(
                update.path.clone(),
                Reading {
                    value: update.value.clone(),
                    at: now,
                },
            );
        }
    }

    pub fn get(&self, path: &str) -> Option<&Reading> {
        self.readings.get(path)
    }

    pub fn number(&self, path: &str) -> Option<f64> {
        self.readings.get(path).and_then(Reading::number)
    }

    /// Every path heard so far, in a stable order.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.readings.keys().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.readings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.readings.is_empty()
    }

    /// Where the boat is, if it has said.
    pub fn position(&self) -> Option<(f64, f64)> {
        match self.readings.get("navigation.position").map(|r| &r.value) {
            Some(Value::Position { lat, lon }) => Some((*lat, *lon)),
            _ => None,
        }
    }

    /// Which way it is pointing, in radians. True heading if the boat reports
    /// it, otherwise magnetic corrected by the reported variation, otherwise
    /// course over ground — which is not the same thing but is better than an
    /// arrow that will not turn.
    pub fn heading_true(&self) -> Option<f64> {
        if let Some(h) = self.number("navigation.headingTrue") {
            return Some(h);
        }
        if let Some(m) = self.number("navigation.headingMagnetic") {
            let variation = self.number("navigation.magneticVariation").unwrap_or(0.0);
            return Some(m + variation);
        }
        self.number("navigation.courseOverGroundTrue")
    }

    pub fn clear(&mut self) {
        self.readings.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signalk::delta;

    fn apply(v: &mut Vessel, json: &str) {
        v.apply(&delta::parse(json).expect("a delta"));
    }

    #[test]
    fn readings_land_and_the_latest_wins() {
        let mut v = Vessel::default();
        apply(&mut v, r#"{"updates":[{"values":[
            {"path":"navigation.speedOverGround","value":3.42}]}]}"#);
        assert_eq!(v.number("navigation.speedOverGround"), Some(3.42));
        apply(&mut v, r#"{"updates":[{"values":[
            {"path":"navigation.speedOverGround","value":4.0}]}]}"#);
        assert_eq!(v.number("navigation.speedOverGround"), Some(4.0));
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn unknown_paths_are_kept_because_someone_may_want_them() {
        let mut v = Vessel::default();
        apply(&mut v, r#"{"updates":[{"values":[
            {"path":"propulsion.port.coolantTemperature","value":355.0},
            {"path":"some.vendor.extension","value":"odd"}]}]}"#);
        assert_eq!(v.len(), 2);
        assert!(v.paths().any(|p| p == "some.vendor.extension"));
    }

    #[test]
    fn position_is_read_back_as_a_pair() {
        let mut v = Vessel::default();
        assert_eq!(v.position(), None);
        apply(&mut v, r#"{"updates":[{"values":[
            {"path":"navigation.position","value":{"latitude":55.68,"longitude":12.57}}]}]}"#);
        assert_eq!(v.position(), Some((55.68, 12.57)));
    }

    #[test]
    fn heading_falls_back_through_magnetic_to_course() {
        let mut v = Vessel::default();
        assert_eq!(v.heading_true(), None);

        // Course over ground is the last resort — not a heading, but it turns.
        apply(&mut v, r#"{"updates":[{"values":[
            {"path":"navigation.courseOverGroundTrue","value":1.0}]}]}"#);
        assert_eq!(v.heading_true(), Some(1.0));

        // Magnetic plus variation beats it.
        apply(&mut v, r#"{"updates":[{"values":[
            {"path":"navigation.headingMagnetic","value":2.0},
            {"path":"navigation.magneticVariation","value":0.1}]}]}"#);
        assert_eq!(v.heading_true(), Some(2.1));

        // True beats everything.
        apply(&mut v, r#"{"updates":[{"values":[
            {"path":"navigation.headingTrue","value":3.0}]}]}"#);
        assert_eq!(v.heading_true(), Some(3.0));
    }

    #[test]
    fn a_fresh_reading_is_not_stale() {
        let mut v = Vessel::default();
        apply(&mut v, r#"{"updates":[{"values":[
            {"path":"navigation.headingTrue","value":3.0}]}]}"#);
        let r = v.get("navigation.headingTrue").expect("the reading");
        assert!(!r.is_stale());
        assert!(r.age() < STALE_AFTER);
    }

    #[test]
    fn paths_come_back_sorted_so_the_picker_does_not_reshuffle() {
        let mut v = Vessel::default();
        apply(&mut v, r#"{"updates":[{"values":[
            {"path":"navigation.speedOverGround","value":1.0},
            {"path":"environment.depth.belowKeel","value":2.0},
            {"path":"navigation.headingTrue","value":3.0}]}]}"#);
        let paths: Vec<_> = v.paths().collect();
        assert_eq!(
            paths,
            vec![
                "environment.depth.belowKeel",
                "navigation.headingTrue",
                "navigation.speedOverGround"
            ]
        );
    }
}
