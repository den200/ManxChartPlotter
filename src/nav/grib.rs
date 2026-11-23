//! Forecast wind: GRIB2 download, decode, and interpolation.
//!
//! The source is NOAA's NOMADS *filter* endpoint for GFS 0.25° — free, no
//! key, and it subsets server-side: Manx asks for exactly two fields
//! (10 m U and V) over exactly the passage box, so a forecast step is
//! kilobytes, not the 500 MB the full file would be. The source sits behind
//! an enum so ECMWF open data or DWD ICON can join without touching callers;
//! the UI shows which one is in use, per Den's ask.
//!
//! §8.5's correctness rule is enforced by construction: interpolation —
//! bilinear in space, linear in time — happens on the **U and V components**,
//! and direction is derived afterwards. Interpolating direction or speed
//! directly breaks at the 0/360 seam and averages vectors wrongly; there is
//! deliberately no code path that could.

use chrono::{DateTime, Duration, Timelike, Utc};

use super::angles::{deg_to_bam, Bam};
use super::isochrone::WindField;
use crate::render::projection::Projection;

const MS_TO_KNOTS: f64 = 3600.0 / 1852.0;

/// Where forecasts come from. One working source today; the enum and the UI
/// picker exist so the next one is an entry, not a refactor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GribSource {
    NoaaGfs025,
}

impl GribSource {
    pub fn label(self) -> &'static str {
        match self {
            GribSource::NoaaGfs025 => "NOAA GFS 0.25° (NOMADS)",
        }
    }
    /// Model cycle spacing in hours.
    fn cycle_hours(self) -> u32 {
        6
    }
    /// Forecast step spacing Manx requests.
    pub fn step_hours(self) -> u32 {
        3
    }
}

/// A lat/lon box, degrees. Longitudes in ±180.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeoBox {
    pub south: f64,
    pub north: f64,
    pub west: f64,
    pub east: f64,
}

impl GeoBox {
    pub fn around(points: &[crate::geo::LatLon], margin_deg: f64) -> Self {
        let mut b = Self {
            south: 90.0,
            north: -90.0,
            west: 180.0,
            east: -180.0,
        };
        for p in points {
            b.south = b.south.min(p.lat);
            b.north = b.north.max(p.lat);
            b.west = b.west.min(p.lon);
            b.east = b.east.max(p.lon);
        }
        b.south -= margin_deg;
        b.north += margin_deg;
        b.west -= margin_deg;
        b.east += margin_deg;
        b
    }
}

/// The NOMADS filter URL for one forecast step.
fn filter_url(source: GribSource, run: DateTime<Utc>, fh: u32, area: GeoBox) -> String {
    match source {
        GribSource::NoaaGfs025 => format!(
            "https://nomads.ncep.noaa.gov/cgi-bin/filter_gfs_0p25.pl?\
             dir=%2Fgfs.{date}%2F{cyc:02}%2Fatmos&file=gfs.t{cyc:02}z.pgrb2.0p25.f{fh:03}\
             &var_UGRD=on&var_VGRD=on&var_GUST=on&lev_10_m_above_ground=on&lev_surface=on\
             &subregion=&toplat={top}&bottomlat={bottom}&leftlon={left}&rightlon={right}",
            date = run.format("%Y%m%d"),
            cyc = run.hour(),
            fh = fh,
            top = area.north.ceil(),
            bottom = area.south.floor(),
            left = area.west.floor(),
            right = area.east.ceil(),
        ),
    }
}

/// Model runs worth trying, newest first. A cycle publishes ~4–5 h after its
/// nominal time, so the newest candidate starts 5 h back and older cycles
/// follow as fallbacks.
fn candidate_runs(source: GribSource, now: DateTime<Utc>) -> Vec<DateTime<Utc>> {
    let cycle = source.cycle_hours();
    let mut t = now - Duration::hours(5);
    t = t
        .with_minute(0)
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(t);
    let aligned_hour = (t.hour() / cycle) * cycle;
    t = t.with_hour(aligned_hour).unwrap_or(t);
    (0..4).map(|i| t - Duration::hours((cycle * i) as i64)).collect()
}

/// The last forecast hour to fetch so that the forecast reaches
/// `hours_ahead` past `now`.
///
/// Forecast hours count from the model run, and the newest run to be had is
/// already five to eleven hours old. Fetching `hours_ahead` hours of it
/// would stop that much short: a 24-hour request covering 13 to 19 hours
/// from now, and the passage's tail sailed on a frozen last field. Rounded
/// up to the model's step.
fn last_step_hours(hours_ahead: u32, run: DateTime<Utc>, now: DateTime<Utc>, step: u32) -> u32 {
    let age_min = (now - run).num_minutes().max(0) as u32;
    let wanted_min = hours_ahead * 60 + age_min;
    wanted_min.div_ceil(60).div_ceil(step.max(1)) * step.max(1)
}

/// One decoded forecast: a stack of (time, U-grid, V-grid) on one lat/lon
/// grid.
pub struct GribForecast {
    pub source: GribSource,
    pub run: DateTime<Utc>,
    /// Valid times, ms UTC, ascending.
    times: Vec<i64>,
    /// Grid axes as decoded (lats may descend; looked up, not assumed).
    lats: Vec<f64>,
    lons: Vec<f64>,
    /// Per time step, row-major `[lat][lon]`, m/s.
    u: Vec<Vec<f32>>,
    v: Vec<Vec<f32>>,
    /// Surface gust per step, m/s — `None` for a step whose file had none,
    /// so a forecast is never refused for lacking the one extra field.
    gust: Vec<Option<Vec<f32>>>,
}

#[derive(Debug)]
pub enum GribError {
    Download(String),
    Decode(String),
    /// Every candidate run failed — offline, or NOMADS is down.
    NoRunAvailable,
}

impl std::fmt::Display for GribError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GribError::Download(e) => write!(f, "forecast download failed: {e}"),
            GribError::Decode(e) => write!(f, "forecast file unreadable: {e}"),
            GribError::NoRunAvailable => {
                write!(f, "no forecast run reachable — check the connection")
            }
        }
    }
}

impl std::error::Error for GribError {}

