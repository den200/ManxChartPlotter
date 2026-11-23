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
//!   and sunset — the last so the sheet can shade the night. Asked of every
//!   model in [`MODELS`] at once, not of one: no model wins consistently, and
//!   what a skipper can actually plan on is whether they *agree*. The lanes
//!   draw the median, the band around it is every model's answer, and the
//!   spread of the middle of the pack becomes the hour's [`Confidence`];
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

/// What to call the source until a forecast arrives and can name its models;
/// after that, [`PointForecast::source_label`].
pub const POINT_SOURCE_PENDING: &str = "Open-Meteo — multi-model, marine SMOC";

/// The models asked for: Open-Meteo's id, and the name the sheet prints.
///
/// They do not all reach as far: Météo-France stops around day five and the
/// Met Office around day seven, so the count of models behind an hour falls
/// with the horizon, and the confidence with it. AIFS has no gusts.
const MODELS: [(&str, &str); 7] = [
    ("ecmwf_ifs025", "ECMWF"),
    ("ecmwf_aifs025_single", "AIFS"),
    ("icon_seamless", "ICON"),
    ("gfs_seamless", "GFS"),
    ("dmi_seamless", "DMI"),
    ("meteofrance_seamless", "Météo-France"),
    ("ukmo_seamless", "UKMO"),
];

/// Fewer models than this and agreement means nothing: two that agree may
/// share a parent, and two that differ cannot say which one is the outlier.
const MIN_MODELS_FOR_CONFIDENCE: u8 = 3;

/// How far the models agree on an hour's wind.
///
/// Measured on the middle of the pack — the highest and lowest model are left
/// out once there are five — because one coastal model with a different land
/// mask otherwise paints every hour red. Calibrated on live answers for the
/// Sound, the Kattegat, the Channel and the Gulf of Lion: the open Kattegat
/// came out good for three days and worse after, the Mistral poor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    Good,
    Fair,
    Poor,
}

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
    /// The lightest and strongest mean wind any model gave.
    pub wind_lo_kt: Option<f32>,
    pub wind_hi_kt: Option<f32>,
    /// The strongest gust any model gave.
    pub gust_hi_kt: Option<f32>,
    /// Spread of the middle of the pack: knots, and the arc in degrees that
    /// holds their directions.
    pub wind_spread_kt: Option<f32>,
    pub dir_spread_deg: Option<f32>,
    /// How many models answered for this hour.
    pub models: Option<u8>,
}

impl Sample {
    /// Green, amber or red for this instant; `None` where too few models
    /// answered to say.
    pub fn confidence(&self) -> Option<Confidence> {
        if self.models? < MIN_MODELS_FOR_CONFIDENCE {
            return None;
        }
        let median = self.wind_kt?;
        // Relative to the wind, floored at ten knots: three knots apart
        // matters at ten and does not at thirty, and in a near-calm every
        // model is "a hundred percent" off.
        let rel = self.wind_spread_kt? / median.max(10.0);
        // In light air the direction is noise, and to a sailor it barely
        // matters which way four knots comes from.
        let dir = if median >= 6.0 {
            self.dir_spread_deg.unwrap_or(0.0)
        } else {
            0.0
        };
        Some(if rel > 0.6 || dir > 90.0 {
            Confidence::Poor
        } else if rel > 0.3 || dir > 45.0 {
            Confidence::Fair
        } else {
            Confidence::Good
        })
    }
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
    /// The median of the models, as are gust and rain. Wind is the level at
    /// the stamp; gust and rain are the maximum and the sum of the hour that
    /// *ends* at it, which is how Open-Meteo reports them.
    pub wind_kt: Vec<Option<f32>>,
    pub gust_kt: Vec<Option<f32>>,
    /// The mean of the models' directions, taken as unit vectors (§8.5).
    pub wind_from_deg: Vec<Option<f32>>,
    pub rain_mm: Vec<Option<f32>>,
    /// Every model's answer, as a band: lightest and strongest mean wind,
    /// strongest gust.
    pub wind_lo_kt: Vec<Option<f32>>,
    pub wind_hi_kt: Vec<Option<f32>>,
    pub gust_hi_kt: Vec<Option<f32>>,
    /// See [`Sample::wind_spread_kt`].
    pub wind_spread_kt: Vec<Option<f32>>,
    pub dir_spread_deg: Vec<Option<f32>>,
    /// Models behind each hour; zero where none.
    pub model_count: Vec<u8>,
    /// The models that answered at all, by the names in [`MODELS`].
    pub models: Vec<&'static str>,
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

        let models = MODELS.map(|(id, _)| id).join(",");
        let air = get(format!(
            "https://api.open-meteo.com/v1/forecast?latitude={lat:.4}&longitude={lon:.4}\
             &hourly=wind_speed_10m,wind_direction_10m,wind_gusts_10m,precipitation\
             &daily=sunrise,sunset&wind_speed_unit=kn&timeformat=unixtime&timezone=UTC\
             &forecast_days={days}&models={models}"
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
            "pointfx: {:.3},{:.3} — {} hours, models {}, sea {}",
            lat,
            lon,
            forecast.times.len(),
            forecast.models.join("/"),
            if forecast.has_sea { "yes" } else { "no" }
        );
        Ok(forecast)
    }

    /// What to call this forecast's source: the models that actually
    /// answered, not the ones that were asked.
    pub fn source_label(&self) -> String {
        match self.models.as_slice() {
            [] => POINT_SOURCE_PENDING.into(),
            [one] => format!("Open-Meteo — {one} only, marine SMOC"),
            all => format!(
                "Open-Meteo — median of {} models ({}), marine SMOC",
                all.len(),
                all.join(", ")
            ),
        }
    }

    /// Parse the forecast API's answer. Split out as the test seam.
    ///
    /// Asked for several models, Open-Meteo suffixes every key with the
    /// model's id — `wind_speed_10m_gfs_seamless`, `sunrise_gfs_seamless`.
    /// A model with no wind anywhere in the answer did not answer, and is not
    /// counted.
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

