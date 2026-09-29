//! Forecast wind: GRIB2 download, decode, and interpolation.
//!
//! The source is NOAA's NOMADS *filter* endpoint for GFS 0.25° — free, no
//! key, and it subsets server-side: navcore asks for exactly two fields
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
    /// Forecast step spacing navcore requests.
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
        sweep_cache(cache_dir);

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
        let mut fh = step;
        while fh <= hours_ahead {
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
        let mut fh = step;
        while fh <= hours_ahead {
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
    if bytes.len() < 100 || !bytes.starts_with(b"GRIB") {
        return Err(GribError::Download(format!(
            "not a GRIB answer ({} bytes)",
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

/// Old forecast files are dead weight; anything past five days is deleted.
fn sweep_cache(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(5 * 86_400);
    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata() {
            if meta.modified().map(|m| m < cutoff).unwrap_or(false) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

fn normalize_lon(lon: f64) -> f64 {
    let mut l = lon % 360.0;
    if l > 180.0 {
        l -= 360.0;
    } else if l < -180.0 {
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
        let (x, y) = Projection::to_mercator(56.5, 11.5);
        let (u, _) = f.uv(56.5, 11.5, 0);
        let _ = (x, y);
        assert!((u - 6.0).abs() < 1e-6, "{u}");
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
}
