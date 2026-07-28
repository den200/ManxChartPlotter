//! A boat that isn't there.
//!
//! Testing a plotter needs a moving vessel, and the public Signal K demo sails
//! in the Gulf of Finland — nowhere near most people's charts. This generates
//! the same deltas a real server would, for a boat sailing wherever you point
//! it, so the marker, the follow-the-boat camera and the instrument bar can all
//! be exercised at the chart table.
//!
//! It emits through the ordinary [`super::delta::Delta`] path rather than
//! writing to the vessel state directly: a simulator that bypasses the parser
//! tests nothing but itself.

use super::delta::{Delta, Update, Value};

/// Where the simulated boat sails, and how.
#[derive(Debug, Clone, Copy)]
pub struct Course {
    pub lat: f64,
    pub lon: f64,
    /// Heading in degrees true at the start.
    pub heading_deg: f64,
    /// Speed through the water, knots.
    pub speed_kn: f64,
}

impl Default for Course {
    fn default() -> Self {
        // The Kattegat, between Sjælland and Jylland — open water on Danish
        // charts, deep enough to be plausible, and busy enough to be a fair
        // test of a chart that has something on it.
        Self {
            lat: 56.55,
            lon: 11.60,
            heading_deg: 200.0,
            speed_kn: 6.5,
        }
    }
}

/// A boat under way.
pub struct Simulator {
    course: Course,
    lat: f64,
    lon: f64,
    heading: f64,
    /// Seconds since the simulation began, advanced by the caller.
    elapsed: f64,
}

impl Simulator {
    pub fn new(course: Course) -> Self {
        Self {
            course,
            lat: course.lat,
            lon: course.lon,
            heading: course.heading_deg.to_radians(),
            elapsed: 0.0,
        }
    }

    /// Advance by `dt` seconds and produce the delta a server would have sent.
    pub fn step(&mut self, dt: f64) -> Delta {
        self.elapsed += dt;

        // A slow weave, so heading and course over ground differ and the
        // display has something honest to show. A boat that tracks a perfect
        // straight line hides every bug in the bearing maths.
        let swing = (self.elapsed / 90.0).sin() * 12f64.to_radians();
        self.heading = self.course.heading_deg.to_radians() + swing;

        let speed_ms = self.course.speed_kn * 1852.0 / 3600.0;
        let distance = speed_ms * dt;

        // Set and drift: a westerly current, which is what makes course over
        // ground differ from heading in the way a navigator expects.
        let set = 250f64.to_radians();
        let drift = 0.4 * 1852.0 / 3600.0;

        let north = distance * self.heading.cos() + drift * dt * set.cos();
        let east = distance * self.heading.sin() + drift * dt * set.sin();

        // Metres to degrees. Good enough over a few metres a second, and the
        // longitude scale shrinks with latitude.
        const M_PER_DEG_LAT: f64 = 111_320.0;
        self.lat += north / M_PER_DEG_LAT;
        self.lon += east / (M_PER_DEG_LAT * self.lat.to_radians().cos());

        // Course over ground is the direction actually travelled, which is
        // heading plus whatever the current did.
        let cog = east.atan2(north).rem_euclid(std::f64::consts::TAU);
        let sog = (north * north + east * east).sqrt() / dt;

        // A seabed that varies, so the depth reading moves and a shallow
        // alarm has something to fire on later.
        let depth = 18.0 + (self.elapsed / 40.0).sin() * 9.0 + (self.elapsed / 7.0).cos() * 0.6;

        // Apparent wind from a true wind on the beam.
        let awa = (-55f64).to_radians() + (self.elapsed / 60.0).cos() * 8f64.to_radians();
        let aws = 9.0 + (self.elapsed / 25.0).sin() * 2.0;

        let values = vec![
            (
                "navigation.position",
                Value::Position {
                    lat: self.lat,
                    lon: self.lon,
                },
            ),
            ("navigation.headingTrue", Value::Number(self.heading)),
            ("navigation.courseOverGroundTrue", Value::Number(cog)),
            ("navigation.speedOverGround", Value::Number(sog)),
            ("navigation.speedThroughWater", Value::Number(speed_ms)),
            ("environment.depth.belowTransducer", Value::Number(depth)),
            (
                "environment.depth.belowKeel",
                Value::Number((depth - 1.8).max(0.0)),
            ),
            ("environment.wind.angleApparent", Value::Number(awa)),
            ("environment.wind.speedApparent", Value::Number(aws)),
            ("environment.water.temperature", Value::Number(287.0)),
            ("navigation.rateOfTurn", Value::Number(swing / 90.0)),
            ("electrical.batteries.house.voltage", Value::Number(12.8)),
        ];

        Delta {
            // No context, which is how a server says "this is my own vessel".
            context: None,
            updates: values
                .into_iter()
                .map(|(path, value)| Update {
                    path: path.to_string(),
                    value,
                })
                .collect(),
        }
    }