impl GribForecast {
    /// Fetch a forecast covering `area` out to `hours_ahead`, caching the
    /// files under `cache_dir`. Blocking; run it on a worker.
    pub fn fetch(
        source: GribSource,
        area: GeoBox,
        hours_ahead: u32,
        cache_dir: &std::path::Path,
        mut progress: impl FnMut(String),
    ) -> Result<Self, GribError> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(60)))
            .build()
            .into();
        std::fs::create_dir_all(cache_dir).map_err(|e| GribError::Download(e.to_string()))?;
        sweep_cache(cache_dir, std::time::SystemTime::now());

        // Find a run that answers: try f000 of each candidate, newest first.
        let now = Utc::now();
        let mut run = None;
        for candidate in candidate_runs(source, now) {
            progress(format!("trying run {}", candidate.format("%Y-%m-%d %HZ")));
            match fetch_step(&agent, source, candidate, 0, area, cache_dir) {
                Ok(bytes) => {
                    run = Some((candidate, bytes));
                    break;
                }
                Err(e) => log::info!("grib: run {} unavailable: {e}", candidate.format("%d/%HZ")),
            }
        }
        let Some((run, first)) = run else {
            return Err(GribError::NoRunAvailable);
        };

        let mut forecast = Self {
            source,
            run,
            times: Vec::new(),
            lats: Vec::new(),
            lons: Vec::new(),
            u: Vec::new(),
            v: Vec::new(),
            gust: Vec::new(),
        };
        forecast.absorb(&first)?;

        let step = source.step_hours();
        let last = last_step_hours(hours_ahead, run, now, step);
        let mut fh = step;
        while fh <= last {
            if crate::nav::isochrone::this_plan_cancelled() {
                return Err(GribError::Download("cancelled".into()));
            }
            progress(format!("forecast +{fh:03}h"));
            let bytes = fetch_step(&agent, source, run, fh, area, cache_dir)?;
            forecast.absorb(&bytes)?;
            fh += step;
        }
        log::info!(
            "grib: {} run {} — {} steps, {}×{} grid",
            source.label(),
            run.format("%Y-%m-%d %HZ"),
            forecast.times.len(),
            forecast.lats.len(),
            forecast.lons.len()
        );
        Ok(forecast)
    }

    /// Decode one file's UGRD/VGRD pair, and its GUST if it has one, into
    /// the stack.
    fn absorb(&mut self, bytes: &[u8]) -> Result<(), GribError> {
        let mut u: Option<(i64, Vec<f32>)> = None;
        let mut v: Option<(i64, Vec<f32>)> = None;
        let mut gust: Option<(i64, Vec<f32>)> = None;
        for message in gribberish::message::read_messages(bytes) {
            let var = message
                .variable_abbrev()
                .map_err(|e| GribError::Decode(e.to_string()))?;
            if var != "UGRD" && var != "VGRD" && var != "GUST" {
                continue;
            }
            let when = message
                .forecast_date()
                .map_err(|e| GribError::Decode(e.to_string()))?
                .timestamp_millis();
            if self.lats.is_empty() {
                let (lats, lons) = message
                    .latlng_projector()
                    .map_err(|e| GribError::Decode(e.to_string()))?
                    .lat_lng();
                self.lats = lats;
                self.lons = lons.into_iter().map(normalize_lon).collect();
            }
            let data: Vec<f32> = message
                .data()
                .map_err(|e| GribError::Decode(e.to_string()))?
                .into_iter()
                .map(|x| x as f32)
                .collect();
            if data.len() != self.lats.len() * self.lons.len() {
                return Err(GribError::Decode(format!(
                    "grid mismatch: {} values for {}×{}",
                    data.len(),
                    self.lats.len(),
                    self.lons.len()
                )));
            }
            match var.as_str() {
                "UGRD" => u = Some((when, data)),
                "VGRD" => v = Some((when, data)),
                _ => gust = Some((when, data)),
            }
        }
        match (u, v) {
            (Some((tu, gu)), Some((tv, gv))) if tu == tv => {
                self.times.push(tu);
                self.u.push(gu);
                self.v.push(gv);
                self.gust.push(gust.filter(|(tg, _)| *tg == tu).map(|(_, g)| g));
                Ok(())
            }
            _ => Err(GribError::Decode("file lacks a matching UGRD/VGRD pair".into())),
        }
    }

    /// Build a forecast directly from grids — the test seam, and the reason
    /// the interpolation below is testable without a network.
    #[cfg(test)]
    pub fn synthetic(
        times: Vec<i64>,
        lats: Vec<f64>,
        lons: Vec<f64>,
        u: Vec<Vec<f32>>,
        v: Vec<Vec<f32>>,
    ) -> Self {
        Self {
            source: GribSource::NoaaGfs025,
            run: Utc::now(),
            gust: vec![None; times.len()],
            times,
            lats,
            lons,
            u,
            v,
        }
    }

    pub fn valid_span(&self) -> Option<(i64, i64)> {
        Some((*self.times.first()?, *self.times.last()?))
    }

    /// The valid times of the decoded steps, ascending — what a time control
    /// steps through.
    pub fn steps(&self) -> &[i64] {
        &self.times
    }

    /// Is this position inside the fetched box? The sampler clamps at the
    /// edges, which is right for a passage fetched *for* those endpoints and
    /// quite wrong for a display: clamping would paint the edge's wind
    /// across the whole ocean beyond it.
    pub fn covers(&self, lat: f64, lon: f64) -> bool {
        let lon = normalize_lon(lon);
        let within = |axis: &[f64], x: f64| match (
            axis.iter().cloned().fold(f64::INFINITY, f64::min),
            axis.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        ) {
            (lo, hi) if lo.is_finite() && hi.is_finite() => x >= lo && x <= hi,
            _ => false,
        };
        within(&self.lats, lat) && within(&self.lons, lon)
    }

    /// Wind at a position and time for display: `None` outside the box.
    pub fn sample(&self, lat: f64, lon: f64, time_ms: i64) -> Option<(f64, f64)> {
        if !self.covers(lat, lon) {
            return None;
        }
        let (u, v) = self.uv(lat, lon, time_ms);
        let kt = (u * u + v * v).sqrt() * MS_TO_KNOTS;
        let from_deg = (-u).atan2(-v).to_degrees().rem_euclid(360.0);
        Some((from_deg, kt))
    }

    /// Wind at a position and time with no coverage test: the edge's value
    /// held outward. For a display that fades the field out past its edge,
    /// which needs a colour to fade *from*.
    pub fn sample_held(&self, lat: f64, lon: f64, time_ms: i64) -> (f64, f64) {
        let (u, v) = self.uv(lat, lon, time_ms);
        let kt = (u * u + v * v).sqrt() * MS_TO_KNOTS;
        let from_deg = (-u).atan2(-v).to_degrees().rem_euclid(360.0);
        (from_deg, kt)
    }

    /// The gust in knots at a position and time, for display: `None` outside
    /// the box, or when either bracketing step came without a gust field —
    /// half a gust interpolated against nothing would be a made-up number.
    pub fn gust_kt(&self, lat: f64, lon: f64, time_ms: i64) -> Option<f64> {
        if !self.covers(lat, lon) {
            return None;
        }
        let lon = normalize_lon(lon);
        let (i0, i1, fy) = bracket_axis(&self.lats, lat);
        let (j0, j1, fx) = bracket_axis(&self.lons, lon);
        let (t0, t1, ft) = bracket_axis_i64(&self.times, time_ms);
        let (a, b) = (self.gust.get(t0)?.as_ref()?, self.gust.get(t1)?.as_ref()?);
        let w = self.lons.len();
        let sample = |grid: &Vec<f32>| -> f64 {
            let g = |i: usize, j: usize| grid[i * w + j] as f64;
            let top = g(i0, j0) + (g(i0, j1) - g(i0, j0)) * fx;
            let bot = g(i1, j0) + (g(i1, j1) - g(i1, j0)) * fx;
            top + (bot - top) * fy
        };
        let (ga, gb) = (sample(a), sample(b));
        Some((ga + (gb - ga) * ft) * MS_TO_KNOTS)
    }

    /// U/V in m/s at a position and time — bilinear in space, linear in
    /// time, clamped at the edges of the fetched box and span. Clamping is
    /// the honest option for a forecast fetched *for* this passage: any
    /// clamped lookup is edge noise, and NaN would poison the router.
    fn uv(&self, lat: f64, lon: f64, time_ms: i64) -> (f64, f64) {
        let lon = normalize_lon(lon);
        let (i0, i1, fy) = bracket_axis(&self.lats, lat);
        let (j0, j1, fx) = bracket_axis(&self.lons, lon);
        let (t0, t1, ft) = bracket_axis_i64(&self.times, time_ms);

        let sample = |grid: &Vec<f32>| -> f64 {
            let w = self.lons.len();
            let g = |i: usize, j: usize| grid[i * w + j] as f64;
            let top = g(i0, j0) + (g(i0, j1) - g(i0, j0)) * fx;
            let bot = g(i1, j0) + (g(i1, j1) - g(i1, j0)) * fx;
            top + (bot - top) * fy
        };
        let u = {
            let a = sample(&self.u[t0]);
            let b = sample(&self.u[t1]);
            a + (b - a) * ft
        };
        let v = {
            let a = sample(&self.v[t0]);
            let b = sample(&self.v[t1]);
            a + (b - a) * ft
        };
        (u, v)
    }
}

impl WindField for GribForecast {
    fn wind(&self, pos_merc: [f64; 2], time_ms: i64) -> (Bam, f64) {
        let (lat, lon) = Projection::to_wgs84(pos_merc[0], pos_merc[1]);
        let (u, v) = self.uv(lat, lon, time_ms);
        let tws_kt = (u * u + v * v).sqrt() * MS_TO_KNOTS;
        // Meteorological "from": the U/V vector points where the air GOES,
        // so the direction it comes from is its opposite (§8.5's formula).
        let from_deg = (-u).atan2(-v).to_degrees();
        (deg_to_bam(from_deg), tws_kt)
    }
}

/// The NOMADS filter URL for one step of the GFS wave model. Same runs, same
/// 0.25° grid, one field: HTSGW, the significant height of the combined sea.
fn wave_filter_url(source: GribSource, run: DateTime<Utc>, fh: u32, area: GeoBox) -> String {
    match source {
        GribSource::NoaaGfs025 => format!(
            "https://nomads.ncep.noaa.gov/cgi-bin/filter_gfswave.pl?\
             dir=%2Fgfs.{date}%2F{cyc:02}%2Fwave%2Fgridded\
             &file=gfswave.t{cyc:02}z.global.0p25.f{fh:03}.grib2\
             &var_HTSGW=on\
             &subregion=&toplat={top}&bottomlat={bottom}&leftlon={left}&rightlon={right}",
            date = run.format("%Y%m%d"),
            cyc = run.hour(),
            fh = fh,
            top = area.north.ceil(),
            bottom = area.south.floor(),
            left = area.west.floor(),
            right = area.east.ceil(),
        ),
    }
}

/// Sea state: a stack of (time, Hs-grid), the wave sibling of
/// [`GribForecast`]. Land-masked cells decode as NaN and stay NaN — the
/// sampler renormalizes around them, and a point with no wave data at all
/// answers `None`, which the engine reads as "no penalty" on purpose.
pub struct WaveForecast {
    pub run: DateTime<Utc>,
    times: Vec<i64>,
    lats: Vec<f64>,
    lons: Vec<f64>,
    hs: Vec<Vec<f32>>,
}

impl WaveForecast {
    /// Fetch waves for the same box and span as the wind. Blocking; run it
    /// on a worker, and treat failure as a warning — a passage plans without
    /// waves, never without wind.
    pub fn fetch(
        source: GribSource,
        area: GeoBox,
        hours_ahead: u32,
        cache_dir: &std::path::Path,
        mut progress: impl FnMut(String),
    ) -> Result<Self, GribError> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(60)))
            .build()
            .into();
        std::fs::create_dir_all(cache_dir).map_err(|e| GribError::Download(e.to_string()))?;

        let now = Utc::now();
        let mut run = None;
        for candidate in candidate_runs(source, now) {
            match fetch_wave_step(&agent, source, candidate, 0, area, cache_dir) {
                Ok(bytes) => {
                    run = Some((candidate, bytes));
                    break;
                }
                Err(e) => {
                    log::info!("grib: wave run {} unavailable: {e}", candidate.format("%d/%HZ"))
                }
            }
        }
        let Some((run, first)) = run else {
            return Err(GribError::NoRunAvailable);
        };

        let mut forecast = Self {
            run,
            times: Vec::new(),
            lats: Vec::new(),
            lons: Vec::new(),
            hs: Vec::new(),
        };
        forecast.absorb(&first)?;

        let step = source.step_hours();
        let last = last_step_hours(hours_ahead, run, now, step);
        let mut fh = step;
        while fh <= last {
            if crate::nav::isochrone::this_plan_cancelled() {
                return Err(GribError::Download("cancelled".into()));
            }
            progress(format!("waves +{fh:03}h"));
            let bytes = fetch_wave_step(&agent, source, run, fh, area, cache_dir)?;
            forecast.absorb(&bytes)?;
            fh += step;
        }
        log::info!(
            "grib: waves run {} — {} steps, {}×{} grid",
            run.format("%Y-%m-%d %HZ"),
            forecast.times.len(),
            forecast.lats.len(),
            forecast.lons.len()
        );
        Ok(forecast)
    }

    fn absorb(&mut self, bytes: &[u8]) -> Result<(), GribError> {
        for message in gribberish::message::read_messages(bytes) {
            // Skip what fails to identify rather than dying on it: the wave
            // file may carry parameters gribberish has no table entry for.
            let Ok(var) = message.variable_abbrev() else { continue };
            if var != "HTSGW" {
                continue;
            }
            let when = message
                .forecast_date()
                .map_err(|e| GribError::Decode(e.to_string()))?
                .timestamp_millis();
            if self.lats.is_empty() {
                let (lats, lons) = message
                    .latlng_projector()
                    .map_err(|e| GribError::Decode(e.to_string()))?
                    .lat_lng();
                self.lats = lats;
                self.lons = lons.into_iter().map(normalize_lon).collect();
            }
            let data: Vec<f32> = message
                .data()
                .map_err(|e| GribError::Decode(e.to_string()))?
                .into_iter()
                .map(|x| x as f32)
                .collect();
            if data.len() != self.lats.len() * self.lons.len() {
                return Err(GribError::Decode(format!(
                    "wave grid mismatch: {} values for {}×{}",
                    data.len(),
                    self.lats.len(),
                    self.lons.len()
                )));
            }
            self.times.push(when);
            self.hs.push(data);
            return Ok(());
        }
        Err(GribError::Decode("wave file lacks HTSGW".into()))
    }

    #[cfg(test)]
    fn synthetic(times: Vec<i64>, lats: Vec<f64>, lons: Vec<f64>, hs: Vec<Vec<f32>>) -> Self {
        Self { run: Utc::now(), times, lats, lons, hs }
    }

    /// Hs at a place and time, or `None` where the model has no sea (land
    /// mask, or outside the box on a coastal fetch). Space is bilinear with
    /// NaN corners dropped and the weights renormalized; time is linear,
    /// falling back to whichever bracketing step has data.
    fn hs_at(&self, lat: f64, lon: f64, time_ms: i64) -> Option<f64> {
        if self.times.is_empty() {
            return None;
        }
        let lon = normalize_lon(lon);
        let (i0, i1, fy) = bracket_axis(&self.lats, lat);
        let (j0, j1, fx) = bracket_axis(&self.lons, lon);
        let (t0, t1, ft) = bracket_axis_i64(&self.times, time_ms);

        let sample = |grid: &Vec<f32>| -> Option<f64> {
            let w = self.lons.len();
            let corners = [
                (grid[i0 * w + j0] as f64, (1.0 - fy) * (1.0 - fx)),
                (grid[i0 * w + j1] as f64, (1.0 - fy) * fx),
                (grid[i1 * w + j0] as f64, fy * (1.0 - fx)),
                (grid[i1 * w + j1] as f64, fy * fx),
            ];
            let (mut acc, mut wsum) = (0.0, 0.0);
            for (v, wt) in corners {
                if v.is_finite() {
                    acc += v * wt;
                    wsum += wt;
                }
            }
            (wsum > 1e-9).then(|| acc / wsum)
        };
        match (sample(&self.hs[t0]), sample(&self.hs[t1])) {
            (Some(a), Some(b)) => Some(a + (b - a) * ft),
            (a, b) => a.or(b),
        }
    }
}

