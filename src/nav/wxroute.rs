//! Weather routing, end to end: forecast + polar + charts → a Route whose
//! legs carry their plan.
//!
//! This is the M5 orchestration. The pieces stay where they belong — GRIB in
//! [`super::grib`], the engine in [`super::isochrone`], hazards in
//! [`super::autoroute`] — and this module only wires a job: fetch the wind
//! over the passage box, quilt the same hazard grid the M3 router uses (both
//! routers must forbid exactly the same water), solve, and dress the result
//! as an ordinary [`Route`] with a [`LegPlan`] per leg and provenance enough
//! to reproduce it.

use chrono::{DateTime, TimeZone, Utc};

use crate::geo::LatLon;
use crate::render::projection::Projection;

use super::autoroute::{self, ChartSource, SafetyConfig};
use super::grib::{GribForecast, WaveForecast};
use super::isochrone::{self, IsochroneInput};
use super::model::{LegPlan, PointOfSail, Route, RouteProvenance, RoutingConfig, Waypoint};
use super::polar::Polar;

/// The polar navcore ships: a generic ~10 m performance cruiser, for trying
/// the feature before feeding in the real boat's numbers.
pub const DEFAULT_POLAR: &str = include_str!("../../assets/polars/default.pol");

#[derive(Debug)]
pub enum WxError {
    Grib(super::grib::GribError),
    Engine(super::isochrone::IsoError),
    Polar(super::polar::PolarError),
}

impl std::fmt::Display for WxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WxError::Grib(e) => write!(f, "{e}"),
            WxError::Engine(e) => write!(f, "{e}"),
            WxError::Polar(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for WxError {}

pub struct WxJob<'a> {
    pub sources: &'a [ChartSource<'a>],
    pub start: LatLon,
    pub finish: LatLon,
    pub depart: DateTime<Utc>,
    pub polar: &'a Polar,
    /// For provenance: what to call the polar in the route's record.
    pub polar_name: &'a str,
    pub forecast: &'a GribForecast,
    /// Sea state, when the wave fetch succeeded. Waves are a refinement:
    /// a passage plans without them, never without wind.
    pub waves: Option<&'a WaveForecast>,
    /// Surface currents, same rule as waves.
    pub currents: Option<&'a super::currents::CurrentForecast>,
    pub config: &'a RoutingConfig,
    pub safety: &'a SafetyConfig,
}

pub struct WxPlanned {
    pub route: Route,
    pub waypoints: Vec<Waypoint>,
    pub arrival: DateTime<Utc>,
    pub warnings: Vec<String>,
}