        struct Member {
            id: &'static str,
            name: &'static str,
            wind: Vec<Option<f32>>,
            gust: Vec<Option<f32>>,
            dir: Vec<Option<f32>>,
            rain: Vec<Option<f32>>,
        }
        let members: Vec<Member> = MODELS
            .iter()
            .filter_map(|&(id, name)| {
                let wind = series(hourly, &format!("wind_speed_10m_{id}"), n);
                wind.iter().any(Option::is_some).then(|| Member {
                    id,
                    name,
                    wind,
                    gust: series(hourly, &format!("wind_gusts_10m_{id}"), n),
                    dir: series(hourly, &format!("wind_direction_10m_{id}"), n),
                    rain: series(hourly, &format!("precipitation_{id}"), n),
                })
            })
            .collect();
        if members.is_empty() {
            return Err("forecast answer has wind from no model".into());
        }

        let mut f = Self {
            lat,
            lon,
            wind_kt: vec![None; n],
            gust_kt: vec![None; n],
            wind_from_deg: vec![None; n],
            rain_mm: vec![None; n],
            wind_lo_kt: vec![None; n],
            wind_hi_kt: vec![None; n],
            gust_hi_kt: vec![None; n],
            wind_spread_kt: vec![None; n],
            dir_spread_deg: vec![None; n],
            model_count: vec![0; n],
            models: members.iter().map(|m| m.name).collect(),
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
        let column = |pick: fn(&Member) -> &Vec<Option<f32>>, i: usize| -> Vec<f32> {
            members.iter().filter_map(|m| pick(m)[i]).collect()
        };
        for i in 0..n {
            let mut wind = column(|m| &m.wind, i);
            let mut gust = column(|m| &m.gust, i);
            let mut rain = column(|m| &m.rain, i);
            let dir = column(|m| &m.dir, i);
            f.model_count[i] = wind.len() as u8;
            f.wind_lo_kt[i] = wind.iter().copied().reduce(f32::min);
            f.wind_hi_kt[i] = wind.iter().copied().reduce(f32::max);
            f.gust_hi_kt[i] = gust.iter().copied().reduce(f32::max);
            f.wind_kt[i] = median(&mut wind);
            f.gust_kt[i] = median(&mut gust);
            f.rain_mm[i] = median(&mut rain);
            f.wind_from_deg[i] = mean_bearing(&dir);
            if wind.len() >= 2 {
                f.wind_spread_kt[i] = Some(trimmed_range(&mut wind));
            }
            if dir.len() >= 2 {
                f.dir_spread_deg[i] = Some(trimmed_arc(&dir));
            }
        }

        if let Some(daily) = root.get("daily") {
            // Every model sends the same sun; take the first that sent any.
            let (rise, set) = members
                .iter()
                .map(|m| {
                    (
                        int_series(daily, &format!("sunrise_{}", m.id)),
                        int_series(daily, &format!("sunset_{}", m.id)),
                    )
                })
                .find(|(r, _)| r.iter().any(Option::is_some))
                .unwrap_or_default();
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
        // Rain and gusts are not levels but the sum and the maximum of an
        // hour, and Open-Meteo stamps each with the hour's *end*: the value
        // at 13:00 fell between 12:00 and 13:00. The honest reading at any
        // instant is the hour it falls in, whole — the later stamp.
        let hour_of = |s: &[Option<f32>]| s.get(i1).copied().flatten();
        Sample {
            wind_kt: lerp(&self.wind_kt),
            gust_kt: hour_of(&self.gust_kt),
            wind_from_deg: lerp_dir(&self.wind_from_deg, i0, i1, f),
            rain_mm: hour_of(&self.rain_mm),
            wave_m: lerp(&self.wave_m),
            wave_from_deg: lerp_dir(&self.wave_from_deg, i0, i1, f),
            wave_period_s: lerp(&self.wave_period_s),
            current_kt: lerp(&self.current_kt),
            current_to_deg: lerp_dir(&self.current_to_deg, i0, i1, f),
            tide_m: lerp(&self.tide_m),
            wind_lo_kt: lerp(&self.wind_lo_kt),
            wind_hi_kt: lerp(&self.wind_hi_kt),
            gust_hi_kt: hour_of(&self.gust_hi_kt),
            wind_spread_kt: lerp(&self.wind_spread_kt),
            dir_spread_deg: lerp(&self.dir_spread_deg),
            // A count does not interpolate; the nearer hour's stands.
            models: self
                .model_count
                .get(if f < 0.5 { i0 } else { i1 })
                .copied()
                .filter(|&c| c > 0),
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
        // A turn is a run of equal samples — one sample, or a stand — with
        // water lower on both sides (high) or higher on both sides (low). A
        // stand on the flood or the ebb has water rising on one side or
        // falling on the other, so it is no turn at all.
        let n = self.times.len();
        let mut first = 1;
        while first < n - 1 {
            let Some(v) = self.tide_m[first] else {
                first += 1;
                continue;
            };
            let mut last = first;
            while last + 1 < n && self.tide_m[last + 1] == Some(v) {
                last += 1;
            }
            let next = self.tide_m.get(last + 1).copied().flatten();
            if let (Some(before), Some(after)) = (self.tide_m[first - 1], next) {
                let high = before < v && after < v;
                let low = before > v && after > v;
                if high || low {
                    // Vertex of the parabola through (−1,before), (0,v),
                    // (1,level) in samples, about the run's first sample. A
                    // stand is flat, so its second point is the stand itself.
                    let level = if last == first { after } else { v };
                    let denom = before - 2.0 * v + level;
                    let shift = if denom.abs() < 1e-6 {
                        0.0
                    } else {
                        (0.5 * (before - level) / denom).clamp(-0.5, 0.5)
                    };
                    let height = v - 0.25 * (before - level) * shift;
                    let time_ms = if last == first {
                        let step = (self.times[first + 1] - self.times[first - 1]) as f64 / 2.0;
                        self.times[first] + (shift as f64 * step) as i64
                    } else {
                        // A stand's turn is the middle of the stand.
                        (self.times[first] + self.times[last]) / 2
                    };
                    out.push(TideExtreme { time_ms, height_m: height, high });
                }
            }
            first = last + 1;
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

    /// A forecast with only a tide in it; every other series is empty, which
    /// [`Self::at`] reads as nothing to say.
    #[cfg(test)]
    fn synthetic(times: Vec<i64>, tide: Vec<Option<f32>>) -> Self {
        Self {
            lat: 55.0,
            lon: 12.0,
            tide_m: tide,
            has_sea: true,
            times,
            ..Default::default()
        }
    }
}

/// A position written the way a chart writes it.
pub fn place_label(lat: f64, lon: f64) -> String {
    let one = |v: f64, pos: char, neg: char| {
        let hemi = if v >= 0.0 { pos } else { neg };
        // Rounded to the tenth of a minute that is printed *before* it is
        // split into degrees and minutes; split first, 55°59.99' rounds up
        // to "55°60.0'" rather than carrying into 56°.
        let tenths = (v.abs() * 600.0).round() as i64;
        let (deg, min) = (tenths / 600, (tenths % 600) as f64 / 10.0);
        // A plain apostrophe, not the typographic prime: egui's bundled
        // fonts have no U+2032 and it drew as an empty box.
        format!("{deg}°{min:04.1}'{hemi}")
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

/// The middle value; the mean of the middle two for an even count. Sorts in
/// place.
fn median(v: &mut [f32]) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f32::total_cmp);
    let m = v.len() / 2;
    Some(if v.len() % 2 == 0 {
        (v[m - 1] + v[m]) * 0.5
    } else {
        v[m]
    })
}

/// Strongest less lightest, once the outer model on each side is set aside —
/// which needs five to leave three. Sorts in place.
fn trimmed_range(v: &mut [f32]) -> f32 {
    v.sort_by(f32::total_cmp);
    let v = if v.len() >= 5 { &v[1..v.len() - 1] } else { v };
    match (v.first(), v.last()) {
        (Some(a), Some(b)) => b - a,
        _ => 0.0,
    }
}

/// The mean of several bearings, as unit vectors (§8.5). Bearings that cancel
/// out have no mean; the first is as good an answer as any.
fn mean_bearing(v: &[f32]) -> Option<f32> {
    let first = *v.first()?;
    let (x, y) = v.iter().fold((0.0f64, 0.0f64), |(x, y), d| {
        let r = (*d as f64).to_radians();
        (x + r.sin(), y + r.cos())
    });
    if x.abs() < 1e-9 && y.abs() < 1e-9 {
        return Some(first);
    }
    Some(x.atan2(y).to_degrees().rem_euclid(360.0) as f32)
}

/// The narrowest arc that holds every bearing, degrees: 350° and 10° are 20°
/// apart, not 340°. With five or more, the bearing furthest from the mean is
/// set aside first, as [`trimmed_range`] does for speed.
fn trimmed_arc(v: &[f32]) -> f32 {
    let mut a: Vec<f32> = v.iter().map(|d| d.rem_euclid(360.0)).collect();
    if a.len() >= 5 {
        if let Some(mean) = mean_bearing(&a) {
            // Angular distance from the mean, 0..=180.
            let off = |d: f32| {
                let x = (d - mean).rem_euclid(360.0);
                x.min(360.0 - x)
            };
            if let Some((worst, _)) = a
                .iter()
                .enumerate()
                .max_by(|x, y| off(*x.1).total_cmp(&off(*y.1)))
            {
                a.remove(worst);
            }
        }
    }
    if a.len() < 2 {
        return 0.0;
    }
    a.sort_by(f32::total_cmp);
    // The arc that holds them all is the circle less the widest empty gap —
    // the gap across north included, which is never zero: identical
    // bearings leave the whole circle empty, not none of it.
    let wrap = a[0] + 360.0 - a[a.len() - 1];
    let widest_gap = a.windows(2).map(|w| w[1] - w[0]).fold(wrap, f32::max);
    360.0 - widest_gap
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
        // Two hours from one model, unixtime seconds, in exactly the shape
        // Open-Meteo returns when asked for several: every key suffixed,
        // and a model that has nothing here sending nulls.
        r#"{"latitude":55.7,"longitude":12.6,
            "hourly":{"time":[1000000,1003600,1007200],
                      "wind_speed_10m_gfs_seamless":[10.0,20.0,null],
                      "wind_direction_10m_gfs_seamless":[350.0,10.0,null],
                      "wind_gusts_10m_gfs_seamless":[14.0,28.0,null],
                      "precipitation_gfs_seamless":[0.0,1.5,null],
                      "wind_speed_10m_ecmwf_aifs025_single":[null,null,null]},
            "daily":{"sunrise_ecmwf_aifs025_single":[null],"sunset_ecmwf_aifs025_single":[null],
                     "sunrise_gfs_seamless":[999000],"sunset_gfs_seamless":[1006000]}}"#
            .into()
    }

    /// Five models over two hours. In the first they agree but for one
    /// outlier; in the second they are all over the place. UKMO has run out
    /// by the second hour, as the far end of its horizon does.
    fn models_json() -> String {
        r#"{"hourly":{"time":[0,3600],
            "wind_speed_10m_ecmwf_ifs025":[12.0,10.0],
            "wind_direction_10m_ecmwf_ifs025":[355.0,90.0],
            "wind_gusts_10m_ecmwf_ifs025":[16.0,14.0],
            "precipitation_ecmwf_ifs025":[0.0,0.0],
            "wind_speed_10m_ecmwf_aifs025_single":[13.0,22.0],
            "wind_direction_10m_ecmwf_aifs025_single":[5.0,200.0],
            "wind_gusts_10m_ecmwf_aifs025_single":[null,null],
            "precipitation_ecmwf_aifs025_single":[0.0,0.0],
            "wind_speed_10m_icon_seamless":[12.0,4.0],
            "wind_direction_10m_icon_seamless":[0.0,300.0],
            "wind_gusts_10m_icon_seamless":[17.0,8.0],
            "precipitation_icon_seamless":[0.2,0.0],
            "wind_speed_10m_gfs_seamless":[14.0,16.0],
            "wind_direction_10m_gfs_seamless":[10.0,150.0],
            "wind_gusts_10m_gfs_seamless":[19.0,24.0],
            "precipitation_gfs_seamless":[0.4,0.0],
            "wind_speed_10m_dmi_seamless":[null,null],
            "wind_speed_10m_ukmo_seamless":[25.0,null],
            "wind_direction_10m_ukmo_seamless":[350.0,null],
            "wind_gusts_10m_ukmo_seamless":[33.0,null],
            "precipitation_ukmo_seamless":[0.0,null]}}"#
            .into()
    }