impl super::isochrone::WaveField for WaveForecast {
    fn wave_height_m(&self, pos_merc: [f64; 2], time_ms: i64) -> Option<f64> {
        let (lat, lon) = Projection::to_wgs84(pos_merc[0], pos_merc[1]);
        self.hs_at(lat, lon, time_ms)
    }
}

fn fetch_wave_step(
    agent: &ureq::Agent,
    source: GribSource,
    run: DateTime<Utc>,
    fh: u32,
    area: GeoBox,
    cache_dir: &std::path::Path,
) -> Result<Vec<u8>, GribError> {
    let path = cache_dir.join(cache_key("gfswave", run, fh, area));
    fetch_cached(agent, &wave_filter_url(source, run, fh, area), &path)
}

/// One step's bytes, from cache or the wire. A cached file is keyed by run,
/// step and area, so a new run or a different passage never collides.
fn fetch_step(
    agent: &ureq::Agent,
    source: GribSource,
    run: DateTime<Utc>,
    fh: u32,
    area: GeoBox,
    cache_dir: &std::path::Path,
) -> Result<Vec<u8>, GribError> {
    // "gfsg": files cached before the gust was asked for would lack it.
    let path = cache_dir.join(cache_key("gfsg", run, fh, area));
    fetch_cached(agent, &filter_url(source, run, fh, area), &path)
}

fn cache_key(prefix: &str, run: DateTime<Utc>, fh: u32, area: GeoBox) -> String {
    format!(
        "{prefix}-{}-{:02}z-f{:03}-{}-{}-{}-{}.grib2",
        run.format("%Y%m%d"),
        run.hour(),
        fh,
        area.south.floor(),
        area.north.ceil(),
        area.west.floor(),
        area.east.ceil()
    )
}

fn fetch_cached(
    agent: &ureq::Agent,
    url: &str,
    path: &std::path::Path,
) -> Result<Vec<u8>, GribError> {
    if let Ok(bytes) = std::fs::read(path) {
        if !bytes.is_empty() {
            return Ok(bytes);
        }
    }
    let mut response = agent
        .get(url)
        .call()
        .map_err(|e| GribError::Download(e.to_string()))?;
    let mut bytes = Vec::new();
    use std::io::Read;
    response
        .body_mut()
        .as_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| GribError::Download(e.to_string()))?;
    if !complete_grib2(&bytes) {
        return Err(GribError::Download(format!(
            "not a whole GRIB answer ({} bytes)",
            bytes.len()
        )));
    }
    // Cache after validation, atomically — a truncated cache file would
    // otherwise poison every later fetch of this step.
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, &bytes).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
    Ok(bytes)
}

/// Is this a whole run of GRIB2 messages, none cut short? Each message starts
/// `GRIB`, says it is edition 2 in octet 8, states its total length in
/// octets 9–16, and ends `7777`; the messages follow one another to the
/// last byte. A download that stopped early fails the length or the end
/// marker — "at least a hundred bytes starting GRIB", the check this
/// replaced, let most truncated answers into the cache.
fn complete_grib2(bytes: &[u8]) -> bool {
    let mut at = 0usize;
    while at < bytes.len() {
        let rest = &bytes[at..];
        if rest.len() < 16 || &rest[..4] != b"GRIB" || rest[7] != 2 {
            return false;
        }
        let Ok(len) = usize::try_from(u64::from_be_bytes(rest[8..16].try_into().unwrap())) else {
            return false;
        };
        if len < 20 || len > rest.len() || &rest[len - 4..len] != b"7777" {
            return false;
        }
        at += len;
    }
    at > 0
}