pub fn plan(job: &WxJob<'_>, name: &str) -> Result<WxPlanned, WxError> {
    let mut warnings = Vec::new();

    // The same box arithmetic as the M3 router, so the two agree about the
    // world they share.
    let a = merc(job.start);
    let b = merc(job.finish);
    let mid_lat = ((job.start.lat + job.finish.lat) / 2.0).to_radians();
    let k = 1.0 / mid_lat.cos();
    let span = ((b[0] - a[0]).hypot(b[1] - a[1])).max(1_000.0);
    let margin = (span * 0.35).max(15_000.0 * k);
    let min = [a[0].min(b[0]) - margin, a[1].min(b[1]) - margin];
    let max = [a[0].max(b[0]) + margin, a[1].max(b[1]) + margin];

    let grid = (!job.sources.is_empty())
        .then(|| autoroute::build_hazard_grid(job.sources, min, max, job.safety, k));

    // §7.7, same as the M3 router: a departure berth is inside the offing by
    // definition. Nudge both ends to the nearest safe water, say so, never
    // spin.
    let offing_plane = job.safety.offing_min_nm * crate::geo::METRES_PER_NM * k;
    let mut start = job.start;
    let mut finish = job.finish;
    if let Some(g) = grid.as_ref() {
        let nudge_max = 2.0 * crate::geo::METRES_PER_NM * k;
        for (point, which) in [(&mut start, "start"), (&mut finish, "finish")] {
            let m = merc(*point);
            let Some(cell) = g.cell_of(m) else { continue };
            match super::autoroute::grid::nudge(g, cell, offing_plane, nudge_max) {
                Some(free) if free != cell => {
                    let c = g.centre(free.0, free.1);
                    let (lat, lon) = Projection::to_wgs84(c[0], c[1]);
                    *point = LatLon::new(lat, lon);
                    warnings.push(format!("{which} moved offshore to clear hazards and offing"));
                }
                Some(_) => {}
                None => {
                    return Err(WxError::Engine(super::isochrone::IsoError::Unreachable));
                }
            }
        }
    }

    // Π₃: the step should resolve the wind you actually have, not more.
    let span_nm = crate::geo::distance_m(job.start, job.finish) / crate::geo::METRES_PER_NM;
    let dt_s = if span_nm < 60.0 {
        job.config.dt_coastal_s
    } else {
        job.config.dt_offshore_s
    };

    let iso = isochrone::solve(&IsochroneInput {
        polar: job.polar,
        wind: job.forecast,
        grid: grid.as_ref(),
        start,
        finish,
        depart_ms: job.depart.timestamp_millis(),
        config: job.config,
        dt_s,
        offing_min_m: job.safety.offing_min_nm * crate::geo::METRES_PER_NM,
        current: job
            .currents
            .map(|c| c as &dyn isochrone::CurrentField),
        waves: job.waves.map(|w| w as &dyn isochrone::WaveField),
        // Gates await a UI.
        gates: &[],
    })
    .map_err(WxError::Engine)?;

    if let Some((_, last)) = job.forecast.valid_span() {
        if iso.arrival_ms > last {
            warnings.push(
                "the passage outruns the forecast — the last steps sail on its final field"
                    .into(),
            );
        }
    }

    // Dress the engine's points as a Route. Leg i runs point i → i+1, and the
    // engine stores the conditions of a leg on its END point.
    let mut route = Route::new(name);
    let mut waypoints = Vec::new();
    for (i, p) in iso.points.iter().enumerate() {
        let wpname = if i == 0 {
            "Start".to_string()
        } else if i + 1 == iso.points.len() {
            "Finish".to_string()
        } else {
            format!("WX-{i:02}")
        };
        let wp = Waypoint::new(wpname, p.pos.lat, p.pos.lon);
        route.waypoints.push(wp.id);
        waypoints.push(wp);
    }
    {
        let mut set = super::model::WaypointSet::default();
        for wp in &waypoints {
            set.insert(wp.clone());
        }
        route.recompute_legs(&set);
    }
    for (leg, end) in route.legs.iter_mut().zip(iso.points.iter().skip(1)) {
        leg.plan = Some(LegPlan {
            eta: Utc
                .timestamp_millis_opt(end.time_ms)
                .single()
                .unwrap_or(job.depart),
            twa_deg: end.twa_deg,
            tws_kt: end.tws_kt,
            point_of_sail: if end.motoring {
                PointOfSail::Motor
            } else {
                isochrone::point_of_sail(end.twa_deg.abs())
            },
            expected_stw_kt: end.stw_kt,
            tack_state: end.tack,
        });
    }
    route.generated = Some(RouteProvenance {
        generated_at: Utc::now(),
        grib_source: Some({
            let mut s = job.forecast.source.label().to_string();
            if job.waves.is_some() {
                s.push_str(" + waves");
            }
            if job.currents.is_some() {
                s.push_str(" + currents");
            }
            s
        }),
        grib_run: Some(job.forecast.run),
        polar: job.polar_name.to_string(),
        engine_params: job.config.clone(),
    });

    Ok(WxPlanned {
        route,
        waypoints,
        arrival: Utc
            .timestamp_millis_opt(iso.arrival_ms)
            .single()
            .unwrap_or(job.depart),
        warnings,
    })
}