    #[test]
    fn the_lane_is_the_median_and_the_band_is_every_model() {
        let f = PointForecast::from_air_json(&models_json(), 0.0, 0.0).unwrap();
        // DMI sent nothing and does not count; the label names who answered.
        assert_eq!(f.models, vec!["ECMWF", "AIFS", "ICON", "GFS", "UKMO"]);
        assert_eq!(
            f.source_label(),
            "Open-Meteo — median of 5 models (ECMWF, AIFS, ICON, GFS, UKMO), marine SMOC"
        );
        assert_eq!(f.model_count, vec![5, 4]);
        // 12, 12, 13, 14, 25: the median ignores the outlier the band shows.
        assert_eq!(f.wind_kt[0], Some(13.0));
        assert_eq!(f.wind_lo_kt[0], Some(12.0));
        assert_eq!(f.wind_hi_kt[0], Some(25.0));
        // AIFS has no gusts: four gusts, so the middle two averaged.
        assert_eq!(f.gust_kt[0], Some(18.0));
        assert_eq!(f.gust_hi_kt[0], Some(33.0));
        assert_eq!(f.rain_mm[0], Some(0.0));
        // Second hour, four models: 4, 10, 16, 22.
        assert_eq!(f.wind_kt[1], Some(13.0));
    }

