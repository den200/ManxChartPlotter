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

    /// As [`set`](Self::set), for a reading that hovers at its limit: an
    /// alarm already raised is cleared only once the reading is well clear
    /// of the limit, not the moment it is back on the right side. `held` is
    /// what to say while it is on the right side but not yet clear, and
    /// `None` once it is.
    ///
    /// Without that, a sounder reading 2.4 / 2.6 m in a swell against a
    /// 2.5 m limit, or a boat swinging on the edge of her anchor circle,
    /// clears and re-raises the alarm with every swing — and each time it
    /// comes back un-silenced and rings again.
    pub fn set_held(&mut self, key: AlarmKey, message: Option<String>, held: Option<String>, now: Instant) {
        let raised = self.active.contains_key(&key);
        self.set(key, message.or(held.filter(|_| raised)), now);
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

/// Inside the anchor circle, but not by enough to clear an alarm already
/// raised: within 5 m of it (a fifth of the radius on a short scope). See
/// [`Alarms::set_held`].
pub fn anchor_held(anchor: LatLon, radius_m: f64, fix: Option<(LatLon, bool)>) -> Option<String> {
    let (at, true) = fix? else { return None };
    let d = crate::geo::distance_m(anchor, at);
    (d > radius_m - (radius_m * 0.2).min(5.0))
        .then(|| format!("the boat is {d:.0} m from the anchor — at the edge of the {radius_m:.0} m circle"))
}

/// Above the depth limit, but not by enough to clear an alarm already
/// raised: within 0.3 m of it, or a tenth of the limit if that is more.
/// No depth at all holds nothing: a sounder switched off is not shallow
/// water.
pub fn depth_held(depth_m: Option<f64>, limit_m: f64) -> Option<String> {
    let d = depth_m?;
    (d < limit_m + (limit_m * 0.1).max(0.3))
        .then(|| format!("{d:.1} m under the boat — barely above the {limit_m:.1} m alarm"))
}

/// Range (metres) and true bearing (radians) from the boat back to a man
/// overboard mark. `fix` is the boat's position and whether it is fresh.
///
/// Nothing without a fresh fix: a bearing reckoned from where the boat was
/// when the GPS dropped out reads as live and is wrong, and the panel then
/// says to steer by the spotter instead.
pub fn way_back(mark: Option<LatLon>, fix: Option<(LatLon, bool)>) -> Option<(f64, f64)> {
    Some(crate::geo::range_bearing(live(fix)?, mark?))
}

/// The boat's position if it is fresh, and nothing if it is only the last
/// one heard. For anything that says "here" or "from the boat": a logbook
/// note, the start of a passage plan, the way back to a man overboard.
pub fn live(fix: Option<(LatLon, bool)>) -> Option<LatLon> {
    fix.filter(|(_, fresh)| *fresh).map(|(at, _)| at)
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

    /// A sounder reading 2.4 / 2.6 m in a swell against a 2.5 m limit is
    /// one shallow patch, not a new alarm at every wave: silenced, it stays
    /// silenced until the depth is clear by the margin (0.3 m here).
    #[test]
    fn a_reading_hovering_at_the_limit_does_not_ring_again() {
        let now = Instant::now();
        let mut alarms = Alarms::default();
        let feed = |alarms: &mut Alarms, depth: f64| {
            alarms.set_held(
                AlarmKey::Depth,
                depth_check(Some(depth), 2.5),
                depth_held(Some(depth), 2.5),
                now,
            );
        };
        feed(&mut alarms, 2.4);
        assert!(alarms.ringing());
        alarms.acknowledge();
        for depth in [2.6, 2.4, 2.7, 2.45, 2.79] {
            feed(&mut alarms, depth);
            assert!(!alarms.is_empty(), "still raised at {depth} m");
            assert!(!alarms.ringing(), "rang again at {depth} m");
        }
        // Clear by the margin: gone, and the next shoal is a new alarm.
        feed(&mut alarms, 2.8);
        assert!(alarms.is_empty());
        feed(&mut alarms, 2.4);
        assert!(alarms.ringing());
    }

    /// Never raised, never held: a reading just above the limit raises
    /// nothing.
    #[test]
    fn the_margin_only_holds_an_alarm_already_raised() {
        let mut alarms = Alarms::default();
        alarms.set_held(AlarmKey::Depth, depth_check(Some(2.6), 2.5), depth_held(Some(2.6), 2.5), Instant::now());
        assert!(alarms.is_empty());
        // No depth at all holds nothing.
        assert_eq!(depth_held(None, 2.5), None);
    }

    /// A 40 m circle clears at 35 m; a 10 m circle at 8 m (a fifth of it).
    /// One metre of latitude is 1/1852 of a minute.
    #[test]
    fn the_anchor_alarm_clears_five_metres_inside_the_circle() {
        let anchor = LatLon::new(56.0, 11.0);
        let north = |m: f64| LatLon::new(56.0 + m / 1852.0 / 60.0, 11.0);
        assert!(anchor_held(anchor, 40.0, Some((north(37.0), true))).is_some());
        assert!(anchor_held(anchor, 40.0, Some((north(33.0), true))).is_none());
        assert!(anchor_held(anchor, 10.0, Some((north(9.0), true))).is_some());
        assert!(anchor_held(anchor, 10.0, Some((north(7.0), true))).is_none());
        // Without a fresh fix the alarm itself speaks; there is nothing to hold.
        assert!(anchor_held(anchor, 40.0, Some((north(37.0), false))).is_none());
        assert!(anchor_check(anchor, 40.0, Some((north(37.0), false))).is_some());

        // Raised for a lost fix, the alarm does not keep saying so once the
        // fix is back: at the edge it says where the boat is.
        let now = Instant::now();
        let mut alarms = Alarms::default();
        let feed = |alarms: &mut Alarms, fix| {
            alarms.set_held(AlarmKey::Anchor, anchor_check(anchor, 40.0, fix), anchor_held(anchor, 40.0, fix), now);
        };
        feed(&mut alarms, None);
        feed(&mut alarms, Some((north(37.0), true)));
        let (_, a) = alarms.iter().next().expect("held at the edge");
        assert!(a.message.contains("37 m"), "{}", a.message);
        feed(&mut alarms, Some((north(20.0), true)));
        assert!(alarms.is_empty());
    }

    /// One minute of latitude is one nautical mile: a mark 0.1' north of
    /// the boat is 185 m away, due north.
    #[test]
    fn the_way_back_needs_a_fresh_fix() {
        let mark = LatLon::new(56.0 + 0.1 / 60.0, 11.0);
        let boat = LatLon::new(56.0, 11.0);
        let (d, b) = way_back(Some(mark), Some((boat, true))).unwrap();
        assert!((d - 185.2).abs() < 0.5, "{d}");
        assert!(b.to_degrees().rem_euclid(360.0).min(360.0 - b.to_degrees().rem_euclid(360.0)) < 0.01);
        // The same position, gone stale: no bearing to steer by.
        assert_eq!(way_back(Some(mark), Some((boat, false))), None);
        assert_eq!(way_back(Some(mark), None), None);
        assert_eq!(way_back(None, Some((boat, true))), None);
        assert_eq!(live(Some((boat, true))), Some(boat));
        assert_eq!(live(Some((boat, false))), None);
        assert_eq!(live(None), None);
    }

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
        // The tanker rings; the ferry, already heard, stays silenced while
        // it remains a danger — a fresh CPA must not ring it all over again.
        let silenced = |a: &Alarms, id: &str| {
            a.iter()
                .find(|(k, _)| **k == AlarmKey::Collision(id.into()))
                .map(|(_, al)| al.acknowledged)
        };
        assert_eq!(silenced(&a, "ferry"), Some(true));
        assert_eq!(silenced(&a, "tanker"), Some(false));
        // The ferry passes: its alarm clears, the tanker's stays.
        a.set_collisions(vec![("tanker".into(), "0.2 nm".into())], now);
        let keys: Vec<_> = a.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(keys, vec![AlarmKey::Collision("tanker".into())]);
        // A ferry that turns back towards us is a new danger, and rings.
        a.acknowledge();
        a.set_collisions(
            vec![("tanker".into(), "0.2 nm".into()), ("ferry".into(), "0.1 nm".into())],
            now,
        );
        assert_eq!(silenced(&a, "ferry"), Some(false));
        assert!(a.ringing());
    }

    /// Clearing the collisions must leave the other watches alone.
    #[test]
    fn clearing_collisions_does_not_clear_the_anchor_watch() {
        let now = t0();
        let mut a = Alarms::default();
        a.set(AlarmKey::Anchor, Some("dragging".into()), now);
        a.set_collisions(vec![("x".into(), "m".into())], now);
        a.set_collisions(Vec::new(), now);
        let keys: Vec<_> = a.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(keys, vec![AlarmKey::Anchor]);
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
        // The circle is in metres and its edge is sharp: half a metre inside
        // is quiet, half a metre outside rings.
        let d = crate::geo::distance_m(anchor, far);
        assert!((d - 111.2).abs() < 0.5, "{d}");
        assert!(anchor_check(anchor, d + 0.5, Some((far, true))).is_none());
        assert!(anchor_check(anchor, d - 0.5, Some((far, true))).is_some());
        let msg = anchor_check(anchor, 40.0, Some((far, true))).unwrap();
        assert!(msg.contains("111 m") && msg.contains("40 m"), "{msg}");
    }

    #[test]
    fn depth_rings_below_the_limit_only_when_there_is_a_depth() {
        assert!(depth_check(Some(1.8), 2.5).is_some());
        assert!(depth_check(Some(2.4), 2.5).is_some());
        assert!(depth_check(Some(2.5), 2.5).is_none(), "at the limit is not below it");
        assert!(depth_check(Some(3.0), 2.5).is_none());
        assert!(depth_check(None, 2.5).is_none());
    }

    /// The banner names each alarm, so each needs a name of its own — and a
    /// man overboard has to say so in words anyone on deck understands.
    #[test]
    fn every_alarm_has_its_own_title() {
        let keys = [
            AlarmKey::ManOverboard,
            AlarmKey::Anchor,
            AlarmKey::Depth,
            AlarmKey::Collision("ferry".into()),
        ];
        let titles: Vec<&str> = keys.iter().map(AlarmKey::title).collect();
        for (i, t) in titles.iter().enumerate() {
            assert!(!t.trim().is_empty(), "{:?} has no title", keys[i]);
            assert!(!titles[..i].contains(t), "{t:?} used twice: {titles:?}");
        }
        assert!(AlarmKey::ManOverboard.title().contains("OVERBOARD"));
        assert!(AlarmKey::Anchor.title().contains("ANCHOR"));
        // One kind of alarm, one title, whichever ship it is about.
        assert_eq!(
            AlarmKey::Collision("ferry".into()).title(),
            AlarmKey::Collision("tanker".into()).title()
        );
    }

    /// The alarm is for a boat that has *left* the circle. A boat exactly on
    /// its edge has not, the same way a depth at the limit is not below it.
    #[test]
    fn a_boat_exactly_on_the_edge_of_the_circle_has_not_left_it() {
        let anchor = LatLon::new(55.0, 11.0);
        let at = LatLon::new(55.0005, 11.0003);
        let d = crate::geo::distance_m(anchor, at);
        assert!(anchor_check(anchor, d, Some((at, true))).is_none());
        // And the boat sitting on the anchor is inside any circle at all.
        assert!(anchor_check(anchor, 0.0, Some((anchor, true))).is_none());
    }

    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Inside the circle is quiet, outside rings, wherever the anchor is
        /// dropped and whichever way the boat swings. A fix that is not fresh
        /// rings regardless — the watch cannot vouch for a position it has
        /// stopped seeing.
        #[test]
        fn the_anchor_watch_rings_exactly_when_the_boat_is_outside(
            lat in -80.0..80.0_f64,
            lon in -180.0..180.0_f64,
            bearing in 0.0..std::f64::consts::TAU,
            radius in 5.0..500.0_f64,
            fraction in 0.0..2.0_f64,
        ) {
            let anchor = LatLon::new(lat, lon);
            let at = crate::geo::advance(anchor, bearing, radius * fraction);
            let alarm = anchor_check(anchor, radius, Some((at, true)));
            if fraction < 0.999 {
                prop_assert!(alarm.is_none(), "{alarm:?}");
            } else if fraction > 1.001 {
                prop_assert!(alarm.is_some());
            }
            prop_assert!(anchor_check(anchor, radius, Some((at, false))).is_some());
            prop_assert!(anchor_check(anchor, radius, None).is_some());
        }

        #[test]
        fn depth_rings_only_below_the_limit(depth in 0.0..50.0_f64, limit in 0.0..50.0_f64) {
            prop_assert_eq!(depth_check(Some(depth), limit).is_some(), depth < limit);
            prop_assert!(depth_check(None, limit).is_none());
        }

        /// Whatever has been raised and cleared, silencing stops the sound and
        /// leaves the alarms on screen; and only a key that was raised last,
        /// not cleared, is shown.
        #[test]
        fn silencing_quiets_everything_and_hides_nothing(
            ops in prop::collection::vec((0..4_usize, any::<bool>()), 0..20)
        ) {
            let now = Instant::now();
            let key = |i: usize| match i {
                0 => AlarmKey::ManOverboard,
                1 => AlarmKey::Anchor,
                2 => AlarmKey::Depth,
                _ => AlarmKey::Collision("x".into()),
            };
            let mut a = Alarms::default();
            let mut raised = [false; 4];
            for (i, on) in ops {
                a.set(key(i), on.then(|| "m".to_string()), now);
                raised[i] = on;
            }
            let shown: Vec<AlarmKey> = a.iter().map(|(k, _)| k.clone()).collect();
            let expected: Vec<AlarmKey> = (0..4).filter(|&i| raised[i]).map(key).collect();
            prop_assert_eq!(&shown, &expected);
            prop_assert_eq!(a.ringing(), !expected.is_empty());
            a.acknowledge();
            prop_assert!(!a.ringing());
            prop_assert!(!a.due(now + REPEAT));
            prop_assert_eq!(a.is_empty(), expected.is_empty());
        }
    }
}