/// §8.4's re-route hysteresis: adopt a new plan only when it earns its keep.
///
/// Dead-nose wind makes port-first and starboard-first exactly degenerate, so
/// a fresh forecast can flip the whole route for a 0.1 % gain — and a route
/// that flips is a route the crew stops trusting. The exception is spelled
/// out by the spec: a plan that has become *illegal* under the new forecast
/// is replaced regardless.
pub fn should_adopt(
    new_arrival: DateTime<Utc>,
    current_arrival: DateTime<Utc>,
    depart: DateTime<Utc>,
    margin: f64,
    current_violates_hard_constraint: bool,
) -> bool {
    if current_violates_hard_constraint {
        return true;
    }
    let current = (current_arrival - depart).num_milliseconds().max(1) as f64;
    let new = (new_arrival - depart).num_milliseconds() as f64;
    new < current * (1.0 - margin)
}

/// The whole job from a chart directory — what both the CLI and the UI
/// button run, on whatever thread they own. Loads only the charts touching
/// the passage box; the decryptor's disk cache makes repeats cheap.
#[allow(clippy::too_many_arguments)]
pub fn plan_from_chart_dir(
    chart_dir: &std::path::Path,
    start: LatLon,
    finish: LatLon,
    hours: u32,
    grib_cache: &std::path::Path,
    polar_text: &str,
    polar_name: &str,
    config: &RoutingConfig,
    safety: &SafetyConfig,
    use_waves: bool,
    use_currents: bool,
    name: &str,
    mut progress: impl FnMut(String),
) -> Result<WxPlanned, String> {
    let polar = Polar::parse(polar_text).map_err(|e| e.to_string())?;

    progress(format!("fetching {}", super::grib::GribSource::NoaaGfs025.label()));
    let area = super::grib::GeoBox::around(&[start, finish], 1.0);
    let forecast = GribForecast::fetch(
        super::grib::GribSource::NoaaGfs025,
        area,
        hours,
        grib_cache,
        &mut progress,
    )
    .map_err(|e| e.to_string())?;

    // Waves are a refinement, so their failure is a warning, not an error:
    // the wave file for a run sometimes lags the atmosphere by an hour.
    let mut wave_warning = None;
    let waves = if use_waves {
        progress("fetching waves".into());
        match WaveForecast::fetch(
            super::grib::GribSource::NoaaGfs025,
            area,
            hours,
            grib_cache,
            &mut progress,
        ) {
            Ok(w) => Some(w),
            Err(e) => {
                wave_warning = Some(format!("waves unavailable, routing on wind alone: {e}"));
                None
            }
        }
    } else {
        None
    };

    let mut current_warning = None;
    let currents = if use_currents {
        progress("fetching currents".into());
        match super::currents::CurrentForecast::fetch(area, hours) {
            Ok(c) => Some(c),
            Err(e) => {
                current_warning = Some(format!("currents unavailable, routing without: {e}"));
                None
            }
        }
    } else {
        None
    };

    progress("loading charts".into());
    let mut planned = with_chart_sources(chart_dir, start, finish, |sources| {
        progress(format!("routing across {} chart(s)", sources.len()));
        plan(
            &WxJob {
                sources,
                start,
                finish,
                depart: Utc::now(),
                polar: &polar,
                polar_name,
                forecast: &forecast,
                waves: waves.as_ref(),
                currents: currents.as_ref(),
                config,
                safety,
            },
            name,
        )
    })?
    .map_err(|e| e.to_string())?;
    if let Some(w) = wave_warning {
        planned.warnings.push(w);
    }
    if let Some(w) = current_warning {
        planned.warnings.push(w);
    }
    Ok(planned)
}

