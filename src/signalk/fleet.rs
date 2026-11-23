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

use chrono::{DateTime, Utc};

use super::delta::Delta;
use super::state::Vessel;

/// How old a value was when it arrived, from the server's timestamps.
///
/// The server's clock and ours need not agree — a Raspberry Pi with no
/// battery clock boots in the past — so a timestamp cannot simply be
/// compared with the time here. What can be: the *smallest* gap ever seen
/// between a timestamp and our clock is the two clocks' disagreement (plus
/// the network's delay), and a value whose gap is larger than that by an
/// hour is an hour old.
///
/// When our own clock is set (NTP arriving after boot) every gap changes at
/// once and the estimate starts again; the jump is seen by comparing the
/// wall clock with the monotonic one.
#[derive(Debug, Default)]
pub struct ServerClock {
    /// Smallest `ours − theirs` seen, milliseconds.
    least_gap_ms: Option<i64>,
    /// A moment on both of our clocks, to notice the wall clock being set.
    anchor: Option<(Instant, DateTime<Utc>)>,
}

impl ServerClock {
    /// The age on arrival of a value stamped `sent`, arriving at `now` —
    /// which is `now_utc` on the wall clock. And, second, by how much
    /// everything stamped earlier now proves older than it was taken for:
    /// the first values of a connection are the server's cache, and an
    /// hour-old one looks new until something live arrives to compare.
    pub fn late(&mut self, sent: DateTime<Utc>, now_utc: DateTime<Utc>, now: Instant) -> (Duration, Duration) {
        match self.anchor {
            Some((then, then_utc)) => {
                let wall = (now_utc - then_utc).num_milliseconds();
                let steady = now.duration_since(then).as_millis() as i64;
                if (wall - steady).abs() > 2_000 {
                    self.least_gap_ms = None;
                    self.anchor = Some((now, now_utc));
                }
            }
            None => self.anchor = Some((now, now_utc)),
        }
        let gap = (now_utc - sent).num_milliseconds();
        let Some(least) = self.least_gap_ms else {
            self.least_gap_ms = Some(gap);
            return (Duration::ZERO, Duration::ZERO);
        };
        // Under a second is the network's jitter, not news about the clock.
        if gap < least - 1_000 {
            self.least_gap_ms = Some(gap);
            return (Duration::ZERO, Duration::from_millis((least - gap) as u64));
        }
        (Duration::from_millis((gap - least).max(0) as u64), Duration::ZERO)
    }
}

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
        // Failing that, only a context that says it is an MMSI —
        // `urn:mrn:imo:mmsi:244060807`. A vessel known by a Signal K uuid
        // has no MMSI, and its uuid must not be shown as one.
        self.vessel.text("mmsi").or_else(|| {
            self.context
                .split_once(":mmsi:")
                .map(|(_, m)| m)
                .filter(|m| !m.is_empty() && m.bytes().all(|b| b.is_ascii_digit()))
        })
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

    /// How long since the target last said where it is — or, for one that
    /// never has, since it said anything. A ship that still broadcasts its
    /// name but no position is not a ship whose position is known.
    pub fn silent_for(&self) -> Duration {
        match self.vessel.get("navigation.position") {
            Some(reading) => reading.age(),
            None => self.last_report.elapsed(),
        }
    }

    /// Nothing heard for long enough that the target should not be trusted.
    pub fn is_lost(&self) -> bool {
        self.silent_for() > LOST_AFTER
    }

    /// Where the target is now and how it is moving: its last reported
    /// position carried forward along its course for as long as that
    /// report is old. A 15-knot ship is a mile from a four-minute-old
    /// position, and the closest approach must be reckoned from where she
    /// is, not from where she was.
    ///
    /// A target that gives no course or no speed is taken as stopped — an
    /// anchored ship is still something to hit — and one that has not moved
    /// is not carried anywhere.
    pub fn motion_now(&self) -> Option<crate::geo::Motion> {
        let (lat, lon) = self.position()?;
        let at = crate::geo::LatLon::new(lat, lon);
        let (course, speed) = match (self.course(), self.speed()) {
            (Some(course), Some(speed)) if speed.is_finite() && course.is_finite() => (course, speed.max(0.0)),
            _ => (0.0, 0.0),
        };
        let age = self.silent_for().min(LOST_AFTER).as_secs_f64();
        let at = if speed > 0.0 { crate::geo::advance(at, course, speed * age) } else { at };
        Some(crate::geo::Motion { at, course, speed })
    }

    /// Under way, as opposed to moored, anchored or aground.
    ///
    /// Worth knowing before drawing a course vector: a moored ship with a
    /// stale course would otherwise sprout a vector across the harbour.
    pub fn under_way(&self) -> bool {
        match self.vessel.text("navigation.state") {
            // Signal K's own words, and the AIS wording that some
            // converters pass straight through.
            Some(s) => !matches!(
                s,
                "moored"
                    | "anchored"
                    | "at anchor"
                    | "not-under-way"
                    | "not under command"
                    | "aground"
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
    clock: ServerClock,
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
        self.apply_at(delta, Instant::now(), Utc::now());
    }

    /// As [`apply`](Self::apply), at a given moment on both clocks.
    pub fn apply_at(&mut self, delta: &Delta, now: Instant, now_utc: DateTime<Utc>) {
        // The server's clock first: what this delta's timestamps say about
        // everything already held is settled before its values go in.
        let mut older = Duration::ZERO;
        let lates: Vec<Duration> = delta
            .updates
            .iter()
            .map(|u| match u.sent {
                Some(sent) => {
                    let (late, by) = self.clock.late(sent, now_utc, now);
                    older += by;
                    late
                }
                None => Duration::ZERO,
            })
            .collect();
        if older > Duration::ZERO {
            self.own.age_stamped(older);
            for target in self.targets.values_mut() {
                target.vessel.age_stamped(older);
            }
        }
        let mut lates = lates.into_iter();
        let late = |_: &super::delta::Update| lates.next().unwrap_or_default();
        if delta.is_self(self.self_context.as_deref()) {
            self.own.apply_aged(delta, now, late);
            return;
        }
        let Some(context) = delta.context.clone() else {
            return;
        };
        let target = self.targets.entry(context.clone()).or_insert_with(|| Target {
            context,
            vessel: Vessel::default(),
            last_report: now,
        });
        target.vessel.apply_aged(delta, now, late);
        target.last_report = now;
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
        self.targets.retain(|_, t| t.silent_for() <= FORGET_AFTER);
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
        self.clock = ServerClock::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signalk::delta;

    /// A delta whose values the server stamped `at` (RFC 3339).
    fn stamped(context: Option<&str>, at: &str, body: &str) -> Delta {
        let ctx = context.map(|c| format!(r#""context":"{c}","#)).unwrap_or_default();
        delta::parse(&format!(r#"{{{ctx}"updates":[{{"timestamp":"{at}","values":[{body}]}}]}}"#))
            .expect("a delta")
    }

    const SHIP: &str = "vessels.urn:mrn:imo:mmsi:219000001";
    const POSITION: &str = r#"{"path":"navigation.position","value":{"latitude":56.0,"longitude":11.0}}"#;

    /// Our own clock, deliberately years off the server's: only the
    /// timestamps' differences may matter.
    fn here() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2031-01-01T00:00:00Z").unwrap().with_timezone(&Utc)
    }

    /// On connecting, the server sends what it remembers. A depth measured
    /// an hour before the newest value in the stream is an hour old, however
    /// recently it arrived — and it arrived first.
    #[test]
    fn a_value_replayed_from_the_servers_cache_is_as_old_as_its_timestamp() {
        let mut fleet = Fleet::default();
        let now = Instant::now();
        fleet.apply_at(
            &stamped(None, "2026-07-27T10:00:00Z", r#"{"path":"environment.depth.belowKeel","value":4.2}"#),
            now,
            here(),
        );
        fleet.apply_at(
            &stamped(None, "2026-07-27T11:00:00Z", r#"{"path":"navigation.speedOverGround","value":3.0}"#),
            now,
            here(),
        );
        let depth = fleet.own.get("environment.depth.belowKeel").unwrap();
        assert!(depth.age() >= Duration::from_secs(3600) && depth.is_stale(), "{:?}", depth.age());
        assert!(!fleet.own.get("navigation.speedOverGround").unwrap().is_stale());

        // And one arriving after the live value, stamped ten minutes back.
        fleet.apply_at(
            &stamped(None, "2026-07-27T10:50:00Z", r#"{"path":"navigation.headingTrue","value":1.0}"#),
            now,
            here(),
        );
        let age = fleet.own.get("navigation.headingTrue").unwrap().age();
        assert!(age >= Duration::from_secs(600) && age < Duration::from_secs(601), "{age:?}");
    }

    /// A value with no timestamp is aged from its arrival, as before.
    #[test]
    fn a_value_without_a_timestamp_is_as_old_as_its_arrival() {
        let mut fleet = Fleet::default();
        fleet.apply_at(&stamped(None, "2026-07-27T11:00:00Z", POSITION), Instant::now(), here());
        fleet.apply_at(&delta_for(None, r#"{"path":"navigation.headingTrue","value":1.0}"#), Instant::now(), here());
        assert!(!fleet.own.get("navigation.headingTrue").unwrap().is_stale());
    }

    /// A ship the server remembers from an hour ago is not on the water
    /// now: lost at once, and forgotten at the next sweep.
    #[test]
    fn a_ship_replayed_from_an_hour_ago_is_not_a_live_target() {
        let mut fleet = Fleet::default();
        let now = Instant::now();
        fleet.apply_at(&stamped(Some(SHIP), "2026-07-27T10:00:00Z", POSITION), now, here());
        fleet.apply_at(&stamped(None, "2026-07-27T11:00:00Z", POSITION), now, here());
        assert!(fleet.targets().next().unwrap().is_lost());
        fleet.forget_stale();
        assert!(fleet.is_empty());
    }

    /// A ship that still sends its name but last sent a position seven
    /// minutes ago is lost (the limit is six): where she is, is not known.
    #[test]
    fn a_name_does_not_keep_a_position_alive() {
        let mut fleet = Fleet::default();
        let now = Instant::now();
        fleet.apply_at(&stamped(Some(SHIP), "2026-07-27T10:53:00Z", POSITION), now, here());
        fleet.apply_at(&stamped(Some(SHIP), "2026-07-27T11:00:00Z", r#"{"path":"name","value":"FREJA"}"#), now, here());
        let ship = fleet.targets().next().unwrap();
        assert_eq!(ship.name(), Some("FREJA"));
        assert!(ship.is_lost(), "silent for {:?}", ship.silent_for());
    }

    /// Fifteen knots for four minutes is one nautical mile. A target whose
    /// position is four minutes old, steering 090°, is 1852 m east of it.
    #[test]
    fn a_target_is_carried_forward_from_an_old_position() {
        let mut fleet = Fleet::default();
        let now = Instant::now();
        let fifteen_kn = 15.0 * 1852.0 / 3600.0;
        let body = format!(
            r#"{POSITION},{{"path":"navigation.courseOverGroundTrue","value":{}}},{{"path":"navigation.speedOverGround","value":{fifteen_kn}}}"#,
            std::f64::consts::FRAC_PI_2
        );
        fleet.apply_at(&stamped(Some(SHIP), "2026-07-27T10:56:00Z", &body), now, here());
        fleet.apply_at(&stamped(None, "2026-07-27T11:00:00Z", POSITION), now, here());
        let m = fleet.targets().next().unwrap().motion_now().unwrap();
        let (run, bearing) = crate::geo::range_bearing(crate::geo::LatLon::new(56.0, 11.0), m.at);
        assert!((run - 1852.0).abs() < 10.0, "{run} m");
        assert!((bearing.to_degrees() - 90.0).abs() < 0.1, "{}", bearing.to_degrees());
        assert!((m.speed - fifteen_kn).abs() < 1e-9);
    }

    /// An anchored ship that reports no course is still there to be hit:
    /// she has a motion, and it is "stopped, where she said".
    #[test]
    fn a_target_without_a_course_is_stopped_not_absent() {
        let mut fleet = Fleet::default();
        fleet.apply_at(&stamped(Some(SHIP), "2026-07-27T11:00:00Z", POSITION), Instant::now(), here());
        let m = fleet.targets().next().unwrap().motion_now().expect("a motion");
        assert_eq!(m.speed, 0.0);
        assert_eq!((m.at.lat, m.at.lon), (56.0, 11.0));
        // Own boat making 5 kn due east from a mile west of her closes to
        // nothing in twelve minutes.
        let own = crate::geo::Motion {
            at: crate::geo::advance(m.at, 270f64.to_radians(), 1852.0),
            course: std::f64::consts::FRAC_PI_2,
            speed: 5.0 * 1852.0 / 3600.0,
        };
        let c = crate::geo::cpa(own, m).expect("an approach");
        assert!(c.distance_m < 5.0 && (c.seconds - 720.0).abs() < 2.0, "{c:?}");
    }

    /// When our own clock is set — NTP arriving after a boot in the past —
    /// the comparison starts again: what arrives next is not a year old.
    #[test]
    fn setting_our_own_clock_does_not_age_everything() {
        let mut clock = ServerClock::default();
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        let t0 = Instant::now();
        let sent = at("2026-07-27T11:00:00Z");
        assert_eq!(clock.late(sent, at("2025-01-01T00:00:00Z"), t0).0, Duration::ZERO);
        // A second later by the steady clock, the wall clock reads 2026.
        let (late, _) = clock.late(
            at("2026-07-27T11:00:01Z"),
            at("2026-07-27T11:00:01Z"),
            t0 + Duration::from_secs(1),
        );
        assert_eq!(late, Duration::ZERO);
        // And ages are counted from there on.
        let (late, _) = clock.late(
            at("2026-07-27T10:59:31Z"),
            at("2026-07-27T11:00:02Z"),
            t0 + Duration::from_secs(2),
        );
        assert_eq!(late, Duration::from_secs(31));
    }

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

    /// A vessel identified by a Signal K uuid has no MMSI to show, and its
    /// uuid — long enough to pass for one — must not be shown as one.
    #[test]
    fn a_uuid_context_is_not_an_mmsi() {
        let mut fleet = Fleet::default();
        let ctx = "vessels.urn:mrn:signalk:uuid:c0d79334-4e25-4245-8892-54e8ccc8021d";
        fleet.apply(&delta_for(
            Some(ctx),
            r#"{"path":"navigation.position","value":{"latitude":55.7,"longitude":12.6}}"#,
        ));
        let target = fleet.targets().next().expect("the target");
        assert_eq!(target.mmsi(), None);
        assert_eq!(target.label(), None);
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

    /// The other two ages a target passes through, which the fresh case above
    /// cannot reach: silent past [`LOST_AFTER`] it is lost but still drawn
    /// (crossed through); silent past [`FORGET_AFTER`] it is gone.
    #[test]
    fn a_silent_target_is_lost_and_then_forgotten() {
        let mut fleet = Fleet::default();
        for mmsi in ["4000001", "4000002", "4000003"] {
            fleet.apply(&delta_for(
                Some(&format!("vessels.urn:mrn:imo:mmsi:{mmsi}")),
                r#"{"path":"navigation.speedOverGround","value":1.0}"#,
            ));
        }
        let silent_for = |secs| {
            std::time::Instant::now()
                .checked_sub(Duration::from_secs(secs))
                .expect("the clock reaches back far enough")
        };
        let mut age = |mmsi: &str, secs| {
            fleet
                .targets
                .get_mut(&format!("vessels.urn:mrn:imo:mmsi:{mmsi}"))
                .unwrap()
                .last_report = silent_for(secs);
        };
        age("4000001", 60); // quiet a minute: still a live target
        age("4000002", LOST_AFTER.as_secs() + 30); // lost, not yet forgotten
        age("4000003", FORGET_AFTER.as_secs() + 30); // long gone

        let lost: Vec<_> = fleet
            .targets()
            .map(|t| (t.mmsi().unwrap().to_string(), t.is_lost()))
            .collect();
        assert_eq!(
            lost,
            vec![
                ("4000001".to_string(), false),
                ("4000002".to_string(), true),
                ("4000003".to_string(), true),
            ]
        );

        fleet.forget_stale();
        let kept: Vec<_> = fleet.targets().map(|t| t.mmsi().unwrap().to_string()).collect();
        assert_eq!(kept, vec!["4000001", "4000002"]);

        // A report from a lost target brings it back to life.
        fleet.apply(&delta_for(
            Some("vessels.urn:mrn:imo:mmsi:4000002"),
            r#"{"path":"navigation.speedOverGround","value":2.0}"#,
        ));
        assert!(fleet.targets().all(|t| !t.is_lost()));
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
