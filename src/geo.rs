//! Distances and bearings on the actual shape of the Earth.
//!
//! Deliberately separate from [`crate::render::projection`], which is a
//! *drawing* projection and must stay exactly as it is: OSENC stores its
//! geometry in OpenCPN's Simple Mercator, and navcore reproduces that to the
//! nanometre so chart features land on their own recorded coordinates.
//! Measuring distance in a Mercator plane, though, is wrong in a way that
//! grows with latitude — northings are stretched by `1/cos φ`, which is 1.8×
//! in the Kattegat and worse further north.
//!
//! So: draw in Mercator, measure on the ellipsoid. This wraps Karney's
//! geodesic algorithms, which are exact to within nanometres — the modern
//! replacement for the haversine formula (spherical, ~0.5 % out) and for
//! Vincenty (ellipsoidal but fails to converge on near-antipodal pairs).
//!
//! Precision here is not academic. A closest-point-of-approach that is 0.5 %
//! wrong on a two-mile CPA is twenty metres, and CPA is the number a watch
//! officer uses to decide whether to alter course.

use geographiclib_rs::{DirectGeodesic, Geodesic, InverseGeodesic};

/// A position, in the units Signal K and GPS both use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatLon {
    pub lat: f64,
    pub lon: f64,
}

impl LatLon {
    pub fn new(lat: f64, lon: f64) -> Self {
        Self { lat, lon }
    }
}

fn wgs84() -> &'static Geodesic {
    // Building the geodesic computes a series expansion; once is enough.
    static G: std::sync::OnceLock<Geodesic> = std::sync::OnceLock::new();
    G.get_or_init(Geodesic::wgs84)
}

/// Metres between two positions, along the geodesic.
pub fn distance_m(a: LatLon, b: LatLon) -> f64 {
    wgs84().inverse(a.lat, a.lon, b.lat, b.lon)
}

/// Distance in metres and initial bearing in radians from `a` to `b`.
pub fn range_bearing(a: LatLon, b: LatLon) -> (f64, f64) {
    let (s12, azi1, _azi2, _) = wgs84().inverse(a.lat, a.lon, b.lat, b.lon);
    (s12, azi1.to_radians().rem_euclid(std::f64::consts::TAU))
}

/// Where you end up steering `bearing` (radians) for `distance` metres.
pub fn advance(from: LatLon, bearing: f64, distance_m: f64) -> LatLon {
    let (lat, lon) = wgs84().direct(from.lat, from.lon, bearing.to_degrees(), distance_m);
    LatLon::new(lat, lon)
}

/// One nautical mile, by definition.
pub const METRES_PER_NM: f64 = 1852.0;

/// Read a position a user typed: "lat, lon" in decimal degrees, the form
/// every phone map hands out. Anything else — one number, three, letters,
/// out-of-range values — is `None`, never a guess.
pub fn parse_latlon(s: &str) -> Option<LatLon> {
    let parts: Vec<f64> = s
        .split(',')
        .map(str::trim)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let [lat, lon] = parts[..] else { return None };
    (lat.abs() <= 90.0 && lon.abs() <= 180.0).then(|| LatLon::new(lat, lon))
}

pub fn to_nm(metres: f64) -> f64 {
    metres / METRES_PER_NM
}

/// A vessel reduced to what a collision calculation needs.
#[derive(Debug, Clone, Copy)]
pub struct Motion {
    pub at: LatLon,
    /// Course over ground, radians.
    pub course: f64,
    /// Speed over ground, metres per second.
    pub speed: f64,
}

/// Closest point of approach: how near, and how long until then.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cpa {
    /// Metres at closest approach.
    pub distance_m: f64,
    /// Seconds until it. Zero when the closest approach is now or past.
    pub seconds: f64,
    /// The approach is behind us — the two are opening, not closing.
    pub past: bool,
}

