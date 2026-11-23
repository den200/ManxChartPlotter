//! Distances and bearings on the actual shape of the Earth.
//!
//! Deliberately separate from [`crate::render::projection`], which is a
//! *drawing* projection and must stay exactly as it is: OSENC stores its
//! geometry in OpenCPN's Simple Mercator, and Manx reproduces that to the
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

/// Read a position a user typed.
///
/// Accepts what sailors and phones actually hand out:
/// - decimal degrees, `56.41, 10.98` or `56.41 10.98` (signed for S and W);
/// - decimal commas, when a semicolon separates the halves: `56,41; 10,98`;
/// - degrees and minutes, or degrees, minutes and seconds, with hemisphere
///   letters before or after: `56°24.6'N 10°58.8'E`, `N56 24.6 E010 58.8`,
///   `56 24 36N 10 58 48E` — including what [`format_latlon`] writes, so a
///   position shown anywhere in Manx can be typed back in.
///
/// Anything ambiguous or out of range — one number, three, minutes past 60,
/// two latitudes — is `None`, never a guess.
pub fn parse_latlon(s: &str) -> Option<LatLon> {
    let s = s.trim();
    // The mark between latitude and longitude: a semicolon when commas are
    // decimal points, a comma otherwise. It is kept, not thrown away as
    // space — with it gone, `56 24 36 1, 10 0` re-split its six numbers
    // three and three and read a position nobody typed.
    let (text, sep) = if s.contains(';') {
        (s.replace(',', "."), ';')
    } else {
        (s.to_string(), ',')
    };

    #[derive(Clone, Copy, PartialEq)]
    enum Tok {
        Num(f64),
        Hemi(char),
    }
    let mut toks = Vec::new();
    // How many tokens came before the separator, when there is one.
    let mut split: Option<usize> = None;
    let mut cur = String::new();
    let flush = |cur: &mut String, toks: &mut Vec<Tok>| -> Option<()> {
        if !cur.is_empty() {
            toks.push(Tok::Num(cur.parse().ok()?));
            cur.clear();
        }
        Some(())
    };
    for ch in text.chars() {
        let up = ch.to_ascii_uppercase();
        if ch == sep {
            flush(&mut cur, &mut toks)?;
            if split.is_some() {
                return None;
            }
            split = Some(toks.len());
        } else if matches!(up, 'N' | 'S' | 'E' | 'W') {
            flush(&mut cur, &mut toks)?;
            toks.push(Tok::Hemi(up));
        } else if ch.is_ascii_digit() || matches!(ch, '.' | '-' | '+') {
            cur.push(ch);
        } else if ch.is_whitespace() || matches!(ch, '°' | 'º' | '\'' | '"' | '′' | '″') {
            flush(&mut cur, &mut toks)?;
        } else {
            return None;
        }
    }
    flush(&mut cur, &mut toks)?;

    // Degrees, or degrees and minutes, or degrees, minutes and seconds.
    // Only the degrees may carry a sign.
    let angle = |nums: &[f64]| -> Option<f64> {
        let (d, rest) = nums.split_first()?;
        if rest.len() > 2 || rest.iter().any(|v| *v < 0.0 || *v >= 60.0) {
            return None;
        }
        if !rest.is_empty() && d.fract() != 0.0 {
            return None;
        }
        let m = rest.first().copied().unwrap_or(0.0);
        let sec = rest.get(1).copied().unwrap_or(0.0);
        if rest.len() == 2 && m.fract() != 0.0 {
            return None;
        }
        let mag = d.abs() + m / 60.0 + sec / 3600.0;
        Some(if *d < 0.0 || (*d == 0.0 && d.is_sign_negative()) { -mag } else { mag })
    };

    let hemis: Vec<usize> = toks
        .iter()
        .enumerate()
        .filter(|(_, t)| matches!(t, Tok::Hemi(_)))
        .map(|(i, _)| i)
        .collect();
    let nums_of = |ts: &[Tok]| -> Option<Vec<f64>> {
        ts.iter()
            .map(|t| match t {
                Tok::Num(v) => Some(*v),
                Tok::Hemi(_) => None,
            })
            .collect()
    };

    let (lat, lon) = match hemis.len() {
        0 => {
            let nums = nums_of(&toks)?;
            match nums.len() {
                2 | 4 | 6 => {
                    let half = nums.len() / 2;
                    if split.is_some_and(|at| at != half) {
                        return None;
                    }
                    (angle(&nums[..half])?, angle(&nums[half..])?)
                }
                _ => return None,
            }
        }
        2 => {
            // Letters all before their numbers, or all after; a separator,
            // if there is one, must fall between the two halves.
            let boundary = if hemis[0] == 0 { hemis[1] } else { hemis[0] + 1 };
            if split.is_some_and(|at| at != boundary) {
                return None;
            }
            let parts: Vec<(char, &[Tok])> = if hemis[0] == 0 {
                vec![
                    (letter(toks[0])?, &toks[1..hemis[1]]),
                    (letter(toks[hemis[1]])?, &toks[hemis[1] + 1..]),
                ]
            } else if hemis[1] == toks.len() - 1 {
                vec![
                    (letter(toks[hemis[0]])?, &toks[..hemis[0]]),
                    (letter(toks[hemis[1]])?, &toks[hemis[0] + 1..hemis[1]]),
                ]
            } else {
                return None;
            };
            let mut lat = None;
            let mut lon = None;
            for (h, ts) in parts {
                let nums = nums_of(ts)?;
                // With a hemisphere letter a sign would say it twice — and
                // `-0°30'N` is a sign too, though −0 is not less than 0.
                if nums.first().is_some_and(|d| d.is_sign_negative()) {
                    return None;
                }
                let v = angle(&nums)?;
                match h {
                    'N' if lat.is_none() => lat = Some(v),
                    'S' if lat.is_none() => lat = Some(-v),
                    'E' if lon.is_none() => lon = Some(v),
                    'W' if lon.is_none() => lon = Some(-v),
                    _ => return None,
                }
            }
            (lat?, lon?)
        }
        _ => return None,
    };
    fn letter(t: Tok) -> Option<char> {
        match t {
            Tok::Hemi(c) => Some(c),
            Tok::Num(_) => None,
        }
    }
    (lat.abs() <= 90.0 && lon.abs() <= 180.0).then(|| LatLon::new(lat, lon))
}

