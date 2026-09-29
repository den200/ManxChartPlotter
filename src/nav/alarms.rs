//! Watch-keeping alarms: what is wrong, since when, and whether anyone has
//! heard it yet.
//!
//! A plotter is watched now and then, not stared at, so an alarm that only
//! changes a colour is an alarm nobody hears. Every alarm here rings — the
//! caller sounds [`Alarms::due`] — until someone silences it; silencing stops
//! the sound but not the alarm, which stays on screen until the condition
//! clears. A condition that clears and comes back rings again.
//!
//! The checks are plain functions of the boat's state, so each is tested
//! without a boat.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::geo::LatLon;

/// What an alarm is about. Collision alarms are one per target, so a second
/// ship closing in rings even after the first was silenced.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AlarmKey {
    ManOverboard,
    Anchor,
    Depth,
    Collision(String),
}

impl AlarmKey {
    /// A man overboard outranks everything; the banner lists it first.
    pub fn title(&self) -> &'static str {
        match self {
            AlarmKey::ManOverboard => "MAN OVERBOARD",
            AlarmKey::Anchor => "ANCHOR",
            AlarmKey::Depth => "SHALLOW",
            AlarmKey::Collision(_) => "COLLISION RISK",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Alarm {
    pub message: String,
    pub since: Instant,
    /// Silenced: no more sound, still shown.
    pub acknowledged: bool,
}

/// How often a ringing alarm sounds again.
pub const REPEAT: Duration = Duration::from_secs(3);

#[derive(Debug, Default)]
pub struct Alarms {
    active: BTreeMap<AlarmKey, Alarm>,
    last_sound: Option<Instant>,
}

impl Alarms {
    /// Raise (or keep, with the message brought up to date) an alarm while
    /// `message` is `Some`; clear it when `None`. Keeping an alarm keeps its
    /// silenced state — a depth that keeps changing must not ring anew with
    /// every reading.
    pub fn set(&mut self, key: AlarmKey, message: Option<String>, now: Instant) {
        match message {
            Some(message) => {
                self.active
                    .entry(key)
                    .and_modify(|a| a.message.clone_from(&message))
                    .or_insert(Alarm { message, since: now, acknowledged: false });
            }
            None => {
                self.active.remove(&key);
            }
        }
    }

    /// The collision alarms, all at once: every target in `dangerous` is
    /// raised (id, message); every collision alarm not in it clears.
    pub fn set_collisions(&mut self, dangerous: Vec<(String, String)>, now: Instant) {
        let ids: std::collections::HashSet<&str> =
            dangerous.iter().map(|(id, _)| id.as_str()).collect();
        self.active.retain(|k, _| match k {
            AlarmKey::Collision(id) => ids.contains(id.as_str()),
            _ => true,
        });
        for (id, message) in dangerous {
            self.set(AlarmKey::Collision(id), Some(message), now);
        }
    }

    /// Silence everything ringing now. What is raised later still rings.
    pub fn acknowledge(&mut self) {
        for a in self.active.values_mut() {
            a.acknowledged = true;
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&AlarmKey, &Alarm)> {
        self.active.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    /// Something is raised that nobody has silenced.
    pub fn ringing(&self) -> bool {
        self.active.values().any(|a| !a.acknowledged)
    }

    /// Time to sound again: ringing, and [`REPEAT`] since the last sound.
    /// Records the sound, so ask once per chance to play it.
    pub fn due(&mut self, now: Instant) -> bool {
        if !self.ringing() {
            return false;
        }
        if self.last_sound.is_some_and(|t| now.duration_since(t) < REPEAT) {
            return false;
        }
        self.last_sound = Some(now);
        true
    }
}

/// Anchor watch: the boat has left the circle, or the watch has lost the
/// position it keeps. `fix` is the boat's position and whether it is fresh.
pub fn anchor_check(anchor: LatLon, radius_m: f64, fix: Option<(LatLon, bool)>) -> Option<String> {
    match fix {
        Some((at, true)) => {
            let d = crate::geo::distance_m(anchor, at);
            (d > radius_m).then(|| {
                format!("the boat is {d:.0} m from the anchor — outside the {radius_m:.0} m circle")
            })
        }
        // A watch that cannot see the boat is not watching.
        _ => Some("no position fix — the anchor watch cannot see the boat".into()),
    }
}

/// Depth under the boat below the limit. `None` depth (nothing heard, or
/// stale) raises nothing: a sounder that is switched off is not shallow
/// water, and the chart's own safety contour still shows.
pub fn depth_check(depth_m: Option<f64>, limit_m: f64) -> Option<String> {
    let d = depth_m?;
    (d < limit_m).then(|| format!("{d:.1} m under the boat — below the {limit_m:.1} m alarm"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn an_alarm_rings_until_silenced_and_shows_until_cleared() {
        let now = t0();
        let mut a = Alarms::default();
        assert!(!a.ringing());
        a.set(AlarmKey::Depth, Some("2.1 m".into()), now);
        assert!(a.ringing());
        assert!(a.due(now));
        assert!(!a.due(now + Duration::from_secs(1)), "not again straight away");
        assert!(a.due(now + REPEAT));
        a.acknowledge();
        assert!(!a.ringing());
        assert!(!a.is_empty(), "silenced, still shown");
        // A new reading updates the words, not the silence.
        a.set(AlarmKey::Depth, Some("1.9 m".into()), now);
        assert!(!a.ringing());
        assert_eq!(a.iter().next().unwrap().1.message, "1.9 m");
        a.set(AlarmKey::Depth, None, now);
        assert!(a.is_empty());
        // Back again: rings again.
        a.set(AlarmKey::Depth, Some("2.0 m".into()), now);
        assert!(a.ringing());
    }

    #[test]
    fn a_second_ship_rings_after_the_first_was_silenced() {
        let now = t0();
        let mut a = Alarms::default();
        a.set_collisions(vec![("ferry".into(), "0.1 nm in 4 min".into())], now);
        a.acknowledge();
        a.set_collisions(
            vec![("ferry".into(), "0.1 nm in 3 min".into()), ("tanker".into(), "0.2 nm".into())],
            now,
        );
        assert!(a.ringing());
        // The ferry passes: its alarm clears, the tanker's stays.
        a.set_collisions(vec![("tanker".into(), "0.2 nm".into())], now);
        let keys: Vec<_> = a.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(keys, vec![AlarmKey::Collision("tanker".into())]);
    }

    #[test]
    fn man_overboard_is_listed_first() {
        let now = t0();
        let mut a = Alarms::default();
        a.set(AlarmKey::Collision("x".into()), Some("m".into()), now);
        a.set(AlarmKey::ManOverboard, Some("m".into()), now);
        assert_eq!(a.iter().next().unwrap().0, &AlarmKey::ManOverboard);
    }

    #[test]
    fn the_anchor_watch_rings_outside_the_circle_and_without_a_fix() {
        let anchor = LatLon::new(55.0, 11.0);
        let near = LatLon::new(55.0002, 11.0); // ~22 m north
        let far = LatLon::new(55.0010, 11.0); // ~111 m
        assert!(anchor_check(anchor, 40.0, Some((near, true))).is_none());
        assert!(anchor_check(anchor, 40.0, Some((far, true))).is_some());
        assert!(anchor_check(anchor, 40.0, Some((near, false))).is_some(), "stale fix");
        assert!(anchor_check(anchor, 40.0, None).is_some());
    }

    #[test]
    fn depth_rings_below_the_limit_only_when_there_is_a_depth() {
        assert!(depth_check(Some(1.8), 2.5).is_some());
        assert!(depth_check(Some(3.0), 2.5).is_none());
        assert!(depth_check(None, 2.5).is_none());
    }
}