/// Closest point of approach between two vessels holding course and speed.
///
/// Worked in a local east/north frame centred on `own`, which is flat enough
/// over the few miles CPA is meaningful for — but the frame is built from a
/// *geodesic* range and bearing, so the position that goes into it is right to
/// the nanometre rather than to whatever Mercator does at this latitude.
///
/// Returns `None` when neither is moving relative to the other, because then
/// there is no approach to compute: the range simply stays what it is.
pub fn cpa(own: Motion, other: Motion) -> Option<Cpa> {
    let (range, bearing) = range_bearing(own.at, other.at);

    // Relative position of the other vessel, east/north metres.
    let rx = range * bearing.sin();
    let ry = range * bearing.cos();

    // Relative velocity: theirs minus ours.
    let vx = other.speed * other.course.sin() - own.speed * own.course.sin();
    let vy = other.speed * other.course.cos() - own.speed * own.course.cos();

    let vv = vx * vx + vy * vy;
    if vv < 1e-9 {
        return None; // no relative motion; the range never changes
    }

    // Time that minimises |r + v t| is -(r·v)/(v·v).
    let t = -(rx * vx + ry * vy) / vv;
    let past = t < 0.0;
    let t_clamped = t.max(0.0);
    let cx = rx + vx * t_clamped;
    let cy = ry + vy * t_clamped;

    Some(Cpa {
        distance_m: (cx * cx + cy * cy).sqrt(),
        seconds: t_clamped,
        past,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distances_match_known_geodesics() {
        // One minute of latitude is a nautical mile, near enough — the classic
        // definition, and about 1855 m on the ellipsoid at this latitude.
        let d = distance_m(LatLon::new(56.0, 11.0), LatLon::new(56.0 + 1.0 / 60.0, 11.0));
        assert!((d - 1855.0).abs() < 5.0, "{d} m");

        // A degree of longitude at 56° N is roughly 62 km.
        let d = distance_m(LatLon::new(56.0, 11.0), LatLon::new(56.0, 12.0));
        assert!((d - 62_200.0).abs() < 300.0, "{d} m");

        // The same point is no distance at all.
        assert!(distance_m(LatLon::new(56.0, 11.0), LatLon::new(56.0, 11.0)) < 1e-6);
    }

    #[test]
    fn bearings_point_the_right_way() {
        let here = LatLon::new(56.0, 11.0);
        let (_, north) = range_bearing(here, LatLon::new(57.0, 11.0));
        assert!(north.to_degrees() < 0.1 || north.to_degrees() > 359.9, "{north}");
        let (_, east) = range_bearing(here, LatLon::new(56.0, 12.0));
        // Due east on the ellipsoid starts a little north of 090 and curves.
        assert!((east.to_degrees() - 90.0).abs() < 1.0, "{}", east.to_degrees());
        let (_, south) = range_bearing(here, LatLon::new(55.0, 11.0));
        assert!((south.to_degrees() - 180.0).abs() < 0.1);
    }

    #[test]
    fn advancing_and_measuring_are_inverses() {
        let start = LatLon::new(56.5, 11.6);
        for bearing_deg in [0.0, 45.0, 137.0, 271.0] {
            let b = (bearing_deg as f64).to_radians();
            let there = advance(start, b, 12_000.0);
            let (back, bearing) = range_bearing(start, there);
            assert!((back - 12_000.0).abs() < 1e-3, "{back}");
            assert!(
                (bearing.to_degrees() - bearing_deg).abs() < 1e-6,
                "{} vs {bearing_deg}",
                bearing.to_degrees()
            );
        }
    }

    #[test]
    fn a_head_on_pair_closes_to_nothing() {
        // Two vessels a mile apart on reciprocal courses, 5 m/s each.
        let own = Motion {
            at: LatLon::new(56.0, 11.0),
            course: 0.0, // due north
            speed: 5.0,
        };
        let ahead = advance(own.at, 0.0, METRES_PER_NM);
        let other = Motion {
            at: ahead,
            course: std::f64::consts::PI, // due south
            speed: 5.0,
        };
        let cpa = cpa(own, other).expect("they are closing");
        assert!(cpa.distance_m < 1.0, "{} m", cpa.distance_m);
        // A mile closed at ten metres a second: about 185 seconds.
        assert!((cpa.seconds - 185.2).abs() < 1.0, "{} s", cpa.seconds);
        assert!(!cpa.past);
    }

    #[test]
    fn a_parallel_pair_never_closes() {
        let own = Motion {
            at: LatLon::new(56.0, 11.0),
            course: 0.0,
            speed: 5.0,
        };
        // Half a mile abeam, same course and speed.
        let beside = advance(own.at, std::f64::consts::FRAC_PI_2, METRES_PER_NM / 2.0);
        let other = Motion {
            at: beside,
            course: 0.0,
            speed: 5.0,
        };
        // No relative motion at all: there is no approach to compute.
        assert!(cpa(own, other).is_none());

        // Same but slightly slower: they open, so the closest point is now.
        let other = Motion { speed: 4.0, ..other };
        let c = cpa(own, other).expect("relative motion exists");
        assert!((to_nm(c.distance_m) - 0.5).abs() < 0.01, "{}", to_nm(c.distance_m));
        assert_eq!(c.seconds, 0.0);
    }

    #[test]
    fn a_crossing_pair_has_a_real_closest_approach() {
        let own = Motion {
            at: LatLon::new(56.0, 11.0),
            course: 0.0, // north
            speed: 5.0,
        };
        // Two miles east, steering west at the same speed: they cross, and the
        // closest approach is neither zero nor the present range.
        let east = advance(own.at, std::f64::consts::FRAC_PI_2, 2.0 * METRES_PER_NM);
        let other = Motion {
            at: east,
            course: 3.0 * std::f64::consts::FRAC_PI_2,
            speed: 5.0,
        };
        let c = cpa(own, other).expect("they are closing");
        assert!(c.seconds > 0.0, "the approach is ahead of us");
        assert!(
            c.distance_m < 2.0 * METRES_PER_NM,
            "it should close from two miles"
        );
        assert!(!c.past);
    }

    #[test]
    fn typed_positions_parse_or_refuse() {
        let p = parse_latlon("56.41, 10.98").expect("plain decimal degrees");
        assert!((p.lat - 56.41).abs() < 1e-9 && (p.lon - 10.98).abs() < 1e-9);
        assert!(parse_latlon(" -33.9,151.2 ").is_some(), "southern hemisphere");
        assert!(parse_latlon("56.41").is_none(), "one number is not a place");
        assert!(parse_latlon("56.41,10.98,3").is_none());
        assert!(parse_latlon("91,0").is_none(), "off the planet");
        assert!(parse_latlon("56°24'N 10°58'E").is_none(), "DMS is not parsed, not misparsed");
        assert!(parse_latlon("").is_none());
    }

    #[test]
    fn an_approach_already_made_is_marked_past() {
        let own = Motion {
            at: LatLon::new(56.0, 11.0),
            course: 0.0,
            speed: 5.0,
        };
        // Astern of us and falling further behind.
        let astern = advance(own.at, std::f64::consts::PI, METRES_PER_NM);
        let other = Motion {
            at: astern,
            course: std::f64::consts::PI,
            speed: 5.0,
        };
        let c = cpa(own, other).expect("relative motion exists");
        assert!(c.past, "the closest approach was behind us");
        assert_eq!(c.seconds, 0.0);
    }
}