/// A position as Manx shows it: degrees and decimal minutes with
/// hemisphere letters, `56°24.62'N 010°58.81'E` — the form on the chart,
/// and one [`parse_latlon`] reads back. Hundredths of a minute are about
/// 18 m, fine enough for a waypoint in a harbour.
pub fn format_latlon(lat: f64, lon: f64) -> String {
    let one = |v: f64, width: usize, pos: char, neg: char| {
        let hemi = if v >= 0.0 { pos } else { neg };
        // Round in hundredths of a minute first, so 59.996' carries into
        // the next degree instead of printing as 60.00'.
        let total = (v.abs() * 6000.0).round() as i64;
        let (deg, cmin) = (total / 6000, total % 6000);
        format!("{deg:0width$}°{:02}.{:02}'{hemi}", cmin / 100, cmin % 100)
    };
    format!("{} {}", one(lat, 2, 'N', 'S'), one(lon, 3, 'E', 'W'))
}

/// A true bearing for display, `045°T`. Rounded before wrapping, so 359.6°
/// reads 000°T rather than 360°T.
pub fn format_bearing(deg: f64) -> String {
    format!("{:03.0}°T", deg.round().rem_euclid(360.0))
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

    /// Published WGS84 figures, not ones worked out here. The tolerances
    /// above would also pass a spherical (haversine) distance; these would
    /// not — on a sphere of mean radius the quarter meridian is 5.6 km long.
    #[test]
    fn distances_match_the_published_ellipsoid() {
        // Equator to pole along a meridian: 10 001 965.729 m.
        let d = distance_m(LatLon::new(0.0, 0.0), LatLon::new(90.0, 0.0));
        assert!((d - 10_001_965.729).abs() < 0.01, "{d} m");
        // One degree along the equator: a·π/180 = 111 319.491 m.
        let d = distance_m(LatLon::new(0.0, 10.0), LatLon::new(0.0, 11.0));
        assert!((d - 111_319.491).abs() < 0.01, "{d} m");
    }

    /// Across the antimeridian two points a degree apart are a degree apart,
    /// not 359 degrees, and east is still east.
    #[test]
    fn the_antimeridian_is_just_another_meridian() {
        let (d, b) = range_bearing(LatLon::new(0.0, 179.5), LatLon::new(0.0, -179.5));
        assert!((d - 111_319.491).abs() < 0.01, "{d} m");
        assert!((b.to_degrees() - 90.0).abs() < 1e-9, "{}", b.to_degrees());
        let there = advance(LatLon::new(0.0, 179.5), b, d);
        assert!((there.lon + 179.5).abs() < 1e-9, "{there:?}");
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

        // The doc's line is "no relative motion", and the code's noise floor
        // is a squared relative speed of 1e-9 m²/s². Exactly at it — a boat
        // stopped beside one creeping at 31 µm/s, which squares to that
        // floor exactly — there is still motion, so still an approach.
        let stopped = Motion { speed: 0.0, ..own };
        let creeping = Motion { speed: 3.162_277_660_168_379_5e-5, course: 0.0, ..other };
        assert_eq!(creeping.speed * creeping.speed, 1e-9);
        assert!(cpa(stopped, creeping).is_some());

        // Same but slightly slower: they open, so the closest point is now.
        let other = Motion { speed: 4.0, ..other };
        let c = cpa(own, other).expect("relative motion exists");
        assert!((to_nm(c.distance_m) - 0.5).abs() < 0.01, "{}", to_nm(c.distance_m));
        // "Now", to within floating-point noise: the geodesic arithmetic
        // lands on 7e-10 s on Linux and exactly 0 on macOS.
        assert!(c.seconds.abs() < 1e-6, "{}", c.seconds);
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
        // Relative motion is 5 m/s west and 5 m/s south from two miles east:
        // closest after 2 nm × 5 / 50 = 370.4 s, at a mile each way off —
        // √2 nm. A sine and cosine swapped anywhere moves either figure.
        assert!((c.seconds - 370.4).abs() < 1e-3, "{} s", c.seconds);
        let expected = METRES_PER_NM * std::f64::consts::SQRT_2;
        assert!((c.distance_m - expected).abs() < 1e-3, "{} m", c.distance_m);
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
        assert!(parse_latlon("").is_none());
    }

    #[test]
    fn positions_parse_in_every_form_a_sailor_writes() {
        let near = |p: Option<LatLon>, lat: f64, lon: f64| {
            let p = p.expect("should parse");
            assert!((p.lat - lat).abs() < 1e-6 && (p.lon - lon).abs() < 1e-6, "{p:?}");
        };
        near(parse_latlon("56°24.6'N 10°58.8'E"), 56.41, 10.98);
        near(parse_latlon("N56 24.6 E010 58.8"), 56.41, 10.98);
        near(parse_latlon("56 24 36N 10 58 48E"), 56.41, 10.98);
        near(parse_latlon("33°54.0'S 151°12.0'E"), -33.9, 151.2);
        near(parse_latlon("10°58.8'E 56°24.6'N"), 56.41, 10.98);
        near(parse_latlon("56.41 10.98"), 56.41, 10.98);
        near(parse_latlon("56,41; 10,98"), 56.41, 10.98);
        near(parse_latlon("56 24.6, 10 58.8"), 56.41, 10.98);
        // What Manx displays reads back in.
        let shown = format_latlon(56.41, -10.98);
        near(parse_latlon(&shown), 56.41, -10.98);

        assert!(parse_latlon("56°64'N 10°58'E").is_none(), "no 64th minute");
        assert!(parse_latlon("56°24'N 10°58'N").is_none(), "two latitudes");
        assert!(parse_latlon("-56°24'N 10°58'E").is_none(), "sign and letter disagree");
        assert!(parse_latlon("56.41 10.98 3").is_none());
        assert!(parse_latlon("somewhere").is_none());
    }

    #[test]
    fn positions_and_bearings_display_without_rounding_slips() {
        assert_eq!(format_latlon(56.41, 10.98), "56°24.60'N 010°58.80'E");
        assert_eq!(format_latlon(55.99999, -0.5), "56°00.00'N 000°30.00'W");
        assert_eq!(format_bearing(359.6), "000°T");
        assert_eq!(format_bearing(45.2), "045°T");
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

    /// Steering straight for a buoy on a course that is not a cardinal point
    /// meets it: both halves of our own velocity count. The earlier CPA tests
    /// all steer due north, where our east component is zero and the north
    /// one is the whole speed, so a slip in either went unseen.
    #[test]
    fn steering_straight_for_a_buoy_on_a_diagonal_meets_it() {
        for course_deg in [45.0_f64, 100.0, 222.0, 300.0] {
            let course = course_deg.to_radians();
            let start = LatLon::new(56.0, 11.0);
            let buoy = Motion { at: advance(start, course, METRES_PER_NM), course: 0.0, speed: 0.0 };
            let own = Motion { at: start, course, speed: 5.0 };
            let c = cpa(own, buoy).expect("we are closing the buoy");
            assert!(c.distance_m < 1e-3, "{course_deg}°: {} m", c.distance_m);
            // A mile at five metres a second.
            assert!((c.seconds - 370.4).abs() < 1e-3, "{course_deg}°: {} s", c.seconds);
            assert!(!c.past);
        }
    }

    /// Two vessels at the same spot are as close as they will ever be right
    /// now. That is a collision, not an approach behind us: callers hide the
    /// CPA of a `past` approach, and this one must not be hidden.
    #[test]
    fn a_vessel_on_top_of_us_is_not_behind_us() {
        let here = LatLon::new(56.0, 11.0);
        let own = Motion { at: here, course: 0.0, speed: 5.0 };
        let other = Motion { at: here, course: std::f64::consts::FRAC_PI_2, speed: 3.0 };
        let c = cpa(own, other).expect("relative motion exists");
        assert!(!c.past, "{c:?}");
        assert_eq!(c.seconds, 0.0);
        assert!(c.distance_m < 1e-9, "{c:?}");
    }

    /// A sign on the degrees is the hemisphere for anyone who writes signed
    /// positions: −33.9 is 33.9° S, and −12°15′ is 12°15′ W, the minutes
    /// going the same way as the degrees.
    #[test]
    fn a_minus_sign_means_south_or_west_and_carries_the_minutes() {
        let p = parse_latlon("-33.9, 151.2").unwrap();
        assert!((p.lat + 33.9).abs() < 1e-9 && (p.lon - 151.2).abs() < 1e-9, "{p:?}");
        let p = parse_latlon("55 40.5, -12 15").unwrap();
        assert!((p.lat - 55.675).abs() < 1e-9 && (p.lon + 12.25).abs() < 1e-9, "{p:?}");
        let p = parse_latlon("-12 15 36, 55 40 30").unwrap();
        assert!((p.lat + 12.26).abs() < 1e-9 && (p.lon - 55.675).abs() < 1e-9, "{p:?}");
    }

    /// Near the equator or the prime meridian the degrees are zero, and an
    /// unsigned zero is north or east like any other unsigned number.
    #[test]
    fn zero_degrees_and_some_minutes_stay_north_and_east() {
        let p = parse_latlon("0 30, 0 15").unwrap();
        assert_eq!((p.lat, p.lon), (0.5, 0.25));
        let p = parse_latlon("00°30.00'N 000°15.00'E").unwrap();
        assert_eq!((p.lat, p.lon), (0.5, 0.25));
        let p = parse_latlon("0°30'S 0°15'W").unwrap();
        assert_eq!((p.lat, p.lon), (-0.5, -0.25));
        let p = parse_latlon("S0 30 W0 15").unwrap();
        assert_eq!((p.lat, p.lon), (-0.5, -0.25));
    }

    /// Signed degrees-and-minutes have no other way to say "half a degree
    /// south" than −0°30′: the minus sign on a zero must still count.
    #[test]
    fn minus_zero_degrees_is_still_south_or_west() {
        let p = parse_latlon("-0 30, -0 15").unwrap();
        assert_eq!((p.lat, p.lon), (-0.5, -0.25));
        let p = parse_latlon("-0°30', -0°15'").unwrap();
        assert_eq!((p.lat, p.lon), (-0.5, -0.25));
    }

    /// A whole degree is written with zero minutes — `57°00'N` — and that
    /// zero is a minute like any other. Only the degrees may carry a sign,
    /// so a negative minute is a typo, not a position.
    #[test]
    fn zero_minutes_are_minutes_and_negative_ones_are_not() {
        let p = parse_latlon("57°00'N 011°00'E").unwrap();
        assert_eq!((p.lat, p.lon), (57.0, 11.0));
        let p = parse_latlon("57 0, 11 0").unwrap();
        assert_eq!((p.lat, p.lon), (57.0, 11.0));
        let p = parse_latlon("57 0 0, 11 0 0").unwrap();
        assert_eq!((p.lat, p.lon), (57.0, 11.0));
        let p = parse_latlon("57 30 0N 11 0 30E").unwrap();
        assert!((p.lat - 57.5).abs() < 1e-12 && (p.lon - (11.0 + 30.0 / 3600.0)).abs() < 1e-12);
        assert!(parse_latlon("56 -24.6, 10 58.8").is_none());
        assert!(parse_latlon("56 24 -36, 10 58 48").is_none());
        assert!(parse_latlon("56°-24.6'N 10°58.8'E").is_none());
    }

    /// The poles and the antimeridian are real places; a hair beyond them
    /// is not. Sixty minutes is the next degree, written as such.
    #[test]
    fn positions_at_the_edges_of_the_globe() {
        let p = parse_latlon("90°00'N 180°00'E").unwrap();
        assert_eq!((p.lat, p.lon), (90.0, 180.0));
        let p = parse_latlon("-90, -180").unwrap();
        assert_eq!((p.lat, p.lon), (-90.0, -180.0));
        assert!(parse_latlon("90.0001, 0").is_none());
        assert!(parse_latlon("0, 180.0001").is_none());
        assert!(parse_latlon("90°00.1'N 0°00'E").is_none());
        assert!(parse_latlon("0°00'N 180°00.1'W").is_none());
        assert!(parse_latlon("56°60'N 10°00'E").is_none(), "sixty minutes is 57°");
        assert!(parse_latlon("56 24 60N 10 0 0E").is_none(), "sixty seconds is a minute");
    }

    /// Lower-case letters and no gaps are how positions get typed on a phone.
    #[test]
    fn hemisphere_letters_in_any_case_and_without_spaces() {
        let p = parse_latlon("56 24.6 n 10 58.8 e").unwrap();
        assert!((p.lat - 56.41).abs() < 1e-9 && (p.lon - 10.98).abs() < 1e-9, "{p:?}");
        let p = parse_latlon("56°24.6'N10°58.8'W").unwrap();
        assert!((p.lat - 56.41).abs() < 1e-9 && (p.lon + 10.98).abs() < 1e-9, "{p:?}");
    }

    /// Half a position, or a position with its parts muddled, is refused
    /// rather than guessed at.
    #[test]
    fn missing_or_muddled_parts_are_refused() {
        for s in [
            "N56 24.6",            // a latitude alone
            "56 24.6 N",           // likewise, letter after
            "56.41N, 10.98",       // one letter for two numbers
            "N E",                 // letters, no numbers
            "N56 24.6 10 58.8E",   // a letter before one half, after the other
            "56 24.6 N E 10 58.8", // the second letter belongs to nothing
            "56°24.6'E 10°58.8'W", // two longitudes
            "56.5 30, 10 0",       // fractional degrees and minutes as well
            "56 24.5 30, 10 0 0",  // fractional minutes and seconds as well
            "56 24 36 1 10",       // five numbers: no way to halve them
            "56.4.1, 10",          // not a number
            "56.41; 10.98; 3",     // three numbers
            "56.41 10.98 x",       // stray character
            "  ",
        ] {
            assert!(parse_latlon(s).is_none(), "{s:?} parsed as {:?}", parse_latlon(s));
        }
    }

    /// `-0°30'N` writes a sign and a letter at once, exactly like
    /// `-56°24'N`, which is refused because the two would say the hemisphere
    /// twice. The zero must not slip through: it currently parses as
    /// 0°30' *south* despite the N.
    #[test]
    fn a_minus_zero_with_a_hemisphere_letter_is_refused_like_any_other_sign() {
        assert_eq!(parse_latlon("-0°30'N 0°15'E"), None);
        assert_eq!(parse_latlon("0°30'N -0°15'E"), None);
    }

    /// When a comma separates the halves it says where one angle ends, so
    /// four numbers before it cannot be one angle. The parser drops the
    /// comma and halves the six numbers instead, reading this as
    /// 56°24'36" and 1°10'00" — a guess, which the doc rules out.
    #[test]
    fn a_comma_in_the_wrong_place_is_not_ignored() {
        assert_eq!(parse_latlon("56 24 36 1, 10 0"), None);
    }

    use proptest::prelude::*;

    fn any_position() -> impl Strategy<Value = LatLon> {
        (-90.0..=90.0_f64, -180.0..=180.0_f64).prop_map(|(lat, lon)| LatLon::new(lat, lon))
    }

    /// Smallest angle between two bearings in radians, whatever their wrap.
    fn bearing_gap(a: f64, b: f64) -> f64 {
        let d = (a - b).rem_euclid(std::f64::consts::TAU);
        d.min(std::f64::consts::TAU - d)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn distance_is_symmetric_and_never_negative(a in any_position(), b in any_position()) {
            let ab = distance_m(a, b);
            let ba = distance_m(b, a);
            prop_assert!(ab >= 0.0);
            prop_assert!((ab - ba).abs() < 1e-6, "{ab} vs {ba}");
            // No two points on the Earth are further apart than half the
            // equator, a·π = 20 037 508.34 m.
            prop_assert!(ab <= 20_037_508.35, "{ab}");
        }

        #[test]
        fn no_detour_is_shorter_than_the_direct_way(
            a in any_position(), b in any_position(), c in any_position()
        ) {
            let direct = distance_m(a, c);
            let detour = distance_m(a, b) + distance_m(b, c);
            prop_assert!(direct <= detour + 1e-6, "{direct} > {detour}");
        }

        /// Away from the poles, where a bearing stops meaning much.
        #[test]
        fn advancing_then_measuring_gives_back_the_distance_and_bearing(
            lat in -80.0..80.0_f64,
            lon in -180.0..180.0_f64,
            bearing in 0.0..std::f64::consts::TAU,
            d in 1.0..2_000_000.0_f64,
        ) {
            let start = LatLon::new(lat, lon);
            let there = advance(start, bearing, d);
            prop_assert!(there.lat.abs() <= 90.0 && there.lon.abs() <= 180.0, "{there:?}");
            let (back, b) = range_bearing(start, there);
            prop_assert!((back - d).abs() < 1e-6 * d.max(1.0), "{back} vs {d}");
            prop_assert!(bearing_gap(b, bearing) < 1e-8, "{b} vs {bearing}");
            prop_assert!((0.0..std::f64::consts::TAU).contains(&b), "{b}");
        }

        /// Sailing east over the date line lands at a western longitude a
        /// short way off, never most of the way round the world.
        #[test]
        fn sailing_east_over_the_date_line_lands_just_across_it(
            lat in -60.0..60.0_f64,
            lon in 179.0..180.0_f64,
            d in 150_000.0..300_000.0_f64,
        ) {
            let start = LatLon::new(lat, lon);
            let there = advance(start, std::f64::consts::FRAC_PI_2, d);
            prop_assert!(there.lon < -170.0 && there.lon >= -180.0, "{there:?}");
            prop_assert!((distance_m(start, there) - d).abs() < 1e-6 * d, "{there:?}");
            let (_, b) = range_bearing(there, start);
            // Going back is westward.
            prop_assert!(bearing_gap(b, 1.5 * std::f64::consts::PI) < 0.1, "{b}");
        }

        /// Hundredths of a minute is the printed precision: half of one is
        /// 1/12 000 of a degree.
        #[test]
        fn a_shown_position_types_back_in_to_the_printed_precision(p in any_position()) {
            let shown = format_latlon(p.lat, p.lon);
            let back = parse_latlon(&shown);
            prop_assert!(back.is_some(), "{shown}");
            let back = back.unwrap();
            let tol = 1.0 / 12_000.0 + 1e-9;
            prop_assert!((back.lat - p.lat).abs() <= tol, "{shown} -> {back:?} from {p:?}");
            prop_assert!((back.lon - p.lon).abs() <= tol, "{shown} -> {back:?} from {p:?}");
        }

        /// Signed decimal degrees, as Rust prints them, read back exactly.
        #[test]
        fn decimal_degrees_read_back_exactly(p in any_position()) {
            let typed = format!("{}, {}", p.lat, p.lon);
            prop_assert_eq!(parse_latlon(&typed), Some(p), "{}", typed);
            let typed = format!("{} {}", p.lat, p.lon);
            prop_assert_eq!(parse_latlon(&typed), Some(p), "{}", typed);
        }

        /// The closest approach is never further than the range now, and an
        /// approach marked past is happening now, at the present range.
        #[test]
        fn the_closest_approach_is_never_further_than_now(
            bearing in 0.0..std::f64::consts::TAU,
            range in 0.0..20_000.0_f64,
            own_course in 0.0..std::f64::consts::TAU,
            own_speed in 0.0..15.0_f64,
            their_course in 0.0..std::f64::consts::TAU,
            their_speed in 0.0..15.0_f64,
        ) {
            let here = LatLon::new(56.0, 11.0);
            let own = Motion { at: here, course: own_course, speed: own_speed };
            let other = Motion { at: advance(here, bearing, range), course: their_course, speed: their_speed };
            if let Some(c) = cpa(own, other) {
                let now = distance_m(own.at, other.at);
                prop_assert!(c.distance_m >= 0.0 && c.seconds >= 0.0, "{c:?}");
                prop_assert!(c.distance_m <= now + 1e-6, "{c:?} vs {now}");
                if c.past {
                    prop_assert_eq!(c.seconds, 0.0);
                    prop_assert!((c.distance_m - now).abs() < 1e-6, "{c:?} vs {now}");
                }
            }
        }

        /// Heading straight at a stationary mark from any direction meets it,
        /// after range ÷ speed seconds.
        #[test]
        fn heading_straight_at_a_mark_meets_it(
            course in 0.0..std::f64::consts::TAU,
            range in 100.0..20_000.0_f64,
            speed in 0.5..15.0_f64,
        ) {
            let here = LatLon::new(56.0, 11.0);
            let mark = Motion { at: advance(here, course, range), course: 0.0, speed: 0.0 };
            let c = cpa(Motion { at: here, course, speed }, mark).unwrap();
            prop_assert!(c.distance_m < 1e-3, "{c:?}");
            prop_assert!((c.seconds - range / speed).abs() < 1e-6 * c.seconds.max(1.0), "{c:?}");
            prop_assert!(!c.past);
        }
    }
}
