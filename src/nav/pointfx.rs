//! The point forecast: everything the weather sheet shows for one place.
//!
//! A field and a point are different questions. The GRIB in [`super::grib`]
//! answers "what is the wind doing across this sea", which is what the barbs
//! on the chart draw. This module answers "what is the weather doing *here*,
//! hour by hour, for the next few days" — wind and gusts, rain, sea state,
//! the set of the tide and the height of the water. That is the question a
//! sheet with a time axis exists to answer, and it wants a different shape of
//! data: dense in time, single in space.
//!
//! Two keyless calls, both Open-Meteo:
//!
//! * the forecast API for wind, gusts, precipitation and the day's sunrise
//!   and sunset — the last so the sheet can shade the night;
//! * the marine API for waves, surface current and sea level, the same
//!   service [`super::currents`] already uses for routing, so the current on
//!   the sheet is the current the router used.
//!
//! The sea half is allowed to fail. A forecast without waves is still a
//! forecast; a forecast without wind is nothing, so that half is fatal.
//!
//! §8.5's rule holds here as everywhere: directions interpolate as unit
//! vectors, never as degrees, so nothing goes wrong at the 0/360 seam.

use chrono::{DateTime, Utc};

/// What to call the source wherever it is shown.
pub const POINT_SOURCE_LABEL: &str = "Open-Meteo — forecast blend, marine SMOC";

const KMH_TO_KNOTS: f64 = 1000.0 / 3600.0 * 3600.0 / 1852.0;

/// One hour's worth of everything, already interpolated to an instant.
///
/// Every field is optional and separately so: the marine call may have failed
/// while the wind arrived, a lake has no tide, and a lane that has nothing to
/// say should say nothing rather than draw a zero.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sample {
    pub wind_kt: Option<f32>,
    pub gust_kt: Option<f32>,
    /// Where the wind comes *from*, degrees true — the mariner's convention.
    pub wind_from_deg: Option<f32>,
    /// Precipitation in the hour, mm.
    pub rain_mm: Option<f32>,
    pub wave_m: Option<f32>,
    /// Where the sea comes from, degrees true.
    pub wave_from_deg: Option<f32>,
    pub wave_period_s: Option<f32>,
    pub current_kt: Option<f32>,
    /// Where the current sets *towards*, degrees true. Currents are named the
    /// other way round from wind, and mixing the two has sunk boats.
    pub current_to_deg: Option<f32>,
    /// Sea level above mean sea level, metres. Not a chart datum height —
    /// see [`PointForecast::tide_m`].
    pub tide_m: Option<f32>,
}

/// A turn of the tide: the moment the water stops rising or stops falling.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TideExtreme {
    pub time_ms: i64,
    pub height_m: f32,
    pub high: bool,
}

/// Sunrise and sunset for one day, ms UTC.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SunDay {
    pub sunrise_ms: i64,
    pub sunset_ms: i64,
}

/// The hourly series for one position.
///
/// Every series is the same length as [`times`](Self::times) and holds `None`
/// where the model said nothing.
#[derive(Debug, Clone, Default)]
pub struct PointForecast {
    pub lat: f64,
    pub lon: f64,
    /// ms UTC, ascending, hourly.
    pub times: Vec<i64>,
    pub wind_kt: Vec<Option<f32>>,
    pub gust_kt: Vec<Option<f32>>,
    pub wind_from_deg: Vec<Option<f32>>,
    pub rain_mm: Vec<Option<f32>>,
    pub wave_m: Vec<Option<f32>>,
    pub wave_from_deg: Vec<Option<f32>>,
    pub wave_period_s: Vec<Option<f32>>,
    pub current_kt: Vec<Option<f32>>,
    pub current_to_deg: Vec<Option<f32>>,
    /// Sea level above MSL, metres. Open-Meteo's `sea_level_height_msl` is
    /// the model's total water level, so it carries the tide *and* whatever
    /// surge the weather is pushing — which for a plotter is the useful
    /// number, and is emphatically not a tide table's height above chart
    /// datum. The sheet says so.
    pub tide_m: Vec<Option<f32>>,
    pub sun: Vec<SunDay>,
    /// Did the marine call come back? False means the sea lanes are empty
    /// because nobody answered, not because the sea is flat.
    pub has_sea: bool,
}

/// Wherever the fetch could not reach, in words fit for the status line.
type FetchResult = Result<PointForecast, String>;