    #[test]
    fn the_spread_is_the_middle_of_the_pack_once_there_are_five() {
        let f = PointForecast::from_air_json(&models_json(), 0.0, 0.0).unwrap();
        // 12..14 once 25 (and one 12) are set aside.
        assert_eq!(f.wind_spread_kt[0], Some(2.0));
        // Four left in the second hour: nothing is trimmed.
        assert_eq!(f.wind_spread_kt[1], Some(18.0));
        // 350, 355, 0, 5, 10 across north: a 20° arc, not 340°. The trim
        // takes off one end, which leaves 15°.
        assert_eq!(f.dir_spread_deg[0], Some(15.0));
        let d = f.wind_from_deg[0].unwrap();
        assert!(d < 1.0 || d > 359.0, "mean bearing crossed the wrong way: {d}");
    }

    #[test]
    fn agreement_reads_good_and_disagreement_reads_poor() {
        let f = PointForecast::from_air_json(&models_json(), 0.0, 0.0).unwrap();
        assert_eq!(f.at(0).confidence(), Some(Confidence::Good));
        assert_eq!(f.at(3_600_000).confidence(), Some(Confidence::Poor));
        assert_eq!(f.at(0).models, Some(5));
    }

    #[test]
    fn confidence_thresholds() {
        let s = |kt: f32, spread: f32, dir: f32, models: u8| Sample {
            wind_kt: Some(kt),
            wind_spread_kt: Some(spread),
            dir_spread_deg: Some(dir),
            models: Some(models),
            ..Default::default()
        };
        assert_eq!(s(20.0, 6.0, 40.0, 6).confidence(), Some(Confidence::Good));
        assert_eq!(s(20.0, 7.0, 40.0, 6).confidence(), Some(Confidence::Fair));
        assert_eq!(s(20.0, 6.0, 50.0, 6).confidence(), Some(Confidence::Fair));
        assert_eq!(s(20.0, 13.0, 0.0, 6).confidence(), Some(Confidence::Poor));
        assert_eq!(s(20.0, 0.0, 120.0, 6).confidence(), Some(Confidence::Poor));
        // Below ten knots the floor holds: three knots apart is still good.
        assert_eq!(s(4.0, 3.0, 0.0, 6).confidence(), Some(Confidence::Good));
        // In light air the models may point anywhere.
        assert_eq!(s(4.0, 1.0, 180.0, 6).confidence(), Some(Confidence::Good));
        // …but from six knots it counts: the same spread is no longer noise.
        assert_eq!(s(6.0, 1.0, 180.0, 6).confidence(), Some(Confidence::Poor));
        // The edges themselves belong to the better colour: exactly 45° and
        // exactly 90° of spread are not yet worse.
        assert_eq!(s(20.0, 0.0, 45.0, 6).confidence(), Some(Confidence::Good));
        assert_eq!(s(20.0, 0.0, 90.0, 6).confidence(), Some(Confidence::Fair));
        // Two models cannot vouch for each other; three are the least that
        // can.
        assert_eq!(s(20.0, 0.0, 0.0, 2).confidence(), None);
        assert_eq!(s(20.0, 0.0, 0.0, 3).confidence(), Some(Confidence::Good));
        assert_eq!(Sample::default().confidence(), None);
    }

    #[test]
    fn the_arc_of_bearings_is_measured_the_short_way_round() {
        assert_eq!(trimmed_arc(&[350.0, 10.0]), 20.0);
        assert_eq!(trimmed_arc(&[90.0, 90.0, 90.0]), 0.0);
        assert_eq!(trimmed_arc(&[0.0, 120.0, 240.0]), 240.0);
        assert_eq!(trimmed_arc(&[45.0]), 0.0);
    }