/// Old forecast files are dead weight; anything older than five days at
/// `now` is deleted.
fn sweep_cache(dir: &std::path::Path, now: std::time::SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let cutoff = now - std::time::Duration::from_secs(5 * 86_400);
    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata() {
            if meta.modified().map(|m| m < cutoff).unwrap_or(false) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// A longitude in (−180, 180]. Half-open on purpose: −180 and 180 are one
/// meridian, and a grid decoded from 0..360 holds it as +180 — so a position
/// at −180 must come out as +180 too, or it falls outside the grid's last
/// column while standing on it.
fn normalize_lon(lon: f64) -> f64 {
    let mut l = lon % 360.0;
    if l > 180.0 {
        l -= 360.0;
    } else if l <= -180.0 {
        l += 360.0;
    }
    l
}

/// Bracketing indices and the interpolation fraction along an axis that may
/// ascend or descend, clamped at the ends.
fn bracket_axis(axis: &[f64], x: f64) -> (usize, usize, f64) {
    if axis.len() < 2 {
        return (0, 0, 0.0);
    }
    let ascending = axis[1] > axis[0];
    let n = axis.len();
    for i in 0..n - 1 {
        let (a, b) = (axis[i], axis[i + 1]);
        let inside = if ascending { x <= b } else { x >= b };
        if inside {
            let f = if (b - a).abs() < 1e-12 {
                0.0
            } else {
                ((x - a) / (b - a)).clamp(0.0, 1.0)
            };
            return (i, i + 1, f);
        }
    }
    (n - 2, n - 1, 1.0)
}

fn bracket_axis_i64(axis: &[i64], x: i64) -> (usize, usize, f64) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;

    /// Two times, 2×2 grid over the Kattegat: enough to prove every axis of
    /// the interpolation.
    fn forecast() -> GribForecast {
        GribForecast::synthetic(
            vec![0, 3_600_000],
            vec![57.0, 56.0], // descending, as GFS ships them
            vec![11.0, 12.0],
            // u at t0: all 10 m/s east; at t1: all 0.
            vec![vec![10.0; 4], vec![0.0; 4]],
            // v at t0: 0; at t1: all 10 m/s north.
            vec![vec![0.0; 4], vec![10.0; 4]],
        )
    }

    fn wind_at(f: &GribForecast, lat: f64, lon: f64, t: i64) -> (f64, f64) {
        let (x, y) = Projection::to_mercator(lat, lon);
        let (bam, kt) = f.wind([x, y], t);
        (super::super::angles::bam_to_deg(bam), kt)
    }

    #[test]
    fn direction_is_meteorological_from() {
        let f = forecast();
        // u=+10 (air moving east) → wind FROM the west, 270°.
        let (dir, kt) = wind_at(&f, 56.5, 11.5, 0);
        assert!((dir - 270.0).abs() < 0.1, "{dir}");
        assert!((kt - 10.0 * MS_TO_KNOTS).abs() < 0.01);
        // v=+10 (air moving north) → FROM the south, 180°.
        let (dir, _) = wind_at(&f, 56.5, 11.5, 3_600_000);
        assert!((dir - 180.0).abs() < 0.1, "{dir}");
        // The display sampler speaks the same convention as the router.
        let (dir, kt) = f.sample(56.5, 11.5, 0).expect("inside the box");
        assert!((dir - 270.0).abs() < 0.1, "{dir}");
        assert!((kt - 10.0 * MS_TO_KNOTS).abs() < 0.01);
        assert_eq!(f.sample(56.5, 14.0, 0), None, "outside the box");
    }

    /// Past the last step the field holds the last step — it does not
    /// extrapolate the trend, which here would invent a 22 m/s gale.
    #[test]
    fn beyond_the_forecast_the_last_step_holds() {
        let f = forecast();
        let (dir, kt) = wind_at(&f, 56.5, 11.5, 3 * 3_600_000);
        assert!((dir - 180.0).abs() < 0.1, "{dir}");
        assert!((kt - 10.0 * MS_TO_KNOTS).abs() < 0.01, "{kt}");
        // And before the first, the first.
        let (dir, _) = wind_at(&f, 56.5, 11.5, -3_600_000);
        assert!((dir - 270.0).abs() < 0.1, "{dir}");
    }

    /// Gusts interpolate like the wind, convert to knots, and refuse to
    /// blend a real gust with a step that had none.
    #[test]
    fn gusts_interpolate_and_need_both_steps() {
        let mut f = forecast();
        f.gust = vec![Some(vec![10.0; 4]), Some(vec![20.0; 4])];
        let g = f.gust_kt(56.5, 11.5, 1_800_000).unwrap();
        assert!((g - 15.0 * MS_TO_KNOTS).abs() < 1e-6, "{g}");
        assert_eq!(f.gust_kt(56.5, 14.0, 0), None, "outside the box");
        f.gust[1] = None;
        assert_eq!(f.gust_kt(56.5, 11.5, 1_800_000), None);
    }

    /// GFS ships longitudes 0..360; the plotter speaks ±180.
    #[test]
    fn longitudes_fold_into_plus_minus_180() {
        assert_eq!(normalize_lon(350.0), -10.0);
        assert_eq!(normalize_lon(10.0), 10.0);
        assert_eq!(normalize_lon(-190.0), 170.0);
        assert_eq!(normalize_lon(540.0), 180.0);
        // A grid decoded from 0..360 answers a western position.
        let f = GribForecast::synthetic(
            vec![0],
            vec![50.0, 49.0],
            vec![normalize_lon(349.0), normalize_lon(350.0)],
            vec![vec![0.0, 4.0, 0.0, 4.0]],
            vec![vec![0.0; 4]],
        );
        assert!(f.covers(49.5, -10.5));
        assert!((f.uv(49.5, -10.5, 0).0 - 2.0).abs() < 1e-6);
    }

    /// §8.5's whole point: halfway between "10 m/s from west" and "10 m/s
    /// from south", component interpolation gives a *south-westerly at
    /// reduced speed* — direction averaging would give the direction right
    /// and the speed wrong.
    #[test]
    fn time_interpolation_is_on_components() {
        let f = forecast();
        let (dir, kt) = wind_at(&f, 56.5, 11.5, 1_800_000);
        assert!((dir - 225.0).abs() < 0.1, "{dir}");
        let expected = (5.0f64.powi(2) + 5.0f64.powi(2)).sqrt() * MS_TO_KNOTS;
        assert!((kt - expected).abs() < 0.05, "{kt} vs {expected}");
    }

    #[test]
    fn space_interpolation_brackets_a_descending_lat_axis() {
        let f = GribForecast::synthetic(
            vec![0, 1],
            vec![57.0, 56.0],
            vec![11.0, 12.0],
            // u varies by corner: NW 0, NE 4, SW 8, SE 12.
            vec![vec![0.0, 4.0, 8.0, 12.0], vec![0.0, 4.0, 8.0, 12.0]],
            vec![vec![0.0; 4], vec![0.0; 4]],
        );
        // Dead centre: mean of the corners = 6 m/s.
        let (u, _) = f.uv(56.5, 11.5, 0);
        assert!((u - 6.0).abs() < 1e-6, "{u}");
        // A quarter of the way east along the northern edge: 0 + 4·¼.
        let (u, _) = f.uv(57.0, 11.25, 0);
        assert!((u - 1.0).abs() < 1e-6, "{u}");
        // On the southern edge, midway east: (8+12)/2.
        let (u, _) = f.uv(56.0, 11.5, 0);
        assert!((u - 10.0).abs() < 1e-6, "{u}");
        // Outside the box: clamped to the nearest edge, never NaN.
        let (u, _) = f.uv(50.0, 20.0, 0);
        assert!((u - 12.0).abs() < 1e-6, "{u}");
    }

    #[test]
    fn the_filter_url_carries_the_area_and_the_fields() {
        let run = chrono::DateTime::parse_from_rfc3339("2026-08-01T06:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let url = filter_url(
            GribSource::NoaaGfs025,
            run,
            27,
            GeoBox { south: 54.2, north: 57.8, west: 9.1, east: 13.9 },
        );
        assert!(url.contains("gfs.20260801"));
        assert!(url.contains("t06z"));
        assert!(url.contains("f027"));
        assert!(url.contains("var_UGRD=on") && url.contains("var_VGRD=on"));
        assert!(url.contains("lev_10_m_above_ground=on"));
        assert!(url.contains("toplat=58") && url.contains("bottomlat=54"));
        assert!(url.contains("leftlon=9") && url.contains("rightlon=14"));
        assert!(url.contains("var_GUST=on") && url.contains("lev_surface=on"));
    }

    #[test]
    fn the_wave_url_asks_the_wave_filter_for_htsgw() {
        let run = chrono::DateTime::parse_from_rfc3339("2026-08-01T06:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let url = wave_filter_url(
            GribSource::NoaaGfs025,
            run,
            9,
            GeoBox { south: 54.2, north: 57.8, west: 9.1, east: 13.9 },
        );
        assert!(url.contains("filter_gfswave.pl"));
        assert!(url.contains("gfswave.t06z.global.0p25.f009.grib2"));
        assert!(url.contains("var_HTSGW=on"));
        assert!(url.contains("%2Fwave%2Fgridded"));
    }

    /// The wave grid's land mask arrives as NaN; a sample next to the coast
    /// must renormalize around it, and a sample wholly on land must answer
    /// `None` — not zero, and never NaN.
    #[test]
    fn wave_sampling_renormalizes_around_the_land_mask() {
        let f = WaveForecast::synthetic(
            vec![0, 3_600_000],
            vec![57.0, 56.0],
            vec![11.0, 12.0],
            // t0: NW corner is land; the three sea corners all read 2 m.
            // t1: everything is land.
            vec![
                vec![f32::NAN, 2.0, 2.0, 2.0],
                vec![f32::NAN; 4],
            ],
        );
        // Dead centre at t0: the NaN corner drops out, the rest agree on 2.
        assert!((f.hs_at(56.5, 11.5, 0).unwrap() - 2.0).abs() < 1e-6);
        // Halfway in time: t1 has no data, so t0's answer stands alone.
        assert!((f.hs_at(56.5, 11.5, 1_800_000).unwrap() - 2.0).abs() < 1e-6);
        // On the land corner itself at t1: no data at all.
        assert!(f.hs_at(57.0, 11.0, 3_600_000).is_none());
    }

    #[test]
    fn wave_time_interpolation_is_linear() {
        let f = WaveForecast::synthetic(
            vec![0, 3_600_000],
            vec![57.0, 56.0],
            vec![11.0, 12.0],
            vec![vec![1.0; 4], vec![3.0; 4]],
        );
        assert!((f.hs_at(56.5, 11.5, 1_800_000).unwrap() - 2.0).abs() < 1e-6);
    }

    #[test]
    fn candidate_runs_are_recent_cycles_newest_first() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-01T14:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let runs = candidate_runs(GribSource::NoaaGfs025, now);
        assert_eq!(runs.len(), 4);
        // 14:30 − 5 h = 09:30 → newest candidate is the 06Z run.
        assert_eq!(runs[0].hour(), 6);
        assert_eq!(runs[1].hour(), 0);
        assert_eq!(runs[2].hour(), 18, "yesterday's evening run");
        for w in runs.windows(2) {
            assert!(w[0] > w[1]);
        }
    }

    /// The 06Z run at 14:30 is 8 h 30 min old. Twenty-four hours from now
    /// is forecast hour 32.5: the next 3-hourly step is +33.
    #[test]
    fn the_forecast_reaches_the_hours_asked_for_from_now_not_from_the_run() {
        let at = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        let run = at("2026-08-01T06:00:00Z");
        assert_eq!(last_step_hours(24, run, at("2026-08-01T14:30:00Z"), 3), 33);
        // A run exactly six hours old: +30 is 24 h from now, on the step.
        assert_eq!(last_step_hours(24, run, at("2026-08-01T12:00:00Z"), 3), 30);
        // A clock behind the run adds nothing and never goes negative.
        assert_eq!(last_step_hours(24, run, at("2026-08-01T05:00:00Z"), 3), 24);
    }

    // ---- GRIB2 assembled by hand --------------------------------------
    //
    // `absorb` and both `fetch`es read real GRIB2 bytes. Rather than a binary
    // fixture, the tests write the messages themselves, byte by byte, after
    // the layout of the NOMADS files Manx caches (WMO FM 92 GRIB edition 2):
    // a regular lat/lon grid (template 3.0), a forecast at a point in time
    // (4.0), simple packing at 8 bits with no scaling (5.0), no bitmap.
    // Values are whole numbers within 255 of the smallest, so the decode is
    // exact and every expected number below is a number written here.

    /// Where a message's grid sits. `north_first` is GRIB's −j scan, the
    /// order GFS ships its rows in; otherwise rows run south to north.
    /// `npts` overrides the point count, to build a self-contradicting file.
    #[derive(Clone, Copy)]
    struct Grid {
        la1: f64,
        lo1: f64,
        nj: u32,
        ni: u32,
        step: f64,
        north_first: bool,
        npts: Option<u32>,
    }

    /// 2 rows × 3 columns from 56°N 10°E, a quarter degree apart, south row
    /// first: lats [56.0, 56.25], lons [10.0, 10.25, 10.5]. Not square, so a
    /// `lats × lons` that became `lats + lons` cannot pass by coincidence.
    const KATTEGAT_2X3: Grid = Grid {
        la1: 56.0,
        lo1: 10.0,
        nj: 2,
        ni: 3,
        step: 0.25,
        north_first: false,
        npts: None,
    };

    /// (discipline, parameter category, parameter number) per WMO code
    /// tables 0.0 and 4.2.
    const UGRD: (u8, u8, u8) = (0, 2, 2);
    const VGRD: (u8, u8, u8) = (0, 2, 3);
    const GUST: (u8, u8, u8) = (0, 2, 22);
    const TMP: (u8, u8, u8) = (0, 0, 0);
    const HTSGW: (u8, u8, u8) = (10, 0, 3);

    fn grib2(
        param: (u8, u8, u8),
        run: DateTime<Utc>,
        fh: u32,
        grid: Grid,
        values: &[f64],
    ) -> Vec<u8> {
        fn section(num: u8, body: &[u8]) -> Vec<u8> {
            let mut s = ((body.len() + 5) as u32).to_be_bytes().to_vec();
            s.push(num);
            s.extend_from_slice(body);
            s
        }
        let micro = |deg: f64| {
            assert!(deg >= 0.0, "the builder writes unsigned coordinates only");
            ((deg * 1e6).round() as u32).to_be_bytes()
        };
        let (disc, cat, num) = param;
        let span_j = grid.step * (grid.nj - 1) as f64;
        let la2 = if grid.north_first { grid.la1 - span_j } else { grid.la1 + span_j };
        let lo2 = grid.lo1 + grid.step * (grid.ni - 1) as f64;
        let npts = grid.npts.unwrap_or(grid.ni * grid.nj);

        let mut s1 = vec![0, 7, 0, 0, 2, 1, 1]; // NCEP, tables v2/1, ref = start of forecast
        s1.extend_from_slice(&(run.year() as u16).to_be_bytes());
        s1.extend_from_slice(&[run.month() as u8, run.day() as u8, run.hour() as u8, 0, 0, 0, 1]);

        let mut s3 = vec![0];
        s3.extend_from_slice(&npts.to_be_bytes());
        s3.extend_from_slice(&[0, 0, 0, 0]); // no optional list; template 3.0
        s3.extend_from_slice(&[6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // spherical earth
        s3.extend_from_slice(&grid.ni.to_be_bytes());
        s3.extend_from_slice(&grid.nj.to_be_bytes());
        s3.extend_from_slice(&[0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]);
        s3.extend_from_slice(&micro(grid.la1));
        s3.extend_from_slice(&micro(grid.lo1));
        s3.push(0x30);
        s3.extend_from_slice(&micro(la2));
        s3.extend_from_slice(&micro(lo2));
        s3.extend_from_slice(&micro(grid.step));
        s3.extend_from_slice(&micro(grid.step));
        s3.push(if grid.north_first { 0x00 } else { 0x40 });

        let mut s4 = vec![0, 0, 0, 0, cat, num, 2, 0, 0x51, 0, 0, 0, 1];
        s4.extend_from_slice(&fh.to_be_bytes());
        s4.extend_from_slice(&[103, 0, 0, 0, 0, 10, 0xff, 0, 0, 0, 0, 0]); // 10 m above ground

        let reference = values.iter().cloned().fold(f64::INFINITY, f64::min);
        let packed: Vec<u8> = values
            .iter()
            .map(|&v| {
                let x = v - reference;
                assert!(x.fract() == 0.0 && (0.0..=255.0).contains(&x), "{v} is not packable");
                x as u8
            })
            .collect();
        let mut s5 = (values.len() as u32).to_be_bytes().to_vec();
        s5.extend_from_slice(&[0, 0]);
        s5.extend_from_slice(&(reference as f32).to_be_bytes());
        s5.extend_from_slice(&[0, 0, 0, 0, 8, 0]); // E = 0, D = 0, 8 bits, floats

        let body: Vec<u8> = [
            section(1, &s1),
            section(3, &s3),
            section(4, &s4),
            section(5, &s5),
            section(6, &[0xff]),
            section(7, &packed),
            b"7777".to_vec(),
        ]
        .concat();
        let mut msg = b"GRIB".to_vec();
        msg.extend_from_slice(&[0, 0, disc, 2]);
        msg.extend_from_slice(&((body.len() + 16) as u64).to_be_bytes());
        msg.extend_from_slice(&body);
        msg
    }

    fn run_2026() -> DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-10-19T06:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    const HOUR_MS: i64 = 3_600_000;

    fn empty_forecast() -> GribForecast {
        GribForecast::synthetic(vec![], vec![], vec![], vec![], vec![])
    }

    fn empty_waves() -> WaveForecast {
        WaveForecast::synthetic(vec![], vec![], vec![], vec![])
    }

    const U6: [f64; 6] = [-3.0, 1.0, 4.0, 1.0, 5.0, 9.0];
    const V6: [f64; 6] = [2.0, -7.0, 1.0, 8.0, -2.0, 8.0];
    const G6: [f64; 6] = [10.0, 12.0, 15.0, 11.0, 14.0, 20.0];

    /// The builder itself, checked against what a GRIB2 reader sees: if
    /// this fails, every absorb test below is testing the builder.
    #[test]
    fn the_hand_built_grib_message_decodes_to_what_was_written() {
        let bytes = grib2(UGRD, run_2026(), 3, KATTEGAT_2X3, &U6);
        let msgs: Vec<_> = gribberish::message::read_messages(&bytes).collect();
        assert_eq!(msgs.len(), 1);
        let m = &msgs[0];
        assert_eq!(m.variable_abbrev().unwrap(), "UGRD");
        assert_eq!(m.forecast_date().unwrap(), run_2026() + Duration::hours(3));
        assert_eq!(m.data().unwrap(), U6.to_vec());
        let (lats, lons) = m.latlng_projector().unwrap().lat_lng();
        assert_eq!(lats, vec![56.0, 56.25]);
        assert_eq!(lons, vec![10.0, 10.25, 10.5]);
        let wave = grib2(HTSGW, run_2026(), 0, KATTEGAT_2X3, &G6);
        let m = gribberish::message::read_messages(&wave).next().unwrap();
        assert_eq!(m.variable_abbrev().unwrap(), "HTSGW");
    }

    /// One NOMADS step file: the gust, an unrelated field, then U and V,
    /// all valid at run + 3 h. Absorbing it adds exactly one step whose
    /// grids are the U, V and gust written — the unrelated field ignored,
    /// not mistaken for the gust.
    #[test]
    fn absorb_stacks_one_step_of_u_v_and_gust_and_ignores_other_fields() {
        let run = run_2026();
        let file = [
            grib2(GUST, run, 3, KATTEGAT_2X3, &G6),
            grib2(TMP, run, 3, KATTEGAT_2X3, &[280.0, 281.0, 282.0, 283.0, 284.0, 285.0]),
            grib2(UGRD, run, 3, KATTEGAT_2X3, &U6),
            grib2(VGRD, run, 3, KATTEGAT_2X3, &V6),
        ]
        .concat();
        let mut f = empty_forecast();
        f.absorb(&file).expect("a complete step file");
        let t = run.timestamp_millis() + 3 * HOUR_MS;
        assert_eq!(f.steps(), &[t]);
        assert_eq!(f.lats, vec![56.0, 56.25]);
        assert_eq!(f.lons, vec![10.0, 10.25, 10.5]);
        let as_f32 = |v: &[f64]| v.iter().map(|&x| x as f32).collect::<Vec<f32>>();
        assert_eq!(f.u, vec![as_f32(&U6)]);
        assert_eq!(f.v, vec![as_f32(&V6)]);
        assert_eq!(f.gust, vec![Some(as_f32(&G6))]);
        // Row 1 is 56.25°N (south row first), column 0 is 10°E: index 3.
        let (u, v) = f.uv(56.25, 10.0, t);
        assert!((u - 1.0).abs() < 1e-9 && (v - 8.0).abs() < 1e-9, "{u} {v}");
        assert!((f.gust_kt(56.25, 10.0, t).unwrap() - 11.0 * MS_TO_KNOTS).abs() < 1e-9);

        // A second file stacks a second step behind the first.
        let next = [
            grib2(UGRD, run, 6, KATTEGAT_2X3, &V6),
            grib2(VGRD, run, 6, KATTEGAT_2X3, &U6),
        ]
        .concat();
        f.absorb(&next).unwrap();
        assert_eq!(f.steps(), &[t, t + 3 * HOUR_MS]);
        assert_eq!(f.gust[1], None, "that file had no gust");
    }

    /// U from one time and V from another are not a wind: pairing them
    /// would invent a vector that never existed.
    #[test]
    fn absorb_refuses_u_and_v_from_different_times() {
        let run = run_2026();
        let file = [
            grib2(UGRD, run, 3, KATTEGAT_2X3, &U6),
            grib2(VGRD, run, 6, KATTEGAT_2X3, &V6),
        ]
        .concat();
        let mut f = empty_forecast();
        assert!(matches!(f.absorb(&file), Err(GribError::Decode(_))));
        assert!(f.steps().is_empty() && f.u.is_empty() && f.v.is_empty());
    }

    /// A gust valid at another time belongs to another step; the step is
    /// still taken, just without a gust.
    #[test]
    fn absorb_drops_a_gust_from_another_time() {
        let run = run_2026();
        let file = [
            grib2(GUST, run, 0, KATTEGAT_2X3, &G6),
            grib2(UGRD, run, 3, KATTEGAT_2X3, &U6),
            grib2(VGRD, run, 3, KATTEGAT_2X3, &V6),
        ]
        .concat();
        let mut f = empty_forecast();
        f.absorb(&file).unwrap();
        assert_eq!(f.gust, vec![None]);
        assert_eq!(f.gust_kt(56.0, 10.0, f.steps()[0]), None);
    }

    #[test]
    fn absorb_refuses_a_file_without_both_components() {
        let run = run_2026();
        for file in [
            grib2(UGRD, run, 3, KATTEGAT_2X3, &U6),
            grib2(VGRD, run, 3, KATTEGAT_2X3, &V6),
            grib2(GUST, run, 3, KATTEGAT_2X3, &G6),
            Vec::new(),
        ] {
            let mut f = empty_forecast();
            assert!(f.absorb(&file).is_err());
            assert!(f.steps().is_empty());
        }
    }

    /// A message whose point count disagrees with its own Ni × Nj cannot be
    /// laid on the grid; indexing it would read the wrong cells.
    #[test]
    fn absorb_refuses_a_grid_that_contradicts_its_point_count() {
        let short = Grid { npts: Some(5), ..KATTEGAT_2X3 };
        let five = &U6[..5];
        let file = [
            grib2(UGRD, run_2026(), 3, short, five),
            grib2(VGRD, run_2026(), 3, short, five),
        ]
        .concat();
        let mut f = empty_forecast();
        assert!(matches!(f.absorb(&file), Err(GribError::Decode(_))));
        let mut w = empty_waves();
        let wave = grib2(HTSGW, run_2026(), 3, short, five);
        assert!(matches!(w.absorb(&wave), Err(GribError::Decode(_))));
        assert!(w.steps_for_test().is_empty());
    }

    impl WaveForecast {
        fn steps_for_test(&self) -> &[i64] {
            &self.times
        }
    }

    /// The wave file may lead with other parameters; the sea-state grid is
    /// the HTSGW one, never whichever came first.
    #[test]
    fn wave_absorb_takes_the_htsgw_message_and_nothing_else() {
        let run = run_2026();
        let hs = [0.0, 1.0, 2.0, 3.0, 4.0, 5.0];
        let file = [grib2(UGRD, run, 6, KATTEGAT_2X3, &U6), grib2(HTSGW, run, 6, KATTEGAT_2X3, &hs)]
            .concat();
        let mut w = empty_waves();
        w.absorb(&file).unwrap();
        assert_eq!(w.steps_for_test(), &[run.timestamp_millis() + 6 * HOUR_MS]);
        assert_eq!(w.hs, vec![hs.iter().map(|&x| x as f32).collect::<Vec<_>>()]);
        assert_eq!(w.lats, vec![56.0, 56.25]);
        assert_eq!(w.lons, vec![10.0, 10.25, 10.5]);

        let mut w = empty_waves();
        let wind_only = grib2(UGRD, run, 6, KATTEGAT_2X3, &U6);
        assert!(matches!(w.absorb(&wind_only), Err(GribError::Decode(_))));
        assert!(w.hs.is_empty());
    }

    // ---- fetch, from the cache only -------------------------------------

    /// A URL nothing listens on: connection refused at once, no network.
    const DEAD_URL: &str = "http://127.0.0.1:1/filter";

    fn test_agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(5)))
            .build()
            .into()
    }

    /// Serve one HTTP 200 with `body` on loopback; returns its URL.
    fn serve_once(body: Vec<u8>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(&body);
            }
        });
        format!("http://{addr}/filter")
    }

    /// Every 6-hourly run from two days back to the next cycle: a superset
    /// of whatever `candidate_runs` picks now.
    fn runs_around_now() -> Vec<DateTime<Utc>> {
        let now = Utc::now().timestamp();
        let cycle = now - now.rem_euclid(6 * 3600);
        (-8..=1)
            .map(|k| DateTime::from_timestamp(cycle + k * 6 * 3600, 0).unwrap())
            .collect()
    }

    /// `fetch` is testable offline only because every file it asks for is
    /// already cached. Prove what that rests on before calling it, so a
    /// mutated helper fails here rather than reaching out to NOMADS.
    fn assert_fetch_stays_offline(cached_runs: &[DateTime<Utc>]) {
        assert_eq!(GribSource::NoaaGfs025.step_hours(), 3, "the cache below holds 3-hourly steps");
        let probe = tempfile::tempdir().unwrap();
        let p = probe.path().join("probe.grib2");
        std::fs::write(&p, b"GRIB probe").unwrap();
        sweep_cache(probe.path(), std::time::SystemTime::now());
        assert!(p.exists(), "a fresh cache file must survive the sweep");
        assert_eq!(fetch_cached(&test_agent(), DEAD_URL, &p).unwrap(), b"GRIB probe");
        for run in candidate_runs(GribSource::NoaaGfs025, Utc::now()) {
            assert!(cached_runs.contains(&run), "{run} not cached");
        }
    }

    /// The passage box of the fetch tests: keys 56-57-10-11.
    const AREA: GeoBox = GeoBox { south: 56.05, north: 56.2, west: 10.05, east: 10.45 };

    /// Asked for six hours, the steps run from the model run, every three
    /// hours with none missing, to the first one at or past six hours from
    /// *now* — and no further than one step beyond it.
    fn assert_reaches_six_hours_from_now(t0: i64, steps: &[i64]) {
        for (k, &t) in steps.iter().enumerate() {
            assert_eq!(t, t0 + 3 * k as i64 * HOUR_MS, "step {k}");
        }
        let wanted = Utc::now().timestamp_millis() + 6 * HOUR_MS;
        let last = *steps.last().unwrap();
        // The clock moved on between the fetch and this line; a minute's
        // grace on the near side.
        assert!(last >= wanted - 120_000, "stops {} min short", (wanted - last) / 60_000);
        assert!(last < wanted + 3 * HOUR_MS, "runs {} min past", (last - wanted) / 60_000);
    }

    /// With every step cached, `fetch` takes the newest run and stacks its
    /// steps from the run to six hours from now — each one, in order.
    #[test]
    fn wind_fetch_stacks_every_three_hour_step_to_the_horizon_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let runs = runs_around_now();
        for &run in &runs {
            for fh in (0u32..=24).step_by(3) {
                // U = fh, V = 10 − fh everywhere, so each step is recognisable.
                let file = [
                    grib2(UGRD, run, fh, KATTEGAT_2X3, &[fh as f64; 6]),
                    grib2(VGRD, run, fh, KATTEGAT_2X3, &[10.0 - fh as f64; 6]),
                ]
                .concat();
                std::fs::write(dir.path().join(cache_key("gfsg", run, fh, AREA)), file).unwrap();
            }
        }
        assert_fetch_stays_offline(&runs);
        let newest = || candidate_runs(GribSource::NoaaGfs025, Utc::now())[0];
        let before = newest();
        let f = GribForecast::fetch(GribSource::NoaaGfs025, AREA, 6, dir.path(), |_| {})
            .expect("every step is cached");
        // The newest candidate, allowing for a cycle turning over meanwhile.
        assert!(f.run == before || f.run == newest(), "took run {}", f.run);
        let t0 = f.run.timestamp_millis();
        assert_reaches_six_hours_from_now(t0, f.steps());
        for (k, &t) in f.steps().iter().enumerate() {
            let (u, v) = f.uv(56.1, 10.1, t);
            let fh = 3.0 * k as f64;
            assert!((u - fh).abs() < 1e-9 && (v - (10.0 - fh)).abs() < 1e-9, "step {k}: {u} {v}");
        }
    }

    #[test]
    fn wave_fetch_stacks_every_three_hour_step_to_the_horizon_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let runs = runs_around_now();
        for &run in &runs {
            for fh in (0u32..=24).step_by(3) {
                let file = grib2(HTSGW, run, fh, KATTEGAT_2X3, &[1.0 + fh as f64; 6]);
                std::fs::write(dir.path().join(cache_key("gfswave", run, fh, AREA)), file).unwrap();
            }
        }
        assert_fetch_stays_offline(&runs);
        let newest = || candidate_runs(GribSource::NoaaGfs025, Utc::now())[0];
        let before = newest();
        let w = WaveForecast::fetch(GribSource::NoaaGfs025, AREA, 6, dir.path(), |_| {})
            .expect("every step is cached");
        // The newest candidate, allowing for a cycle turning over meanwhile.
        assert!(w.run == before || w.run == newest(), "took run {}", w.run);
        let t0 = w.run.timestamp_millis();
        assert_reaches_six_hours_from_now(t0, w.steps_for_test());
        assert!((w.hs_at(56.1, 10.1, t0 + 6 * HOUR_MS).unwrap() - 7.0).abs() < 1e-9);
    }

    /// Keyed by run, step and area: a new run or a different passage never
    /// collides (the doc on `fetch_step`).
    #[test]
    fn cache_keys_differ_by_run_step_area_and_product() {
        let run = run_2026();
        let key = |prefix, run, fh, area| cache_key(prefix, run, fh, area);
        let base = key("gfsg", run, 3, AREA);
        assert!(base.ends_with(".grib2"), "{base}");
        let other_area = GeoBox { south: 50.0, ..AREA };
        for other in [
            key("gfswave", run, 3, AREA),
            key("gfsg", run - Duration::hours(6), 3, AREA),
            key("gfsg", run, 6, AREA),
            key("gfsg", run, 3, other_area),
        ] {
            assert_ne!(other, base);
        }
    }

    #[test]
    fn a_cached_file_is_answered_without_asking_the_wire() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("step.grib2");
        let bytes = grib2(UGRD, run_2026(), 0, KATTEGAT_2X3, &U6);
        std::fs::write(&p, &bytes).unwrap();
        assert_eq!(fetch_cached(&test_agent(), DEAD_URL, &p).unwrap(), bytes);
    }

    /// An empty cache file is a failed write, not a forecast: fetch again
    /// (and here fail, the wire being dead).
    #[test]
    fn an_empty_cached_file_is_fetched_again() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("step.grib2");
        std::fs::write(&p, b"").unwrap();
        assert!(matches!(fetch_cached(&test_agent(), DEAD_URL, &p), Err(GribError::Download(_))));
    }

    /// A GRIB answer is cached, atomically, and answered from the cache next
    /// time.
    #[test]
    fn a_downloaded_grib_is_cached_after_validation() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("step.grib2");
        let bytes = grib2(UGRD, run_2026(), 0, KATTEGAT_2X3, &U6);
        assert!(bytes.len() > 100);
        let url = serve_once(bytes.clone());
        assert_eq!(fetch_cached(&test_agent(), &url, &p).unwrap(), bytes);
        assert_eq!(std::fs::read(&p).unwrap(), bytes);
        assert!(!p.with_extension("tmp").exists());
        assert_eq!(fetch_cached(&test_agent(), DEAD_URL, &p).unwrap(), bytes);
    }

    /// NOMADS answers errors with a 200 and an HTML page, and a dropped
    /// connection leaves a truncated body. Neither is a forecast, and
    /// neither may be cached — a bad cache file would poison every later
    /// fetch of that step.
    #[test]
    fn a_non_grib_or_truncated_answer_is_refused_and_not_cached() {
        let full = grib2(UGRD, run_2026(), 0, KATTEGAT_2X3, &U6);
        let html = b"<html><body>data file is not present: /gfs.20261019/06 ... </body></html> "
            .repeat(4);
        assert!(html.len() >= 100);
        for body in [html, full[..60].to_vec(), b"GRIB".to_vec(), Vec::new()] {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("step.grib2");
            let url = serve_once(body.clone());
            let got = fetch_cached(&test_agent(), &url, &p);
            assert!(matches!(got, Err(GribError::Download(_))), "{} bytes accepted", body.len());
            assert!(!p.exists(), "refused answer was cached");
        }
    }

    /// Five days, per the doc: older goes, younger stays.
    #[test]
    fn the_cache_sweep_deletes_files_older_than_five_days_only() {
        let dir = tempfile::tempdir().unwrap();
        let now = std::time::SystemTime::now();
        let day = std::time::Duration::from_secs(86_400);
        let aged = |name: &str, age: std::time::Duration| {
            let p = dir.path().join(name);
            let f = std::fs::File::create(&p).unwrap();
            f.set_modified(now - age).unwrap();
            p
        };
        let old = aged("old.grib2", 6 * day);
        let four_days = aged("four.grib2", 4 * day);
        let hour = aged("hour.grib2", std::time::Duration::from_secs(3600));
        // "Older than five days": exactly five days is not older, a second
        // more is. The sweep is given `now`, so the boundary is exact.
        let five_days = aged("five.grib2", 5 * day);
        let five_and_a_second = aged("five-plus.grib2", 5 * day + std::time::Duration::from_secs(1));
        sweep_cache(dir.path(), now);
        assert!(!old.exists(), "six days old");
        assert!(four_days.exists(), "four days old");
        assert!(hour.exists(), "an hour old");
        assert!(five_days.exists(), "exactly five days old is not older than five days");
        assert!(!five_and_a_second.exists(), "five days and a second is");
        // A missing directory is not an error.
        sweep_cache(&dir.path().join("absent"), now);
    }

    // ---- small things the UI reads --------------------------------------

    /// The module doc: the UI shows which source is in use.
    #[test]
    fn the_source_label_names_noaa_gfs() {
        let label = GribSource::NoaaGfs025.label();
        assert!(label.contains("NOAA") && label.contains("GFS"), "{label}");
    }

    /// Each error reaches the user as a sentence carrying its cause.
    #[test]
    fn errors_read_as_sentences_with_their_cause() {
        let d = GribError::Download("timed out".into()).to_string();
        assert!(d.contains("download") && d.contains("timed out"), "{d}");
        let d = GribError::Decode("bad section 5".into()).to_string();
        assert!(d.contains("bad section 5"), "{d}");
        let d = GribError::NoRunAvailable.to_string();
        assert!(d.contains("connection"), "{d}");
    }

    #[test]
    fn steps_and_span_are_the_decoded_valid_times() {
        let f = GribForecast::synthetic(
            vec![5 * HOUR_MS, 8 * HOUR_MS, 11 * HOUR_MS],
            vec![57.0, 56.0],
            vec![11.0, 12.0],
            vec![vec![0.0; 4]; 3],
            vec![vec![0.0; 4]; 3],
        );
        assert_eq!(f.steps(), &[5 * HOUR_MS, 8 * HOUR_MS, 11 * HOUR_MS]);
        assert_eq!(f.valid_span(), Some((5 * HOUR_MS, 11 * HOUR_MS)));
        assert_eq!(empty_forecast().valid_span(), None);
    }

    /// The bounds of a margin box: the extreme points, pushed out by the
    /// margin on every side.
    #[test]
    fn geobox_around_is_the_extremes_plus_the_margin() {
        let pts = [
            crate::geo::LatLon::new(55.0, 10.0),
            crate::geo::LatLon::new(56.5, 12.5),
            crate::geo::LatLon::new(55.5, 11.0),
        ];
        let b = GeoBox::around(&pts, 0.5);
        assert_eq!(b, GeoBox { south: 54.5, north: 57.0, west: 9.5, east: 13.0 });
        // Southern and western hemispheres too: the seeds must not leak in.
        let b = GeoBox::around(&[crate::geo::LatLon::new(-33.0, -70.0)], 0.0);
        assert_eq!(b, GeoBox { south: -33.0, north: -33.0, west: -70.0, east: -70.0 });
    }

    /// An axis that decoded to a non-finite value is a broken grid, not one
    /// stretching to the pole: it covers nothing.
    #[test]
    fn a_grid_with_a_non_finite_axis_covers_nothing() {
        let f = GribForecast::synthetic(
            vec![0],
            vec![56.0, f64::INFINITY],
            vec![11.0, 12.0],
            vec![vec![0.0; 4]],
            vec![vec![0.0; 4]],
        );
        assert!(!f.covers(57.0, 11.5));
        assert_eq!(f.sample(57.0, 11.5, 0), None);
    }

    /// u = 3, v = 4 m/s: the air moves toward 036.87° at 5 m/s, so it comes
    /// FROM 216.87° — speed and direction by hand, both samplers.
    #[test]
    fn samplers_give_speed_and_from_direction_of_a_3_4_5_wind() {
        let f = GribForecast::synthetic(
            vec![0],
            vec![57.0, 56.0],
            vec![11.0, 12.0],
            vec![vec![3.0; 4]],
            vec![vec![4.0; 4]],
        );
        let from = 180.0 + 3.0f64.atan2(4.0).to_degrees(); // 216.87°
        let kt = 5.0 * MS_TO_KNOTS;
        let (d, s) = f.sample(56.5, 11.5, 0).unwrap();
        assert!((d - from).abs() < 1e-9 && (s - kt).abs() < 1e-9, "{d} {s}");
        let (d, s) = f.sample_held(56.5, 11.5, 0);
        assert!((d - from).abs() < 1e-9 && (s - kt).abs() < 1e-9, "{d} {s}");
    }

    /// Outside the box, `sample_held` answers the nearest edge's wind — here
    /// the south-east corner, the only one blowing.
    #[test]
    fn sample_held_holds_the_edge_wind_outward() {
        let f = GribForecast::synthetic(
            vec![0],
            vec![57.0, 56.0],
            vec![11.0, 12.0],
            vec![vec![0.0, 0.0, 0.0, -6.0]], // SE corner: air going west…
            vec![vec![0.0, 0.0, 0.0, 8.0]],  // …and north: from 143.13°
        );
        let (d, s) = f.sample_held(55.0, 13.0, 0);
        let from = (6.0f64).atan2(-8.0).to_degrees(); // 143.13°
        assert!((d - from).abs() < 1e-9, "{d}");
        assert!((s - 10.0 * MS_TO_KNOTS).abs() < 1e-9, "{s}");
        assert_eq!(f.sample(55.0, 13.0, 0), None, "the display sampler refuses it");
    }

    /// A 3 × 4 grid with every cell distinct, and a point between rows 1–2
    /// and columns 1–2, so the cell lookup `i·w + j` is exercised with both
    /// terms non-zero. Lats descend as GFS ships them.
    ///
    /// ```text
    ///          10°  11°  12°  13°
    ///   58°N    1    2    3    4
    ///   57°N    5    7   11   13
    ///   56°N   17   19   29   31
    /// ```
    ///
    /// At 56.25°N 11.25°E: fy = (56.25−57)/(56−57) = ¾, fx = ¼.
    /// north edge 7 + (11−7)·¼ = 8; south edge 19 + (29−19)·¼ = 21.5;
    /// between them 8 + (21.5−8)·¾ = 18.125.
    const CELLS_3X4: [f32; 12] = [1.0, 2.0, 3.0, 4.0, 5.0, 7.0, 11.0, 13.0, 17.0, 19.0, 29.0, 31.0];

    fn waves_3x4(hs: Vec<f32>) -> WaveForecast {
        WaveForecast::synthetic(
            vec![0],
            vec![58.0, 57.0, 56.0],
            vec![10.0, 11.0, 12.0, 13.0],
            vec![hs],
        )
    }

    #[test]
    fn gust_is_bilinear_inside_a_cell_away_from_the_origin() {
        let mut f = GribForecast::synthetic(
            vec![0, HOUR_MS],
            vec![58.0, 57.0, 56.0],
            vec![10.0, 11.0, 12.0, 13.0],
            vec![vec![0.0; 12]; 2],
            vec![vec![0.0; 12]; 2],
        );
        f.gust = vec![Some(CELLS_3X4.to_vec()), Some(CELLS_3X4.to_vec())];
        let g = f.gust_kt(56.25, 11.25, 0).unwrap();
        assert!((g - 18.125 * MS_TO_KNOTS).abs() < 1e-9, "{g}");
    }

    /// The same cell for waves (in decimetres → metres): all four corners
    /// at sea, the renormalised weights are the bilinear ones, 1.8125 m.
    #[test]
    fn wave_height_is_bilinear_inside_a_cell_away_from_the_origin() {
        let dm: Vec<f32> = CELLS_3X4.iter().map(|x| x / 10.0).collect();
        let w = waves_3x4(dm);
        let h = w.hs_at(56.25, 11.25, 0).unwrap();
        assert!((h - 1.8125).abs() < 1e-6, "{h}");
    }

    /// The coast: the NW corner (0.7 m) of that cell is land. Its weight
    /// (¼·¾ = 0.1875) drops out and the other three are scaled up to sum
    /// to one: (1.1·0.0625 + 1.9·0.5625 + 2.9·0.1875) / 0.8125 = 269/130 m.
    /// Treating the land as 0 m would give 1.68 m — dragged toward zero.
    #[test]
    fn wave_height_renormalises_around_a_land_corner() {
        let mut dm: Vec<f32> = CELLS_3X4.iter().map(|x| x / 10.0).collect();
        dm[5] = f32::NAN;
        let w = waves_3x4(dm);
        let h = w.hs_at(56.25, 11.25, 0).unwrap();
        assert!((h - 269.0 / 130.0).abs() < 1e-6, "{h}");
        assert!((1.1..=2.9).contains(&h), "outside its sea corners: {h}");
    }

    /// The router's view of the same field, through Mercator.
    #[test]
    fn the_wave_field_answers_the_router_in_mercator() {
        use super::super::isochrone::WaveField;
        let dm: Vec<f32> = CELLS_3X4.iter().map(|x| x / 10.0).collect();
        let w = waves_3x4(dm);
        let (x, y) = Projection::to_mercator(56.25, 11.25);
        let h = w.wave_height_m([x, y], 0).unwrap();
        assert!((h - 1.8125).abs() < 1e-5, "{h}");
        assert_eq!(empty_waves().wave_height_m([x, y], 0), None, "no steps, no waves");
    }

    #[test]
    fn bracket_axis_finds_the_cell_on_a_longer_axis() {
        // Three nodes a quarter degree apart, the value in the second cell.
        assert_eq!(bracket_axis(&[10.0, 10.25, 10.5], 10.375), (1, 2, 0.5));
        // Descending.
        assert_eq!(bracket_axis(&[58.0, 57.0, 56.0], 56.25), (1, 2, 0.75));
        // Past either end: the end cell, the fraction pinned to its node.
        let axis = [0.0, 1.0, 2.0, 3.0, 4.0];
        assert_eq!(bracket_axis(&axis, 9.0), (3, 4, 1.0));
        assert_eq!(bracket_axis(&axis, -9.0), (0, 1, 0.0));
        // Fewer than two nodes: nothing to interpolate.
        assert_eq!(bracket_axis(&[5.0], 7.0), (0, 0, 0.0));
    }

    /// Longitudes across Greenwich: nodes either side of 0°, and one on it.
    #[test]
    fn bracket_axis_across_the_prime_meridian() {
        assert_eq!(bracket_axis(&[-0.25, 0.25], 0.125), (0, 1, 0.75));
        assert_eq!(bracket_axis(&[-0.25, 0.0, 0.25], -0.125), (0, 1, 0.5));
        assert_eq!(bracket_axis(&[0.25, 0.0, -0.25], 0.125), (0, 1, 0.5));
    }

    /// Coincident nodes give a finite weight — never 0/0 = NaN, which would
    /// poison the router — and a descending axis that starts with a
    /// repeated node still finds the cell the value lies in.
    #[test]
    fn bracket_axis_survives_coincident_nodes() {
        let (i0, i1, f) = bracket_axis(&[5.0, 5.0], 5.0);
        assert_eq!((i0, i1), (0, 1));
        assert!(f.is_finite(), "{f}");
        assert_eq!(bracket_axis(&[5.0, 5.0, 4.0], 4.5), (1, 2, 0.5));
        // Nodes 1e-12 apart, the value on the second: the fraction is 1.
        assert_eq!(bracket_axis(&[0.0, 1e-12], 1e-12), (0, 1, 1.0));
    }

    /// Times that do not start at zero, so an offset error cannot hide.
    #[test]
    fn bracket_axis_i64_on_an_axis_away_from_zero() {
        assert_eq!(bracket_axis_i64(&[1000, 3000], 2000), (0, 1, 0.5));
        assert_eq!(bracket_axis_i64(&[1000, 3000, 7000], 4000), (1, 2, 0.25));
        let axis = [0, 1, 2, 3, 4];
        assert_eq!(bracket_axis_i64(&axis, 9), (3, 4, 1.0));
        assert_eq!(bracket_axis_i64(&axis, -9), (0, 1, 0.0));
    }

    /// A download is cached only when every GRIB2 message in it is whole:
    /// the stated length is the length that arrived, and the message ends
    /// `7777`. These are the ways a transfer actually goes wrong — cut off
    /// part way, cut off between messages, or something else entirely.
    #[test]
    fn only_whole_grib2_messages_count_as_an_answer() {
        let u = grib2(UGRD, run_2026(), 3, KATTEGAT_2X3, &U6);
        let v = grib2(VGRD, run_2026(), 3, KATTEGAT_2X3, &U6);
        let both: Vec<u8> = [u.clone(), v.clone()].concat();
        assert!(complete_grib2(&u), "one whole message");
        assert!(complete_grib2(&both), "two whole messages back to back");
        // Cut short by one byte, inside the first and inside the second.
        assert!(!complete_grib2(&u[..u.len() - 1]));
        assert!(!complete_grib2(&both[..both.len() - 1]));
        // Cut exactly after the first message's header, mid-way through.
        assert!(!complete_grib2(&both[..u.len() + 16]));
        // The end marker damaged.
        let mut bad = u.clone();
        let n = bad.len();
        bad[n - 1] = b'8';
        assert!(!complete_grib2(&bad));
        // GRIB edition 1 is not what was asked for.
        let mut ed1 = u.clone();
        ed1[7] = 1;
        assert!(!complete_grib2(&ed1));
        // A stated length longer than what arrived, or too short to hold the
        // header and the end marker.
        let mut long = u.clone();
        long[8..16].copy_from_slice(&((u.len() as u64) + 1).to_be_bytes());
        assert!(!complete_grib2(&long));
        let mut tiny = b"GRIB\0\0\0\x02".to_vec();
        tiny.extend_from_slice(&19u64.to_be_bytes());
        tiny.extend_from_slice(b"7777");
        assert!(!complete_grib2(&tiny));
        // Trailing junk after whole messages.
        assert!(!complete_grib2(&[u.clone(), b"<html>".to_vec()].concat()));
        // Nothing, or an HTML error page.
        assert!(!complete_grib2(b""));
        assert!(!complete_grib2(b"<html><body>404</body></html>"));
    }

    /// The smallest message the format allows — header and end marker, 20
    /// octets — is whole; the check is about truncation, not about size.
    #[test]
    fn the_smallest_whole_message_is_whole() {
        let mut m = b"GRIB\0\0\0\x02".to_vec();
        m.extend_from_slice(&20u64.to_be_bytes());
        m.extend_from_slice(b"7777");
        assert_eq!(m.len(), 20);
        assert!(complete_grib2(&m));
    }

    /// −180° and 180° are one meridian. A GFS grid decodes it as 180 (its
    /// longitudes come 0..360, folded by `normalize_lon`), so a boat at
    /// −180.0 must find that node.
    #[test]
    fn the_antimeridian_is_one_meridian() {
        let f = GribForecast::synthetic(
            vec![0],
            vec![-16.0, -17.0],
            vec![normalize_lon(179.75), normalize_lon(180.0)],
            vec![vec![1.0; 4]],
            vec![vec![1.0; 4]],
        );
        assert!(f.covers(-16.5, 180.0));
        assert!(f.covers(-16.5, -180.0), "−180° is the grid's 180° edge");
    }

    // ---- properties -----------------------------------------------------

    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Bilinear-then-linear interpolation is a weighted mean with
        /// non-negative weights: each component stays within the eight
        /// samples around it (four corners × two steps).
        #[test]
        fn interpolated_components_stay_within_their_samples(
            u in prop::collection::vec(-40.0f32..40.0, 8),
            v in prop::collection::vec(-40.0f32..40.0, 8),
            descending in any::<bool>(),
            lat in 56.0..=57.0f64,
            lon in 11.0..=12.0f64,
            t in 0..=HOUR_MS,
        ) {
            let lats = if descending { vec![57.0, 56.0] } else { vec![56.0, 57.0] };
            let f = GribForecast::synthetic(
                vec![0, HOUR_MS],
                lats,
                vec![11.0, 12.0],
                vec![u[..4].to_vec(), u[4..].to_vec()],
                vec![v[..4].to_vec(), v[4..].to_vec()],
            );
            let (iu, iv) = f.uv(lat, lon, t);
            let range = |s: &[f32]| (
                s.iter().map(|&x| x as f64).fold(f64::INFINITY, f64::min),
                s.iter().map(|&x| x as f64).fold(f64::NEG_INFINITY, f64::max),
            );
            let (ulo, uhi) = range(&u);
            let (vlo, vhi) = range(&v);
            prop_assert!(iu >= ulo - 1e-9 && iu <= uhi + 1e-9, "u {iu} not in [{ulo}, {uhi}]");
            prop_assert!(iv >= vlo - 1e-9 && iv <= vhi + 1e-9, "v {iv} not in [{vlo}, {vhi}]");
        }

        /// §8.5: winds from 340–360° on the west column and 0–20° on the
        /// east blend, on components, into a northerly — within the arc
        /// between them, never the ~180° that averaging the angles gives.
        #[test]
        fn direction_is_continuous_across_north(
            west_from in 340.0..360.0f64,
            east_from in 0.0..20.0f64,
            west_ms in 1.0..30.0f64,
            east_ms in 1.0..30.0f64,
            lon in 11.0..=12.0f64,
        ) {
            let comp = |from: f64, s: f64| {
                let r = from.to_radians();
                ((-s * r.sin()) as f32, (-s * r.cos()) as f32)
            };
            let (uw, vw) = comp(west_from, west_ms);
            let (ue, ve) = comp(east_from, east_ms);
            let f = GribForecast::synthetic(
                vec![0],
                vec![57.0, 56.0],
                vec![11.0, 12.0],
                vec![vec![uw, ue, uw, ue]],
                vec![vec![vw, ve, vw, ve]],
            );
            let (dir, _) = f.sample(56.5, lon, 0).unwrap();
            let off_north = dir.min(360.0 - dir);
            prop_assert!(off_north <= 20.0 + 1e-6, "{west_from}° & {east_from}° gave {dir}°");
        }

        /// Ascending or descending, inside or beyond: the two indices are
        /// neighbours, the fraction is in [0, 1], and together they land
        /// on the value (or on the nearer end, beyond the axis).
        #[test]
        fn bracket_axis_brackets_the_value(
            start in -170.0..170.0f64,
            step in prop_oneof![0.1..2.0f64, -2.0..-0.1f64],
            n in 2usize..12,
            pick in -0.3..1.3f64,
        ) {
            let axis: Vec<f64> = (0..n).map(|i| start + step * i as f64).collect();
            let (lo, hi) = (axis[0].min(axis[n - 1]), axis[0].max(axis[n - 1]));
            let x = lo + (hi - lo) * pick;
            let (i0, i1, f) = bracket_axis(&axis, x);
            prop_assert_eq!(i1, i0 + 1);
            prop_assert!(i1 < n);
            prop_assert!((0.0..=1.0).contains(&f), "{}", f);
            let landed = axis[i0] + f * (axis[i1] - axis[i0]);
            prop_assert!((landed - x.clamp(lo, hi)).abs() < 1e-9, "{} vs {}", landed, x);
        }

        #[test]
        fn geobox_around_contains_every_point_with_its_margin(
            pts in prop::collection::vec((-80.0..80.0f64, -170.0..170.0f64), 1..8),
            margin in 0.0..5.0f64,
        ) {
            let pts: Vec<_> = pts.into_iter().map(|(a, o)| crate::geo::LatLon::new(a, o)).collect();
            let b = GeoBox::around(&pts, margin);
            for p in &pts {
                prop_assert!(b.south <= p.lat - margin + 1e-9 && b.north >= p.lat + margin - 1e-9);
                prop_assert!(b.west <= p.lon - margin + 1e-9 && b.east >= p.lon + margin - 1e-9);
            }
            // And no bigger than that: some point sits on each inner edge.
            prop_assert!(pts.iter().any(|p| (p.lat - margin - b.south).abs() < 1e-9));
            prop_assert!(pts.iter().any(|p| (p.lat + margin - b.north).abs() < 1e-9));
            prop_assert!(pts.iter().any(|p| (p.lon - margin - b.west).abs() < 1e-9));
            prop_assert!(pts.iter().any(|p| (p.lon + margin - b.east).abs() < 1e-9));
        }
    }
}