impl PointForecast {
    /// Fetch `days` days of hourly weather at one position. Blocking; run it
    /// on a worker.
    pub fn fetch(lat: f64, lon: f64, days: u32) -> FetchResult {
        let days = days.clamp(1, 8);
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(45)))
            .build()
            .into();
        let get = |url: String| -> Result<String, String> {
            agent
                .get(&url)
                .call()
                .map_err(|e| format!("{e}"))?
                .body_mut()
                .read_to_string()
                .map_err(|e| format!("{e}"))
        };

        let air = get(format!(
            "https://api.open-meteo.com/v1/forecast?latitude={lat:.4}&longitude={lon:.4}\
             &hourly=wind_speed_10m,wind_direction_10m,wind_gusts_10m,precipitation\
             &daily=sunrise,sunset&wind_speed_unit=kn&timeformat=unixtime&timezone=UTC\
             &forecast_days={days}"
        ))
        .map_err(|e| format!("forecast fetch failed: {e}"))?;
        let mut forecast = Self::from_air_json(&air, lat, lon)?;

        // The sea is a refinement, exactly as it is for the router: log the
        // failure, show the wind anyway.
        match get(format!(
            "https://marine-api.open-meteo.com/v1/marine?latitude={lat:.4}&longitude={lon:.4}\
             &hourly=wave_height,wave_direction,wave_period,ocean_current_velocity,\
             ocean_current_direction,sea_level_height_msl\
             &timeformat=unixtime&timezone=UTC&forecast_days={days}"
        )) {
            Ok(sea) => {
                if let Err(e) = forecast.absorb_sea_json(&sea) {
                    log::info!("pointfx: marine answer unusable: {e}");
                }
            }
            Err(e) => log::info!("pointfx: marine fetch failed: {e}"),
        }
        log::info!(
            "pointfx: {:.3},{:.3} — {} hours, sea {}",
            lat,
            lon,
            forecast.times.len(),
            if forecast.has_sea { "yes" } else { "no" }
        );
        Ok(forecast)
    }

    /// Parse the forecast API's answer. Split out as the test seam.
    fn from_air_json(text: &str, lat: f64, lon: f64) -> FetchResult {
        let root: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("forecast answer unreadable: {e}"))?;
        if let Some(reason) = root.get("reason").and_then(|r| r.as_str()) {
            return Err(format!("forecast refused: {reason}"));
        }
        let hourly = root
            .get("hourly")
            .ok_or("forecast answer has no hourly block")?;
        let times = time_series(hourly).ok_or("forecast answer has no hourly.time")?;
        if times.is_empty() {
            return Err("forecast answer covers no hours".into());
        }
        let n = times.len();
        let mut f = Self {
            lat,
            lon,
            wind_kt: series(hourly, "wind_speed_10m", n),
            gust_kt: series(hourly, "wind_gusts_10m", n),
            wind_from_deg: series(hourly, "wind_direction_10m", n),
            rain_mm: series(hourly, "precipitation", n),
            wave_m: vec![None; n],
            wave_from_deg: vec![None; n],
            wave_period_s: vec![None; n],
            current_kt: vec![None; n],
            current_to_deg: vec![None; n],
            tide_m: vec![None; n],
            sun: Vec::new(),
            has_sea: false,
            times,
        };
        if let Some(daily) = root.get("daily") {
            let rise = int_series(daily, "sunrise");
            let set = int_series(daily, "sunset");
            for (a, b) in rise.iter().zip(set.iter()) {
                if let (Some(a), Some(b)) = (a, b) {
                    f.sun.push(SunDay {
                        sunrise_ms: a * 1000,
                        sunset_ms: b * 1000,
                    });
                }
            }
        }
        Ok(f)
    }

    /// Fold the marine API's answer onto the hour axis already established.
    ///
    /// Matched by timestamp rather than by index. The two services agree in
    /// practice, and relying on that agreement is exactly the sort of thing
    /// that shifts a tide curve by an hour the day one of them changes its
    /// horizon.
    fn absorb_sea_json(&mut self, text: &str) -> Result<(), String> {
        let root: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("marine answer unreadable: {e}"))?;
        if let Some(reason) = root.get("reason").and_then(|r| r.as_str()) {
            return Err(format!("marine refused: {reason}"));
        }
        let hourly = root.get("hourly").ok_or("marine answer has no hourly block")?;
        let times = time_series(hourly).ok_or("marine answer has no hourly.time")?;
        let n = times.len();
        let wave = series(hourly, "wave_height", n);
        let wave_dir = series(hourly, "wave_direction", n);
        let period = series(hourly, "wave_period", n);
        let cur = series(hourly, "ocean_current_velocity", n);
        let cur_dir = series(hourly, "ocean_current_direction", n);
        let level = series(hourly, "sea_level_height_msl", n);

        let index: std::collections::HashMap<i64, usize> = self
            .times
            .iter()
            .enumerate()
            .map(|(i, &t)| (t, i))
            .collect();
        let mut landed = 0usize;
        for (j, t) in times.iter().enumerate() {
            let Some(&i) = index.get(t) else { continue };
            self.wave_m[i] = wave[j];
            self.wave_from_deg[i] = wave_dir[j];
            self.wave_period_s[i] = period[j];
            // km/h in, knots out — the sheet speaks knots everywhere.
            self.current_kt[i] = cur[j].map(|v| (v as f64 * KMH_TO_KNOTS) as f32);
            self.current_to_deg[i] = cur_dir[j];
            self.tide_m[i] = level[j];
            landed += 1;
        }
        if landed == 0 {
            return Err("marine answer shares no hours with the forecast".into());
        }
        self.has_sea = true;
        Ok(())
    }

    /// First and last hour covered, ms UTC.
    pub fn span(&self) -> Option<(i64, i64)> {
        Some((*self.times.first()?, *self.times.last()?))
    }

    /// Everything at an instant, interpolated. Outside the span every field
    /// is `None`: a sheet that extrapolates weather is lying.
    pub fn at(&self, time_ms: i64) -> Sample {
        let Some((first, last)) = self.span() else {
            return Sample::default();
        };
        if time_ms < first || time_ms > last {
            return Sample::default();
        }
        let (i0, i1, f) = bracket(&self.times, time_ms);
        let lerp = |s: &[Option<f32>]| -> Option<f32> {
            match (s.get(i0).copied().flatten(), s.get(i1).copied().flatten()) {
                (Some(a), Some(b)) => Some(a + (b - a) * f as f32),
                (Some(a), None) | (None, Some(a)) => Some(a),
                (None, None) => None,
            }
        };
        Sample {
            wind_kt: lerp(&self.wind_kt),
            gust_kt: lerp(&self.gust_kt),
            wind_from_deg: lerp_dir(&self.wind_from_deg, i0, i1, f),
            // Rain is a total over the hour, not a level: the honest reading
            // at any instant inside that hour is the hour's own total.
            rain_mm: self.rain_mm.get(i0).copied().flatten(),
            wave_m: lerp(&self.wave_m),
            wave_from_deg: lerp_dir(&self.wave_from_deg, i0, i1, f),
            wave_period_s: lerp(&self.wave_period_s),
            current_kt: lerp(&self.current_kt),
            current_to_deg: lerp_dir(&self.current_to_deg, i0, i1, f),
            tide_m: lerp(&self.tide_m),
        }
    }

    /// The turns of the tide: every local high and low in the series.
    ///
    /// Refined by fitting a parabola through the three samples around the
    /// turn, because the water does not turn on the hour and an hourly series
    /// alone would round high water to the nearest sixty minutes.
    pub fn tide_extremes(&self) -> Vec<TideExtreme> {
        let mut out = Vec::new();
        // The series are built together and are always the same length; the
        // guard is here so a hand-made forecast in a test — or a future
        // partial fill — cannot turn a display bug into a panic at sea.
        if self.times.len() < 3 || self.tide_m.len() != self.times.len() {
            return out;
        }
        for i in 1..self.times.len() - 1 {
            let (Some(a), Some(b), Some(c)) = (
                self.tide_m[i - 1],
                self.tide_m[i],
                self.tide_m[i + 1],
            ) else {
                continue;
            };
            let high = b >= a && b >= c && (b > a || b > c);
            let low = b <= a && b <= c && (b < a || b < c);
            if !high && !low {
                continue;
            }
            // Vertex of the parabola through (−1,a), (0,b), (1,c), in samples.
            let denom = a - 2.0 * b + c;
            let shift = if denom.abs() < 1e-6 {
                0.0
            } else {
                (0.5 * (a - c) / denom).clamp(-0.5, 0.5)
            };
            let height = b - 0.25 * (a - c) * shift;
            let step = (self.times[i + 1] - self.times[i - 1]) as f64 / 2.0;
            out.push(TideExtreme {
                time_ms: self.times[i] + (shift as f64 * step) as i64,
                height_m: height,
                high,
            });
        }
        out
    }

    /// Lowest and highest water in the series, for scaling the tide lane.
    pub fn tide_range(&self) -> Option<(f32, f32)> {
        let mut lo = f32::INFINITY;
        let mut hi = f32::NEG_INFINITY;
        for v in self.tide_m.iter().flatten() {
            lo = lo.min(*v);
            hi = hi.max(*v);
        }
        (lo.is_finite() && hi.is_finite()).then_some((lo, hi))
    }

    /// The largest value in a series, for scaling a lane. `None` when the
    /// series is empty of data.
    pub fn peak(series: &[Option<f32>]) -> Option<f32> {
        series
            .iter()
            .flatten()
            .copied()
            .fold(None, |acc: Option<f32>, v| {
                Some(acc.map_or(v, |a| a.max(v)))
            })
    }

    /// Is `time_ms` in darkness?
    ///
    /// Daylight is any `[sunrise, sunset)` the API sent; everything else
    /// inside the covered days is night. A forecast that came back without
    /// sun times reports daylight throughout rather than guessing — a wrongly
    /// shaded night is worse than none.
    pub fn is_night(&self, time_ms: i64) -> bool {
        if self.sun.is_empty() {
            return false;
        }
        !self
            .sun
            .iter()
            .any(|d| time_ms >= d.sunrise_ms && time_ms < d.sunset_ms)
    }

    #[cfg(test)]
    fn synthetic(times: Vec<i64>, tide: Vec<Option<f32>>) -> Self {
        let n = times.len();
        Self {
            lat: 55.0,
            lon: 12.0,
            wind_kt: vec![None; n],
            gust_kt: vec![None; n],
            wind_from_deg: vec![None; n],
            rain_mm: vec![None; n],
            wave_m: vec![None; n],
            wave_from_deg: vec![None; n],
            wave_period_s: vec![None; n],
            current_kt: vec![None; n],
            current_to_deg: vec![None; n],
            tide_m: tide,
            sun: Vec::new(),
            has_sea: true,
            times,
        }
    }
}