    #[test]
    fn an_answer_without_any_model_is_refused() {
        let e = PointForecast::from_air_json(
            r#"{"hourly":{"time":[0],"wind_speed_10m_gfs_seamless":[null]}}"#,
            0.0,
            0.0,
        )
        .unwrap_err();
        assert!(e.contains("no model"), "{e}");
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
        assert_eq!(f.gust_kt[2], None);
        assert_eq!(f.model_count[2], 0);
        // Between the last real hour and the empty one the reading holds the
        // real hour's 20 kn. A null blended in as zero would read 10 here.
        assert_eq!(f.at(1_003_600_000 + HOUR / 2).wind_kt, Some(20.0));
        // And an hour nobody answered for has no model count to show.
        assert_eq!(f.at(1_007_200_000).models, None);
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
                          "ocean_current_direction":[350.0,10.0],
                          "sea_level_height_msl":[0.4,0.9]}}"#,
        )
        .unwrap();
        assert!(f.has_sea);
        assert_eq!(f.wave_m[0], None, "the unshared first hour must stay empty");
        assert_eq!(f.wave_m[1], Some(1.2));
        // 1.852 km/h is exactly one knot.
        assert!((f.current_kt[1].unwrap() - 1.0).abs() < 1e-3);
        assert!((f.current_kt[2].unwrap() - 2.0).abs() < 1e-3);
        // Directions go through untouched: the sea still comes FROM 200° and
        // the stream still sets TOWARDS 350°. Turning either round "to match"
        // the other is the from/to mistake.
        assert_eq!(f.wave_from_deg[1], Some(200.0));
        assert_eq!(f.current_to_deg[1], Some(350.0));
        assert_eq!(f.wave_period_s[2], Some(5.5));
        assert_eq!(f.tide_m[2], Some(0.9));
        // Half way between the hours the stream's set interpolates as a
        // bearing too: 350° and 010° meet at north, not at south.
        let mid = f.at(1_003_600_000 + HOUR / 2);
        let set = mid.current_to_deg.unwrap();
        assert!(set < 0.01 || set > 359.99, "set crossed the wrong way: {set}");
        assert!((mid.wave_from_deg.unwrap() - 205.0).abs() < 0.01);
        assert!((mid.tide_m.unwrap() - 0.65).abs() < 1e-6);
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
        assert!(ex[0].high);
        // The parabola through (1.0, 1.6, 1.5) peaks 5/14 of a step after the
        // middle sample — 2 h 21 min 26 s — at 1.6 + 0.125·5/14 m.
        let expected_ms = 2 * HOUR + (HOUR as f64 * 5.0 / 14.0) as i64;
        assert!(
            (ex[0].time_ms - expected_ms).abs() < 1_000,
            "high water at {} ms, expected {expected_ms}",
            ex[0].time_ms
        );
        assert!((ex[0].height_m - (1.6 + 0.125 * 5.0 / 14.0)).abs() < 1e-4, "{}", ex[0].height_m);
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
        // Each turn lies within half a step of the sample it was found at.
        assert!((ex[0].time_ms - HOUR).abs() <= HOUR / 2, "{}", ex[0].time_ms);
        assert!((ex[1].time_ms - 4 * HOUR).abs() <= HOUR / 2, "{}", ex[1].time_ms);
        assert!(ex[0].height_m >= 1.8 && ex[1].height_m <= -1.1);
        assert_eq!(f.tide_range(), Some((-1.1, 1.8)));
    }

    #[test]
    fn a_flat_series_has_no_turns_to_report() {
        let t: Vec<i64> = (0..5).map(|i| i as i64 * HOUR).collect();
        let f = PointForecast::synthetic(t, vec![Some(0.5); 5]);
        assert!(f.tide_extremes().is_empty());
    }

    /// Open-Meteo's precipitation is "the sum of the preceding hour" and its
    /// gust "the maximum of the preceding hour": the 1.5 mm and 28 kn stamped
    /// at the second hour happened during the first. Read from the hour that
    /// *starts* at the stamp, as the sheet once did, every shower and every
    /// squall arrived an hour late.
    #[test]
    fn rain_and_gusts_belong_to_the_hour_before_their_stamp() {
        let f = PointForecast::from_air_json(&air_json(), 55.7, 12.6).unwrap();
        let first_hour = 1_000_000_000 + HOUR / 2;
        assert_eq!(f.at(first_hour).rain_mm, Some(1.5));
        // The hour's maximum, whole — not a blend with the hour before.
        assert_eq!(f.at(first_hour).gust_kt, Some(28.0));
        // The stamp itself closes the hour it ends.
        assert_eq!(f.at(1_003_600_000).rain_mm, Some(1.5));
        // The second hour's total is the null stamped at the third.
        assert_eq!(f.at(1_003_600_000 + HOUR / 2).rain_mm, None);
        assert_eq!(f.at(1_003_600_000 + HOUR / 2).gust_kt, None);
        // The mean wind is a level at its stamp, and still interpolates.
        assert_eq!(f.at(first_hour).wind_kt, Some(15.0));
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
        assert_eq!(place_label(-33.875, 151.2), "33°52.5'S 151°12.0'E");
    }

    /// Minutes that round up to sixty must carry into the degree: a chart
    /// never writes 60.0'.
    #[test]
    fn minutes_that_round_to_sixty_carry_into_the_degree() {
        assert_eq!(place_label(55.99999, 12.0), "56°00.0'N 12°00.0'E");
    }

    /// `timeformat=unixtime` gives sunrise and sunset in seconds, exactly as
    /// it gives the hours; the sheet keeps milliseconds. The GFS day in
    /// `air_json` rises at 999 000 s and sets at 1 006 000 s.
    #[test]
    fn sunrise_and_sunset_arrive_in_seconds_and_are_kept_in_milliseconds() {
        let f = PointForecast::from_air_json(&air_json(), 55.7, 12.6).unwrap();
        assert_eq!(
            f.sun,
            vec![SunDay { sunrise_ms: 999_000_000, sunset_ms: 1_006_000_000 }]
        );
    }

    /// Every day the API sent is kept, in order, from the first model that
    /// sent any sun at all.
    #[test]
    fn every_day_of_sun_is_kept() {
        let f = PointForecast::from_air_json(
            r#"{"hourly":{"time":[0],"wind_speed_10m_ecmwf_ifs025":[5.0],
                          "wind_speed_10m_gfs_seamless":[6.0]},
                "daily":{"sunrise_ecmwf_ifs025":[null,null],"sunset_ecmwf_ifs025":[null,null],
                         "sunrise_gfs_seamless":[21600,108000],
                         "sunset_gfs_seamless":[72000,158400]}}"#,
            0.0,
            0.0,
        )
        .unwrap();
        assert_eq!(
            f.sun,
            vec![
                SunDay { sunrise_ms: 6 * HOUR, sunset_ms: 20 * HOUR },
                SunDay { sunrise_ms: 30 * HOUR, sunset_ms: 44 * HOUR },
            ]
        );
    }

    /// Sixty percent apart is the edge of fair, not yet poor: the edges
    /// belong to the better colour, as the 45° and 90° ones do in
    /// `confidence_thresholds`. 12 kn of spread at 20 kn is exactly 60 %.
    #[test]
    fn a_spread_of_exactly_sixty_percent_is_still_fair() {
        let s = Sample {
            wind_kt: Some(20.0),
            wind_spread_kt: Some(12.0),
            dir_spread_deg: Some(0.0),
            models: Some(5),
            ..Default::default()
        };
        assert_eq!(s.confidence(), Some(Confidence::Fair));
    }

    /// A count does not interpolate: the nearer hour's stands. Exactly half
    /// way neither is nearer, and the tie goes to the later hour, as
    /// rounding half up does — the hour whose gusts and rain are already
    /// the ones read in between.
    #[test]
    fn the_model_count_is_the_nearer_hours_and_half_way_goes_to_the_later() {
        let f = PointForecast::from_air_json(&models_json(), 0.0, 0.0).unwrap();
        assert_eq!(f.model_count, vec![5, 4]);
        assert_eq!(f.at(HOUR / 2 - 1).models, Some(5));
        assert_eq!(f.at(HOUR / 2).models, Some(4));
        assert_eq!(f.at(HOUR / 2 + 1).models, Some(4));
    }

    fn hours(n: i64) -> Vec<i64> {
        (0..n).map(|i| i * HOUR).collect()
    }

    /// Three samples are the least a turn can be seen in, and enough: the
    /// parabola through 0.2, 1.0, 0.2 peaks on the middle one at 1.0 m.
    #[test]
    fn three_hours_are_enough_to_find_a_turn() {
        let f = PointForecast::synthetic(hours(3), vec![Some(0.2), Some(1.0), Some(0.2)]);
        assert_eq!(
            f.tide_extremes(),
            vec![TideExtreme { time_ms: HOUR, height_m: 1.0, high: true }]
        );
        let f = PointForecast::synthetic(hours(3), vec![Some(0.2), Some(-0.6), Some(0.2)]);
        assert_eq!(
            f.tide_extremes(),
            vec![TideExtreme { time_ms: HOUR, height_m: -0.6, high: false }]
        );
    }

    /// The guard in `tide_extremes` exists so that a hand-made forecast
    /// cannot turn a display bug into a panic at sea: too few hours, or a
    /// tide series shorter than the hours, reads as no turns.
    #[test]
    fn too_few_hours_or_a_short_tide_series_has_no_turns_and_does_not_panic() {
        assert!(PointForecast::default().tide_extremes().is_empty());
        let two = PointForecast::synthetic(hours(2), vec![Some(0.0), Some(1.0)]);
        assert!(two.tide_extremes().is_empty());
        let short = PointForecast::synthetic(hours(5), vec![Some(0.0), Some(1.0), Some(0.0)]);
        assert!(short.tide_extremes().is_empty());
    }

    /// Sea level comes rounded to the centimetre, and in the Baltic's tides
    /// of a few centimetres two hours in a row at high water often read the
    /// same. That is still a high water. 0, 1, 1, 0 is symmetric about
    /// 1.5 h, and the one parabola through all four samples,
    /// 1.125 − 0.5·(t − 1.5)², tops out there at 1.125 m.
    #[test]
    fn a_flat_topped_high_water_is_still_found() {
        let f = PointForecast::synthetic(hours(4), vec![Some(0.0), Some(1.0), Some(1.0), Some(0.0)]);
        let ex = f.tide_extremes();
        assert!(ex.iter().any(|e| e.high), "{ex:?}");
        for e in &ex {
            assert!(e.high, "{ex:?}");
            assert_eq!(e.time_ms, HOUR + HOUR / 2, "{ex:?}");
            assert!((e.height_m - 1.125).abs() < 1e-6, "{ex:?}");
        }
    }

    /// The same at low water: 1, 0, 0, 1 bottoms out at 1.5 h, −0.125 m.
    #[test]
    fn a_flat_bottomed_low_water_is_still_found() {
        let f = PointForecast::synthetic(hours(4), vec![Some(1.0), Some(0.0), Some(0.0), Some(1.0)]);
        let ex = f.tide_extremes();
        assert!(ex.iter().any(|e| !e.high), "{ex:?}");
        for e in &ex {
            assert!(!e.high, "{ex:?}");
            assert_eq!(e.time_ms, HOUR + HOUR / 2, "{ex:?}");
            assert!((e.height_m + 0.125).abs() < 1e-6, "{ex:?}");
        }
    }

    /// A stand of two equal hours at high water is one turn of the tide.
    #[test]
    fn a_stand_at_high_or_low_water_is_one_turn_not_two() {
        let high = PointForecast::synthetic(hours(4), vec![Some(0.0), Some(1.0), Some(1.0), Some(0.0)]);
        assert_eq!(high.tide_extremes().len(), 1, "{:?}", high.tide_extremes());
        let low = PointForecast::synthetic(hours(4), vec![Some(1.0), Some(0.0), Some(0.0), Some(1.0)]);
        assert_eq!(low.tide_extremes().len(), 1, "{:?}", low.tide_extremes());
    }

    /// Water that pauses on its way up and then rises again has not turned:
    /// a turn is the moment it stops rising *and starts falling*.
    #[test]
    fn a_stand_on_the_flood_or_the_ebb_is_no_turn_at_all() {
        let flood = PointForecast::synthetic(hours(4), vec![Some(0.0), Some(1.0), Some(1.0), Some(2.0)]);
        assert!(flood.tide_extremes().is_empty(), "{:?}", flood.tide_extremes());
        let ebb = PointForecast::synthetic(hours(4), vec![Some(2.0), Some(1.0), Some(1.0), Some(0.0)]);
        assert!(ebb.tide_extremes().is_empty(), "{:?}", ebb.tide_extremes());
    }

    /// A series that opens on a level stretch has no turn at its start:
    /// nothing before it says whether that stretch is a top or a bottom. A
    /// stand that is the top of a turn is found; one that merely begins the
    /// series is not.
    #[test]
    fn a_series_that_opens_level_has_no_turn_at_its_start() {
        let high_start = PointForecast::synthetic(hours(4), vec![Some(1.0), Some(1.0), Some(0.0), Some(0.0)]);
        assert!(high_start.tide_extremes().is_empty(), "{:?}", high_start.tide_extremes());
        let low_start = PointForecast::synthetic(hours(4), vec![Some(0.0), Some(0.0), Some(1.0), Some(1.0)]);
        assert!(low_start.tide_extremes().is_empty(), "{:?}", low_start.tide_extremes());
    }

    /// A missing sample is no level, so it hides no turn and makes none: the
    /// turns either side of the gap are both found, at their own hours.
    #[test]
    fn a_missing_sample_is_skipped_and_the_turns_either_side_are_kept() {
        let f = PointForecast::synthetic(
            hours(7),
            vec![Some(0.0), Some(1.0), Some(0.0), None, Some(0.0), Some(1.0), Some(0.0)],
        );
        let ex = f.tide_extremes();
        let times: Vec<i64> = ex.iter().map(|e| e.time_ms).collect();
        assert_eq!(times, vec![HOUR, 5 * HOUR], "{ex:?}");
        assert!(ex.iter().all(|e| e.high), "{ex:?}");
    }

    /// Below a micrometre the three samples say nothing about where the top
    /// lies, so the turn keeps its sample's hour rather than a fitted vertex
    /// that the noise invented.
    #[test]
    fn a_turn_too_flat_to_fit_a_parabola_sits_on_its_sample() {
        let f = PointForecast::synthetic(hours(4), vec![Some(0.0), Some(4e-7), Some(2e-7), Some(0.0)]);
        let ex = f.tide_extremes();
        assert_eq!(ex.len(), 1, "{ex:?}");
        assert_eq!(ex[0].time_ms, HOUR, "{ex:?}");
    }

    /// The range scales the tide lane, and a lane cannot be scaled to
    /// infinity. A sea level past f32's range (1e39 in the JSON) arrives as
    /// infinity; whatever comes back must be finite at both ends.
    #[test]
    fn a_tide_range_is_finite_or_nothing() {
        let f = PointForecast::synthetic(hours(3), vec![Some(0.5), Some(f32::INFINITY), Some(-0.3)]);
        let r = f.tide_range();
        assert!(r.is_none_or(|(lo, hi)| lo.is_finite() && hi.is_finite()), "{r:?}");
        let empty = PointForecast::synthetic(hours(2), vec![None, None]);
        assert_eq!(empty.tide_range(), None);
    }

    #[test]
    fn a_lane_peaks_at_its_largest_value_and_an_empty_lane_has_none() {
        assert_eq!(PointForecast::peak(&[Some(3.0), None, Some(7.5), Some(-2.0)]), Some(7.5));
        assert_eq!(PointForecast::peak(&[Some(-4.0), Some(-1.5)]), Some(-1.5));
        assert_eq!(PointForecast::peak(&[None, None]), None);
        assert_eq!(PointForecast::peak(&[]), None);
    }

    fn assert_bearing(got: Option<f32>, want: f32) {
        let got = got.expect("a bearing");
        let off = ((got - want + 540.0).rem_euclid(360.0) - 180.0).abs();
        assert!(off < 1e-3, "{got} is not {want}");
    }

    /// The mean of bearings is the direction of the sum of their unit
    /// vectors (§8.5): 30° and 90° point at 60°, 80° and 100° due east, and
    /// three bearings 20° apart at the middle one.
    #[test]
    fn the_mean_of_bearings_is_taken_as_vectors() {
        assert_bearing(mean_bearing(&[30.0, 90.0]), 60.0);
        assert_bearing(mean_bearing(&[80.0, 100.0]), 90.0);
        assert_bearing(mean_bearing(&[200.0, 220.0, 240.0]), 220.0);
        assert_bearing(mean_bearing(&[350.0, 10.0]), 0.0);
        assert_eq!(mean_bearing(&[]), None);
    }

    /// "Bearings that cancel out have no mean; the first is as good an
    /// answer as any" — and certainly better than a direction made of
    /// rounding noise that none of the models gave.
    #[test]
    fn bearings_that_cancel_have_no_mean_and_the_first_stands() {
        assert_eq!(mean_bearing(&[90.0, 270.0]), Some(90.0));
        assert_eq!(mean_bearing(&[10.0, 190.0]), Some(10.0));
        assert_eq!(mean_bearing(&[10.0, 130.0, 250.0]), Some(10.0));
    }

    /// Between two hours a bearing moves through its unit vector: half way
    /// from 80° to 100° is 90°, from 30° to 90° is 60°.
    #[test]
    fn a_bearing_interpolates_through_its_vector() {
        assert_bearing(lerp_dir(&[Some(80.0), Some(100.0)], 0, 1, 0.5), 90.0);
        assert_bearing(lerp_dir(&[Some(30.0), Some(90.0)], 0, 1, 0.5), 60.0);
        // A quarter of the way along the chord from 350° to 10°: north
        // component cos 10°, east component −sin 10°·(0.75 − 0.25), so
        // 360° − atan(½·tan 10°) ≈ 354.96°, a shade short of 355°.
        let quarter = 360.0 - (0.5 * 10f64.to_radians().tan()).atan().to_degrees();
        assert_bearing(lerp_dir(&[Some(350.0), Some(10.0)], 0, 1, 0.25), quarter as f32);
        // One hour missing: the other stands, untouched.
        assert_eq!(lerp_dir(&[Some(40.0), None], 0, 1, 0.3), Some(40.0));
        assert_eq!(lerp_dir(&[None, Some(220.0)], 0, 1, 0.3), Some(220.0));
        assert_eq!(lerp_dir(&[None, None], 0, 1, 0.3), None);
    }

    /// Half way between opposite bearings the vector vanishes. Whatever is
    /// read there must be one of the two hours' own bearings, not some third
    /// direction that neither hour forecast.
    #[test]
    fn half_way_between_opposite_bearings_one_of_them_stands() {
        for (a, b) in [(90.0, 270.0), (10.0, 190.0), (0.0, 180.0)] {
            let r = lerp_dir(&[Some(a), Some(b)], 0, 1, 0.5).unwrap();
            assert!(r == a || r == b, "{a}..{b} read {r}");
        }
    }

    /// With five or more, the bearing furthest from the mean is the one set
    /// aside — whichever side of the pack it lies on, in whatever order the
    /// models answered. Four within 6° and one far off leave 6°.
    #[test]
    fn the_bearing_furthest_from_the_mean_is_the_one_set_aside() {
        assert_eq!(trimmed_arc(&[100.0, 102.0, 104.0, 106.0, 200.0]), 6.0);
        assert_eq!(trimmed_arc(&[20.0, 100.0, 102.0, 104.0, 106.0]), 6.0);
        assert_eq!(trimmed_arc(&[104.0, 20.0, 106.0, 100.0, 102.0]), 6.0);
        // Across north: 350..2 and an outlier at 90.
        assert_eq!(trimmed_arc(&[350.0, 354.0, 358.0, 2.0, 90.0]), 12.0);
    }

    /// "Bracket an ascending axis, clamped": past the last hour the bracket
    /// is the last pair, wholly at its far end.
    #[test]
    fn past_the_last_hour_the_bracket_clamps_to_the_last_pair() {
        assert_eq!(bracket(&[0, 10, 20, 30, 40], 55), (3, 4, 1.0));
        assert_eq!(bracket(&[0, 10], 25), (0, 1, 1.0));
        assert_eq!(bracket(&[0, 10, 20, 30, 40], 25), (2, 3, 0.5));
        assert_eq!(bracket(&[0, 10, 20, 30, 40], 0), (0, 1, 0.0));
        assert_eq!(bracket(&[7], 3), (0, 0, 0.0));
    }

    /// 2026-01-15 12:00 UTC, a Thursday. Every time zone on Earth (UTC−12
    /// to UTC+14) puts that instant on Thursday the 15th or Friday the 16th,
    /// so the test holds wherever it runs.
    const NOON_15_JAN_2026_MS: i64 = 1_768_478_400_000;

    fn minutes_of(hhmm: &str) -> i64 {
        let b = hhmm.as_bytes();
        assert!(
            b.len() == 5 && b[2] == b':' && [0, 1, 3, 4].iter().all(|&i| b[i].is_ascii_digit()),
            "not HH:MM: {hhmm:?}"
        );
        let (h, m): (i64, i64) = (hhmm[..2].parse().unwrap(), hhmm[3..].parse().unwrap());
        assert!(h < 24 && m < 60, "{hhmm:?}");
        h * 60 + m
    }

    #[test]
    fn a_time_reads_as_hours_and_minutes_and_moves_with_the_clock() {
        let t = NOON_15_JAN_2026_MS;
        let now = minutes_of(&local_hhmm(t));
        // Half an hour later reads half an hour later, in any zone.
        let later = minutes_of(&local_hhmm(t + HOUR / 2));
        assert_eq!((later - now).rem_euclid(24 * 60), 30);
        let later = minutes_of(&local_hhmm(t + 7 * 60_000));
        assert_eq!((later - now).rem_euclid(24 * 60), 7);
    }

    #[test]
    fn a_day_reads_as_weekday_and_date() {
        let t = NOON_15_JAN_2026_MS;
        let today = local_day(t);
        assert!(["Thu 15", "Fri 16"].contains(&today.as_str()), "{today:?}");
        // A day later is the next day, whichever zone this runs in.
        let tomorrow = local_day(t + 24 * HOUR);
        let want = if today == "Thu 15" { "Fri 16" } else { "Sat 17" };
        assert_eq!(tomorrow, want);
    }

    use proptest::prelude::*;

    /// One hour's JSON with model `ids[k]` giving `answers[k]`.
    fn one_hour_json(answers: &[(f32, f32)], ids: &[usize]) -> String {
        let mut s = String::from(r#"{"hourly":{"time":[0]"#);
        for (k, &(w, d)) in answers.iter().enumerate() {
            let id = MODELS[ids[k]].0;
            s += &format!(r#","wind_speed_10m_{id}":[{w}],"wind_direction_10m_{id}":[{d}]"#);
        }
        s + "}}"
    }

    fn angle_between(a: f32, b: f32) -> f32 {
        ((a - b + 540.0).rem_euclid(360.0) - 180.0).abs()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Which model said what is not part of the forecast: the median,
        /// the band and the spreads are the same whichever model carries
        /// which answer.
        #[test]
        fn the_summary_does_not_depend_on_which_model_said_what(
            answers in prop::collection::vec((0.0f32..60.0, 0.0f32..360.0), 5),
            order in Just((0..5).collect::<Vec<usize>>()).prop_shuffle(),
        ) {
            let a = PointForecast::from_air_json(&one_hour_json(&answers, &[0, 1, 2, 3, 4]), 0.0, 0.0).unwrap();
            let b = PointForecast::from_air_json(&one_hour_json(&answers, &order), 0.0, 0.0).unwrap();
            prop_assert_eq!(a.model_count[0], 5);
            prop_assert_eq!(a.wind_kt[0], b.wind_kt[0]);
            prop_assert_eq!(a.wind_lo_kt[0], b.wind_lo_kt[0]);
            prop_assert_eq!(a.wind_hi_kt[0], b.wind_hi_kt[0]);
            prop_assert_eq!(a.wind_spread_kt[0], b.wind_spread_kt[0]);
            let (da, db) = (a.dir_spread_deg[0].unwrap(), b.dir_spread_deg[0].unwrap());
            prop_assert!((da - db).abs() < 1e-2, "{} vs {}", da, db);
        }

        /// The median is the middle of the sorted answers, whatever order
        /// they came in, and the trimmed range the same.
        #[test]
        fn median_and_trimmed_range_ignore_the_order(
            (v, shuffled) in prop::collection::vec(0.0f32..60.0, 1..8)
                .prop_flat_map(|v| (Just(v.clone()), Just(v).prop_shuffle())),
        ) {
            let mut sorted = v.clone();
            sorted.sort_by(f32::total_cmp);
            let (mut a, mut b) = (v.clone(), shuffled.clone());
            let m = median(&mut a).unwrap();
            prop_assert_eq!(Some(m), median(&mut b));
            prop_assert!(sorted[0] <= m && m <= sorted[sorted.len() - 1]);
            // As many answers at or below the median as at or above it.
            let below = v.iter().filter(|&&x| x <= m).count();
            let above = v.iter().filter(|&&x| x >= m).count();
            prop_assert!(2 * below >= v.len() && 2 * above >= v.len());
            let (mut a, mut b) = (v.clone(), shuffled);
            prop_assert_eq!(trimmed_range(&mut a), trimmed_range(&mut b));
        }

        /// Turning every bearing by the same angle turns their mean by that
        /// angle and leaves the arc that holds them alone (§8.5: nothing
        /// depends on where north happens to be).
        #[test]
        fn turning_every_bearing_turns_the_mean_and_keeps_the_arc(
            v in prop::collection::vec(0.0f32..360.0, 2..8),
            turn in 0.0f32..360.0,
        ) {
            let turned: Vec<f32> = v.iter().map(|d| (d + turn).rem_euclid(360.0)).collect();
            let (a, b) = (trimmed_arc(&v), trimmed_arc(&turned));
            prop_assert!((a - b).abs() < 1e-2, "{} vs {}", a, b);
            // The mean only means something when the vectors do not nearly
            // cancel.
            let (x, y) = v.iter().fold((0.0f64, 0.0f64), |(x, y), d| {
                let r = (*d as f64).to_radians();
                (x + r.sin(), y + r.cos())
            });
            prop_assume!(x.hypot(y) > 0.5);
            let (ma, mb) = (mean_bearing(&v).unwrap(), mean_bearing(&turned).unwrap());
            prop_assert!(angle_between(ma + turn, mb) < 1e-2, "{} + {} vs {}", ma, turn, mb);
        }
    }
}