    pub fn position(&self) -> (f64, f64) {
        (self.lat, self.lon)
    }

    pub fn heading(&self) -> f64 {
        self.heading
    }
}

/// One simulated AIS target.
pub struct Traffic {
    pub context: String,
    pub name: &'static str,
    /// What the vessel says it is doing, in Signal K's vocabulary.
    pub state: &'static str,
    /// AIS ship type code — 30 fishing, 60 passenger, 70 cargo, 80 tanker.
    pub ship_type: u32,
    sim: Simulator,
    /// A moored or anchored vessel does not move, whatever its course says.
    moving: bool,
}

impl Traffic {
    pub fn step(&mut self, dt: f64) -> Delta {
        let mut delta = if self.moving {
            self.sim.step(dt)
        } else {
            // Still, but still reporting: an anchored ship broadcasts its
            // position every few minutes and must appear on the chart.
            let (lat, lon) = self.sim.position();
            Delta {
                context: None,
                updates: vec![
                    Update {
                        path: "navigation.position".into(),
                        value: Value::Position { lat, lon },
                    },
                    Update {
                        path: "navigation.speedOverGround".into(),
                        value: Value::Number(0.0),
                    },
                    Update {
                        path: "navigation.headingTrue".into(),
                        value: Value::Number(self.sim.heading()),
                    },
                ],
            }
        };

        // A target's identity travels with it — the chart labels a ship, not a
        // number, whenever the ship has told us its name.
        delta.updates.push(Update {
            path: "name".into(),
            value: Value::Text(self.name.to_string()),
        });
        delta.updates.push(Update {
            path: "navigation.state".into(),
            value: Value::Text(self.state.to_string()),
        });
        delta.updates.push(Update {
            path: "design.aisShipType".into(),
            value: Value::Number(self.ship_type as f64),
        });
        delta.context = Some(self.context.clone());
        delta
    }
}

/// A plausible sea's worth of traffic around a point.
///
/// Chosen to exercise the display rather than to be realistic: a ship crossing
/// ahead so there is a real closest approach, one overtaking slowly from
/// astern, a fishing boat wandering, and one at anchor that must draw no
/// course vector however stale its heading.
pub fn traffic_around(centre: Course) -> Vec<Traffic> {
    let spec: &[(&str, &'static str, &'static str, u32, f64, f64, f64, f64, bool)] = &[
        // name, state, mmsi, type, dlat, dlon, heading, knots, moving
        ("NORDLYS", "under way using engine", "219001234", 60, 0.045, -0.070, 95.0, 12.0, true),
        ("KATTEGAT TRADER", "under way using engine", "244060807", 70, -0.055, 0.045, 340.0, 9.5, true),
        ("HAVFISK", "under way using engine", "219778001", 30, 0.020, 0.065, 210.0, 4.0, true),
        ("ANNA MAERSK", "at anchor", "219900555", 80, -0.030, -0.055, 15.0, 0.0, false),
    ];
    spec.iter()
        .map(
            |(name, state, mmsi, ship_type, dlat, dlon, heading, knots, moving)| Traffic {
                context: format!("vessels.urn:mrn:imo:mmsi:{mmsi}"),
                name,
                state,
                ship_type: *ship_type,
                sim: Simulator::new(Course {
                    lat: centre.lat + dlat,
                    lon: centre.lon + dlon,
                    heading_deg: *heading,
                    speed_kn: *knots,
                }),
                moving: *moving,
            },
        )
        .collect()
}