/// A position written the way a chart writes it.
pub fn place_label(lat: f64, lon: f64) -> String {
    let one = |v: f64, pos: char, neg: char| {
        let hemi = if v >= 0.0 { pos } else { neg };
        let a = v.abs();
        let deg = a.floor();
        let min = (a - deg) * 60.0;
        // A plain apostrophe, not the typographic prime: egui's bundled
        // fonts have no U+2032 and it drew as an empty box.
        format!("{deg:.0}°{min:04.1}'{hemi}")
    };
    format!("{} {}", one(lat, 'N', 'S'), one(lon, 'E', 'W'))
}

/// `hourly.time` as ms UTC. `timeformat=unixtime` gives seconds; anything
/// else in there is a sign the request changed and is refused rather than
/// guessed at.
fn time_series(hourly: &serde_json::Value) -> Option<Vec<i64>> {
    let a = hourly.get("time")?.as_array()?;
    let mut out = Vec::with_capacity(a.len());
    for v in a {
        out.push(v.as_i64()? * 1000);
    }
    Some(out)
}

fn series(hourly: &serde_json::Value, key: &str, n: usize) -> Vec<Option<f32>> {
    let Some(a) = hourly.get(key).and_then(|v| v.as_array()) else {
        return vec![None; n];
    };
    let mut out: Vec<Option<f32>> = a
        .iter()
        .map(|v| v.as_f64().map(|x| x as f32))
        .collect();
    out.resize(n, None);
    out
}

