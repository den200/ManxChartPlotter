//! The track: where the boat has actually been.
//!
//! A point goes down whenever the boat has moved 10 m from the last one, and
//! every five minutes regardless, so a night at anchor still shows the swing
//! without filling the file. Only fresh fixes are recorded: a dead GPS
//! must not draw the boat sitting still. The track is written as GPX 1.1
//! `<trk>` — what every other plotter reads — to its own file in the tracks
//! folder, rewritten each minute so a power cut loses a minute at most.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::geo::LatLon;

/// Move this far and a new point is recorded.
const MIN_MOVE_M: f64 = 10.0;
/// And at least this often, moving or not.
const MAX_GAP: Duration = Duration::from_secs(300);
/// How often the file is rewritten while recording.
const SAVE_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy)]
pub struct TrackPoint {
    pub at: LatLon,
    pub time: DateTime<Utc>,
}

#[derive(Debug)]
pub struct Track {
    pub points: Vec<TrackPoint>,
    pub started: DateTime<Utc>,
    /// Where it is saved.
    pub file: PathBuf,
    last_point: Option<Instant>,
    last_save: Option<Instant>,
    dirty: bool,
}

impl Track {
    /// A new, empty track saved under `dir`, named for when it started.
    pub fn new(dir: &Path, now: DateTime<Utc>) -> Self {
        let file = dir.join(format!("track-{}.gpx", now.format("%Y-%m-%d-%H%M%S")));
        Self {
            points: Vec::new(),
            started: now,
            file,
            last_point: None,
            last_save: None,
            dirty: false,
        }
    }

    /// Offer a fresh fix. Returns whether it was recorded.
    pub fn offer(&mut self, at: LatLon, time: DateTime<Utc>, now: Instant) -> bool {
        let take = match (self.points.last(), self.last_point) {
            (Some(last), Some(when)) => {
                crate::geo::distance_m(last.at, at) >= MIN_MOVE_M
                    || now.duration_since(when) >= MAX_GAP
            }
            _ => true,
        };
        if take {
            self.points.push(TrackPoint { at, time });
            self.last_point = Some(now);
            self.dirty = true;
        }
        take
    }

    /// Length along the track, nautical miles.
    pub fn distance_nm(&self) -> f64 {
        self.points
            .windows(2)
            .map(|w| crate::geo::distance_m(w[0].at, w[1].at))
            // Not `sum`: an empty float sum is -0.0, which prints as "-0.0".
            .fold(0.0, |a, b| a + b)
            / crate::geo::METRES_PER_NM
    }

    /// Write the file if it has changed and a minute has passed (or `force`).
    pub fn save_if_due(&mut self, now: Instant, force: bool) {
        let due = force || self.last_save.is_none_or(|t| now.duration_since(t) >= SAVE_EVERY);
        if !self.dirty || !due || self.points.is_empty() {
            return;
        }
        if let Some(parent) = self.file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Aside and renamed into place, like the settings: power on a boat
        // goes when it goes.
        let tmp = self.file.with_extension("gpx.tmp");
        let written = std::fs::write(&tmp, self.to_gpx()).and_then(|_| std::fs::rename(&tmp, &self.file));
        match written {
            Ok(()) => {
                self.dirty = false;
                self.last_save = Some(now);
            }
            Err(e) => log::warn!("track: could not save {}: {e}", self.file.display()),
        }
    }

    /// The track as GPX 1.1.
    pub fn to_gpx(&self) -> String {
        let mut s = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <gpx version=\"1.1\" creator=\"navcore\" xmlns=\"http://www.topografix.com/GPX/1/1\">\n",
        );
        s.push_str(&format!(
            "  <trk>\n    <name>Track {}</name>\n    <trkseg>\n",
            self.started.format("%Y-%m-%d %H:%M UTC")
        ));
        for p in &self.points {
            // Shortest round-trip formatting, as the routes are written: the
            // position survives to the bit.
            s.push_str(&format!(
                "      <trkpt lat=\"{}\" lon=\"{}\"><time>{}</time></trkpt>\n",
                p.at.lat,
                p.at.lon,
                p.time.format("%Y-%m-%dT%H:%M:%SZ")
            ));
        }
        s.push_str("    </trkseg>\n  </trk>\n</gpx>\n");
        s
    }
}

/// Where tracks are kept: beside the routes.
pub fn default_dir() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("navcore").join("tracks"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_go_down_on_movement_or_after_a_while() {
        let dir = std::env::temp_dir();
        let t0 = Instant::now();
        let now = Utc::now();
        let mut t = Track::new(&dir, now);
        let here = LatLon::new(55.0, 11.0);
        assert!(t.offer(here, now, t0), "the first fix always");
        assert!(!t.offer(LatLon::new(55.00005, 11.0), now, t0 + Duration::from_secs(5)), "5 m");
        assert!(t.offer(LatLon::new(55.0002, 11.0), now, t0 + Duration::from_secs(10)), "22 m");
        assert!(t.offer(LatLon::new(55.0002, 11.0), now, t0 + Duration::from_secs(400)), "5 min");
        assert_eq!(t.points.len(), 3);
        assert!((t.distance_nm() * 1852.0 - 22.3).abs() < 1.0, "{}", t.distance_nm() * 1852.0);
    }

    #[test]
    fn the_track_reads_back_as_gpx() {
        let mut t = Track::new(&std::env::temp_dir(), Utc::now());
        t.offer(LatLon::new(55.123456789, 11.987654321), Utc::now(), Instant::now());
        let gpx = t.to_gpx();
        assert!(gpx.contains("<trkpt lat=\"55.123456789\" lon=\"11.987654321\">"));
        // navcore's own reader skips tracks, but must not choke on one.
        assert!(crate::nav::gpx::parse(&gpx).is_ok());
    }
}