/// Read a course from a URL-ish string: `sim`, or `sim:lat,lon`, or
/// `sim:lat,lon,heading,speed`.
///
/// `None` when this is not a simulator address at all.
pub fn parse_course(input: &str) -> Option<Course> {
    let rest = input
        .trim()
        .strip_prefix("sim")?
        .trim_start_matches(':')
        .trim();
    if rest.is_empty() {
        return Some(Course::default());
    }
    let parts: Vec<f64> = rest.split(',').filter_map(|p| p.trim().parse().ok()).collect();
    let base = Course::default();
    Some(match parts.len() {
        2 => Course {
            lat: parts[0],
            lon: parts[1],
            ..base
        },
        3 => Course {
            lat: parts[0],
            lon: parts[1],
            heading_deg: parts[2],
            ..base
        },
        4 => Course {
            lat: parts[0],
            lon: parts[1],
            heading_deg: parts[2],
            speed_kn: parts[3],
        },
        _ => base,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boat_moves_and_stays_near_where_it_started() {
        let mut sim = Simulator::new(Course::default());
        let start = sim.position();
        for _ in 0..60 {
            sim.step(1.0);
        }
        let (lat, lon) = sim.position();
        assert_ne!((lat, lon), start, "it should have moved");
        // A minute at six knots is about 200 m, so a tenth of a degree of
        // travel would mean the maths is wrong by an order of magnitude.
        assert!((lat - start.0).abs() < 0.01, "lat ran away: {lat}");
        assert!((lon - start.1).abs() < 0.01, "lon ran away: {lon}");
        // Heading roughly south-west, so latitude must fall.
        assert!(lat < start.0, "heading 200° should take it south");
    }

    #[test]
    fn a_delta_carries_what_a_plotter_needs() {
        let mut sim = Simulator::new(Course::default());
        let delta = sim.step(1.0);
        assert!(delta.is_self(None), "the simulator speaks as our own vessel");
        let paths: Vec<&str> = delta.updates.iter().map(|u| u.path.as_str()).collect();
        for needed in [
            "navigation.position",
            "navigation.headingTrue",
            "navigation.courseOverGroundTrue",
            "navigation.speedOverGround",
            "environment.depth.belowTransducer",
        ] {
            assert!(paths.contains(&needed), "missing {needed}");
        }
    }

    #[test]
    fn speed_over_ground_is_close_to_the_speed_sailed() {
        let mut sim = Simulator::new(Course::default());
        let delta = sim.step(1.0);
        let sog = delta
            .updates
            .iter()
            .find(|u| u.path == "navigation.speedOverGround")
            .and_then(|u| u.value.as_number())
            .expect("SOG");
        // 6.5 knots through the water plus 0.4 of current: between 6 and 7.5.
        let knots = sog * 3600.0 / 1852.0;
        assert!((6.0..7.5).contains(&knots), "{knots} knots");
    }

    #[test]
    fn course_over_ground_differs_from_heading_because_of_the_current() {
        let mut sim = Simulator::new(Course::default());
        let delta = sim.step(1.0);
        let get = |p: &str| {
            delta
                .updates
                .iter()
                .find(|u| u.path == p)
                .and_then(|u| u.value.as_number())
                .unwrap()
        };
        let hdg = get("navigation.headingTrue");
        let cog = get("navigation.courseOverGroundTrue");
        assert!(
            (hdg - cog).abs() > 1e-4,
            "a boat in a current should not make good its heading"
        );
    }

    #[test]
    fn addresses_parse_into_courses() {
        assert!(parse_course("sim").is_some());
        assert!(parse_course("demo.signalk.org").is_none());
        assert!(parse_course("192.168.1.50").is_none());

        let c = parse_course("sim:56.1,11.2").expect("a course");
        assert_eq!((c.lat, c.lon), (56.1, 11.2));
        assert_eq!(c.heading_deg, Course::default().heading_deg);

        let c = parse_course("sim:56.1,11.2,90,4.5").expect("a course");
        assert_eq!((c.heading_deg, c.speed_kn), (90.0, 4.5));

        // The default is the Kattegat, which is where the Danish charts are.
        let d = Course::default();
        assert!((56.0..57.0).contains(&d.lat) && (10.5..12.5).contains(&d.lon));
    }
}