fn int_series(block: &serde_json::Value, key: &str) -> Vec<Option<i64>> {
    block
        .get(key)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(|v| v.as_i64()).collect())
        .unwrap_or_default()
}

/// Interpolate a bearing through its unit vector, so 350° and 10° average to
/// 0° and not to 180°.
fn lerp_dir(s: &[Option<f32>], i0: usize, i1: usize, f: f64) -> Option<f32> {
    let a = s.get(i0).copied().flatten();
    let b = s.get(i1).copied().flatten();
    match (a, b) {
        (Some(a), Some(b)) => {
            let (ar, br) = ((a as f64).to_radians(), (b as f64).to_radians());
            let x = ar.sin() * (1.0 - f) + br.sin() * f;
            let y = ar.cos() * (1.0 - f) + br.cos() * f;
            if x.abs() < 1e-9 && y.abs() < 1e-9 {
                Some(a)
            } else {
                Some(x.atan2(y).to_degrees().rem_euclid(360.0) as f32)
            }
        }
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    }
}

/// Bracket an ascending axis, clamped.
fn bracket(axis: &[i64], x: i64) -> (usize, usize, f64) {
    if axis.len() < 2 {
        return (0, 0, 0.0);
    }
    let n = axis.len();
    for i in 0..n - 1 {
        if x <= axis[i + 1] {
            let span = (axis[i + 1] - axis[i]) as f64;
            let f = if span <= 0.0 {
                0.0
            } else {
                ((x - axis[i]) as f64 / span).clamp(0.0, 1.0)
            };
            return (i, i + 1, f);
        }
    }
    (n - 2, n - 1, 1.0)
}