/// Load every chart touching the passage box and hand the sources to `f`.
///
/// A callback rather than a return value because the sources borrow the
/// catalog, the keys and the decryptor: the whole chain has to outlive the
/// use, and the closure scope is the honest way to say so. Shared by the
/// weather router and the plain (motor) router so both plan on exactly the
/// same charts.
pub fn with_chart_sources<T>(
    chart_dir: &std::path::Path,
    start: LatLon,
    finish: LatLon,
    f: impl FnOnce(&[ChartSource<'_>]) -> T,
) -> Result<T, String> {
    use crate::cache::CachedDecryptor;
    use crate::decrypt::{ChartDecryptor, KeyStore};
    use crate::senc::ChartCatalog;
    use crate::tiles::builder::TileBuilder;
    use std::collections::HashMap;
    use std::sync::Mutex;

    let mut keys = KeyStore::new();
    let _ = keys.load_keylists_in_dir(chart_dir);
    let base = ChartDecryptor::new("license").map_err(|e| e.to_string())?;
    let mut decryptor = CachedDecryptor::new(base);
    let catalog =
        ChartCatalog::from_directory(chart_dir, &keys, &mut decryptor).map_err(|e| e.to_string())?;

    let (ax, ay) = crate::tiles::latlon_to_mercator(start.lat, start.lon);
    let (bx, by) = crate::tiles::latlon_to_mercator(finish.lat, finish.lon);
    let span = ((bx - ax).hypot(by - ay)).max(1_000.0);
    let margin = (span * 0.35).max(15_000.0);
    let (min_x, max_x) = (ax.min(bx) - margin, ax.max(bx) + margin);
    let (min_y, max_y) = (ay.min(by) - margin, ay.max(by) + margin);

    let chart_cache: crate::tiles::builder::ChartCache = Mutex::new(HashMap::new());
    let coverage_cache: crate::tiles::builder::CoverageCache = Mutex::new(HashMap::new());
    let decryptor = Mutex::new(decryptor);
    let builder = TileBuilder::with_cache(&catalog, &keys, &decryptor, &chart_cache, &coverage_cache);
    let mut loaded = Vec::new();
    for info in catalog.charts.iter().filter(|c| {
        c.extent_mercator.min_x <= max_x
            && c.extent_mercator.max_x >= min_x
            && c.extent_mercator.min_y <= max_y
            && c.extent_mercator.max_y >= min_y
    }) {
        match builder.load_chart(info) {
            Ok(chart) => loaded.push((chart, info)),
            Err(e) => log::warn!("route: skipping {}: {e}", info.name),
        }
    }
    let sources: Vec<ChartSource> = loaded
        .iter()
        .map(|(data, info)| ChartSource { data, info })
        .collect();
    Ok(f(&sources))
}

fn merc(p: LatLon) -> [f64; 2] {
    let (x, y) = Projection::to_mercator(p.lat, p.lon);
    [x, y]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_polar_parses_and_sails() {
        let p = Polar::parse(DEFAULT_POLAR).expect("the built-in polar is valid");
        assert!(p.speed_kt(90.0, 12.0).unwrap() > 6.0);
        assert!(p.speed_kt(30.0, 12.0).is_none(), "no-go below 45");
        let vmg = p.vmg_at(12.0);
        assert!(vmg.beat_vmg_kt > 3.0 && vmg.run_vmg_kt > 4.0);
    }

    #[test]
    fn hysteresis_holds_the_course_for_marginal_gains() {
        let depart = Utc.timestamp_millis_opt(0).single().unwrap();
        let t = |h: f64| Utc.timestamp_millis_opt((h * 3.6e6) as i64).single().unwrap();
        // 1 % faster: not adopted at a 2 % margin.
        assert!(!should_adopt(t(9.9), t(10.0), depart, 0.02, false));
        // 5 % faster: adopted.
        assert!(should_adopt(t(9.5), t(10.0), depart, 0.02, true) );
        assert!(should_adopt(t(9.5), t(10.0), depart, 0.02, false));
        // Slower: never adopted on merit…
        assert!(!should_adopt(t(10.5), t(10.0), depart, 0.02, false));
        // …but always when the current plan has become illegal.
        assert!(should_adopt(t(10.5), t(10.0), depart, 0.02, true));
    }
}