/// Format an instant the way a crew says it, in the boat's own time zone.
pub fn local_hhmm(time_ms: i64) -> String {
    local(time_ms).format("%H:%M").to_string()
}

pub fn local_day(time_ms: i64) -> String {
    local(time_ms).format("%a %d").to_string()
}

fn local(time_ms: i64) -> DateTime<chrono::Local> {
    use chrono::TimeZone as _;
    let utc: DateTime<Utc> = Utc
        .timestamp_millis_opt(time_ms)
        .single()
        .unwrap_or_else(Utc::now);
    utc.with_timezone(&chrono::Local)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3_600_000;

    fn air_json() -> String {
        // Two hours, unixtime seconds, exactly the shape Open-Meteo returns.
        r#"{"latitude":55.7,"longitude":12.6,
            "hourly":{"time":[1000000,1003600,1007200],
                      "wind_speed_10m":[10.0,20.0,null],
                      "wind_direction_10m":[350.0,10.0,null],
                      "wind_gusts_10m":[14.0,28.0,null],
                      "precipitation":[0.0,1.5,null]},
            "daily":{"sunrise":[999000],"sunset":[1006000]}}"#
            .into()
    }

    #[test]
    fn the_wind_series_parses_and_interpolates_between_hours() {
        let f = PointForecast::from_air_json(&air_json(), 55.7, 12.6).unwrap();
        assert_eq!(f.times, vec![1_000_000_000, 1_003_600_000, 1_007_200_000]);
        let mid = f.at(1_000_000_000 + HOUR / 2);
        assert!((mid.wind_kt.unwrap() - 15.0).abs() < 1e-4);
        // 350° and 10° must meet at north, not at south. This is the seam
        // that catches a naive average.
        let d = mid.wind_from_deg.unwrap();
        assert!(d < 1.0 || d > 359.0, "direction crossed the wrong way: {d}");
    }

    #[test]
    fn outside_the_span_nothing_is_reported_rather_than_extrapolated() {
        let f = PointForecast::from_air_json(&air_json(), 55.7, 12.6).unwrap();
        assert_eq!(f.at(999_000_000), Sample::default());
        assert_eq!(f.at(1_100_000_000), Sample::default());
    }

    #[test]
    fn a_missing_hour_reads_as_missing_not_as_zero() {
        let f = PointForecast::from_air_json(&air_json(), 55.7, 12.6).unwrap();
        // The third hour is null in every series; the second is not.
        assert_eq!(f.wind_kt[2], None);
        assert_eq!(f.at(1_007_200_000).wind_kt, Some(20.0));
    }

    #[test]
    fn the_sea_lands_on_the_hours_it_shares_and_not_on_the_others() {
        let mut f = PointForecast::from_air_json(&air_json(), 55.7, 12.6).unwrap();
        // The marine answer starts an hour later, as it may.
        f.absorb_sea_json(
            r#"{"hourly":{"time":[1003600,1007200],
                          "wave_height":[1.2,1.4],
                          "wave_direction":[200.0,210.0],
                          "wave_period":[5.0,5.5],
                          "ocean_current_velocity":[1.852,3.704],
                          "ocean_current_direction":[90.0,95.0],
                          "sea_level_height_msl":[0.4,0.9]}}"#,
        )
        .unwrap();
        assert!(f.has_sea);
        assert_eq!(f.wave_m[0], None, "the unshared first hour must stay empty");
        assert_eq!(f.wave_m[1], Some(1.2));
        // 1.852 km/h is exactly one knot.
        assert!((f.current_kt[1].unwrap() - 1.0).abs() < 1e-3);
        assert!((f.current_kt[2].unwrap() - 2.0).abs() < 1e-3);
    }

    #[test]
    fn a_marine_answer_that_shares_no_hour_is_refused() {
        let mut f = PointForecast::from_air_json(&air_json(), 55.7, 12.6).unwrap();
        let e = f
            .absorb_sea_json(r#"{"hourly":{"time":[500],"wave_height":[1.0]}}"#)
            .unwrap_err();
        assert!(e.contains("shares no hours"), "{e}");
        assert!(!f.has_sea);
    }

    /// The turn of the tide is the number a mariner actually plans around,
    /// and it does not happen on the hour.
    #[test]
    fn high_water_is_refined_off_the_sampled_hour() {
        // A symmetric peak: the vertex sits exactly on the middle sample.
        let t: Vec<i64> = (0..5).map(|i| i as i64 * HOUR).collect();
        let f = PointForecast::synthetic(
            t.clone(),
            vec![Some(0.0), Some(1.0), Some(1.6), Some(1.0), Some(0.0)],
        );
        let ex = f.tide_extremes();
        assert_eq!(ex.len(), 1);
        assert!(ex[0].high);
        assert_eq!(ex[0].time_ms, 2 * HOUR);

        // Skewed: high water lies between the samples, and the fitted height
        // is above the highest one we actually have.
        let f = PointForecast::synthetic(
            t,
            vec![Some(0.0), Some(1.0), Some(1.6), Some(1.5), Some(0.4)],
        );
        let ex = f.tide_extremes();
        assert_eq!(ex.len(), 1);
        assert!(
            ex[0].time_ms > 2 * HOUR && ex[0].time_ms < 3 * HOUR,
            "high water at {} should fall between the samples",
            ex[0].time_ms
        );
        assert!(ex[0].height_m > 1.6);
    }

    #[test]
    fn both_turns_are_found_and_named() {
        let t: Vec<i64> = (0..7).map(|i| i as i64 * HOUR).collect();
        let f = PointForecast::synthetic(
            t,
            vec![
                Some(1.0),
                Some(1.8),
                Some(1.0),
                Some(-0.4),
                Some(-1.1),
                Some(-0.2),
                Some(0.9),
            ],
        );
        let ex = f.tide_extremes();
        assert_eq!(ex.len(), 2);
        assert!(ex[0].high);
        assert!(!ex[1].high);
        assert_eq!(f.tide_range(), Some((-1.1, 1.8)));
    }

    #[test]
    fn a_flat_series_has_no_turns_to_report() {
        let t: Vec<i64> = (0..5).map(|i| i as i64 * HOUR).collect();
        let f = PointForecast::synthetic(t, vec![Some(0.5); 5]);
        assert!(f.tide_extremes().is_empty());
    }

    #[test]
    fn rain_reports_the_hour_it_fell_in_rather_than_a_blend() {
        let f = PointForecast::from_air_json(&air_json(), 55.7, 12.6).unwrap();
        // Half way through the first hour it is still that hour's total.
        assert_eq!(f.at(1_000_000_000 + HOUR / 2).rain_mm, Some(0.0));
        assert_eq!(f.at(1_003_600_000 + HOUR / 2).rain_mm, Some(1.5));
    }

    #[test]
    fn a_refusal_from_the_api_is_reported_not_parsed() {
        let e = PointForecast::from_air_json(r#"{"error":true,"reason":"Bad latitude"}"#, 0.0, 0.0)
            .unwrap_err();
        assert!(e.contains("Bad latitude"), "{e}");
    }

    #[test]
    fn night_is_everything_that_is_not_between_a_sunrise_and_a_sunset() {
        let mut f = PointForecast::synthetic(vec![0, HOUR], vec![None, None]);
        assert!(!f.is_night(0), "without sun times nothing is shaded");
        f.sun = vec![SunDay { sunrise_ms: 6 * HOUR, sunset_ms: 20 * HOUR }];
        assert!(f.is_night(3 * HOUR));
        assert!(!f.is_night(6 * HOUR));
        assert!(!f.is_night(19 * HOUR));
        assert!(f.is_night(20 * HOUR));
    }

    #[test]
    fn a_position_reads_as_a_chart_writes_it() {
        assert_eq!(place_label(55.5, -12.25), "55°30.0'N 12°15.0'W");
    }
}
