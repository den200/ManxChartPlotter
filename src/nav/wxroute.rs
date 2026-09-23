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
    /// The safe corridor, found first: it decides where the boat may go,
    /// and the forecast decides how fast she gets there.
    pub corridor: &'a autoroute::Corridor,
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

/// Open water, for the weather router: at least this far from anything the
/// chart calls unsafe, all along the corridor.
const OPEN_WATER_NM: f64 = 0.5;
/// An open stretch shorter than this is not worth an isochrone: it is timed
/// along the corridor with the rest of the confined water around it.
const MIN_OPEN_STRETCH_NM: f64 = 5.0;

/// Plan a sailing passage along a corridor.
///
/// The isochrone method is time-optimal where the water is open, and blind
/// in the rest of it: its steps are miles long and straight, and a dredged
/// channel or a harbour entrance is narrower than one of them. So the work
/// is split along the corridor the chart search already proved safe —
/// open stretches are sailed by isochrones, free to leave the corridor for
/// better wind; confined stretches follow it, timed through the polar with
/// the wind, current and sea of the hour they are sailed in. A route
/// therefore exists whenever safe water connects the ends, and is never
/// worse than the corridor sailed well.
pub fn plan(job: &WxJob<'_>, name: &str) -> Result<WxPlanned, WxError> {
    let c = job.corridor;
    let mut warnings = c.warnings.clone();
    let depart_ms = job.depart.timestamp_millis();

    let base = IsochroneInput {
        polar: job.polar,
        wind: job.forecast,
        grid: Some(&c.grid),
        start: c.start,
        finish: c.finish,
        depart_ms,
        config: job.config,
        dt_s: job.config.dt_coastal_s,
        offing_min_m: job.safety.offing_min_nm * crate::geo::METRES_PER_NM,
        current: job
            .currents
            .map(|c| c as &dyn isochrone::CurrentField),
        waves: job.waves.map(|w| w as &dyn isochrone::WaveField),
        // Gates await a UI.
        gates: &[],
    };

    let mut points = vec![isochrone::PlannedPoint {
        pos: c.start,
        time_ms: depart_ms,
        heading_deg: 0.0,
        twa_deg: 0.0,
        tws_kt: 0.0,
        stw_kt: 0.0,
        tack: super::model::TackState::Starboard,
        motoring: false,
    }];
    let mut t = depart_ms;
    let nm = crate::geo::METRES_PER_NM * c.k;
    let mut stretch_count = (0usize, 0usize);
    for stretch in stretches(c, OPEN_WATER_NM * nm, MIN_OPEN_STRETCH_NM * nm) {
        if isochrone::this_plan_cancelled() {
            return Err(WxError::Engine(isochrone::IsoError::Cancelled));
        }
        let (from, to) = (stretch.pts[0], *stretch.pts.last().unwrap());
        let mut sailed = false;
        if stretch.open {
            let len_nm = polyline_len(&stretch.pts) / nm;
            // Π₃, bounded: the configured step, but short enough that the
            // stretch takes several rings — a step as long as the stretch
            // has nothing to choose between.
            let configured = if len_nm < 60.0 {
                job.config.dt_coastal_s
            } else {
                job.config.dt_offshore_s
            };
            let dt_s = configured
                .min((len_nm / 6.0 / 8.0 * 3600.0) as u32)
                .max(600);
            let input = IsochroneInput {
                start: to_latlon(from),
                finish: to_latlon(to),
                depart_ms: t,
                dt_s,
                ..base
            };
            match isochrone::solve(&input) {
                Ok(iso) => {
                    t = iso.arrival_ms;
                    points.extend(iso.points.into_iter().skip(1));
                    sailed = true;
                    stretch_count.0 += 1;
                }
                Err(isochrone::IsoError::Cancelled) => {
                    return Err(WxError::Engine(isochrone::IsoError::Cancelled));
                }
                Err(e) => {
                    log::info!("wxroute: open stretch of {len_nm:.1} nm not solved: {e}");
                    warnings.push(format!(
                        "no better way was found across {len_nm:.0} NM of open water \
                         near {}; that stretch follows the charted route",
                        crate::nav::pointfx::place_label(to_latlon(from).lat, to_latlon(from).lon)
                    ));
                }
            }
        }
        if !sailed {
            let timed = time_polyline(&base, &stretch.pts, t, c.k, &mut warnings);
            if let Some(last) = timed.last() {
                t = last.time_ms;
            }
            points.extend(timed);
            stretch_count.1 += 1;
        }
    }
    log::info!(
        "wxroute: {} open stretch(es) by isochrone, {} along the corridor",
        stretch_count.0,
        stretch_count.1
    );
    // Stretch boundaries fall wherever the water changes, so a boundary can
    // leave a leg of a few metres. A point closer than a cable to the one
    // before goes, where the leg that replaces it is itself clear.
    let cable = 0.1 * nm;
    let mut i = 1;
    while i + 1 < points.len() {
        let (a, b, n) = (
            merc(points[i - 1].pos),
            merc(points[i].pos),
            merc(points[i + 1].pos),
        );
        if (b[0] - a[0]).hypot(b[1] - a[1]) < cable && c.grid.segment_clear_by(a, n, &c.params) {
            points.remove(i);
        } else {
            i += 1;
        }
    }
    // The isochrones may leave the corridor for better wind, and they see
    // only what is forbidden, not what asks for care.
    if !warnings.iter().any(|w| w == autoroute::CAUTION_AREA_NOTE)
        && points
            .windows(2)
            .any(|w| c.grid.segment_touches_soft(merc(w[0].pos), merc(w[1].pos)))
    {
        warnings.push(autoroute::CAUTION_AREA_NOTE.into());
    }
    if let Ok(out) = std::env::var("NAVCORE_ROUTE_DEBUG") {
        let sailed: Vec<[f64; 2]> = points.iter().map(|p| merc(p.pos)).collect();
        c.grid
            .debug_png(&sailed, std::path::Path::new(&format!("{out}.sailed.png")), 1600);
    }
    let iso = Isoroute2 {
        points,
        arrival_ms: t,
    };

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

/// The assembled plan: [`isochrone::PlannedPoint`]s start to finish.
struct Isoroute2 {
    points: Vec<isochrone::PlannedPoint>,
    arrival_ms: i64,
}

/// A piece of the corridor: open water (sailed by isochrones) or not.
struct Stretch {
    pts: Vec<[f64; 2]>,
    open: bool,
}

fn polyline_len(pts: &[[f64; 2]]) -> f64 {
    pts.windows(2)
        .map(|w| (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]))
        .sum()
}

fn to_latlon(p: [f64; 2]) -> LatLon {
    let (lat, lon) = Projection::to_wgs84(p[0], p[1]);
    LatLon::new(lat, lon)
}

/// Split the corridor where it passes from open water to confined and back.
///
/// Walked a cell at a time: a sample is open when the chart has nothing
/// unsafe within `open_m` of it. Open runs shorter than `min_open_m` count
/// as confined — an isochrone across a few miles gains nothing and risks
/// the most. Every boundary lies on the corridor, so the pieces join.
fn stretches(c: &autoroute::Corridor, open_m: f64, min_open_m: f64) -> Vec<Stretch> {
    // (position, index of the corridor vertex it follows, open, arc length)
    let mut samples: Vec<([f64; 2], usize, bool, f64)> = Vec::new();
    let mut arc = 0.0;
    for (vi, w) in c.points.windows(2).enumerate() {
        let len = (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]);
        let n = ((len / c.grid.res).ceil() as usize).max(1);
        for i in 0..n {
            let f = i as f64 / n as f64;
            let p = [w[0][0] + (w[1][0] - w[0][0]) * f, w[0][1] + (w[1][1] - w[0][1]) * f];
            let open = c
                .grid
                .cell_of(p)
                .is_some_and(|(x, y)| c.grid.hazard_distance(x, y) >= open_m);
            samples.push((p, vi, open, arc + len * f));
        }
        arc += len;
    }
    let last = *c.points.last().unwrap();
    samples.push((last, c.points.len() - 1, false, arc));
    // The ends are confined by definition: a harbour, or a mark inshore.
    if let Some(first) = samples.first_mut() {
        first.2 = false;
    }

    // Runs of equal openness, as sample index ranges.
    let mut runs: Vec<(usize, usize, bool)> = Vec::new();
    for (i, s) in samples.iter().enumerate() {
        match runs.last_mut() {
            Some(r) if r.2 == s.2 => r.1 = i,
            _ => runs.push((i, i, s.2)),
        }
    }
    for r in runs.iter_mut() {
        if r.2 && samples[r.1].3 - samples[r.0].3 < min_open_m {
            r.2 = false;
        }
    }
    let mut merged: Vec<(usize, usize, bool)> = Vec::new();
    for r in runs {
        match merged.last_mut() {
            Some(m) if m.2 == r.2 => m.1 = r.1,
            _ => merged.push(r),
        }
    }

    // Each run becomes a polyline: its first sample, the corridor's own
    // vertices inside it, and the next run's first sample — so consecutive
    // stretches share their boundary point.
    let mut out = Vec::new();
    for (ri, &(a, b, open)) in merged.iter().enumerate() {
        let end = if ri + 1 < merged.len() { merged[ri + 1].0 } else { b };
        let mut pts = vec![samples[a].0];
        for v in (samples[a].1 + 1)..=samples[end].1 {
            if v < c.points.len() && samples[end].3 > 0.0 {
                let pv = c.points[v];
                if pv != samples[end].0 {
                    pts.push(pv);
                }
            }
        }
        pts.push(samples[end].0);
        pts.dedup_by(|p, q| (p[0] - q[0]).hypot(p[1] - q[1]) < 1.0);
        if pts.len() >= 2 {
            out.push(Stretch { pts, open });
        }
    }
    out
}

/// Sail a polyline in the order given, timing each leg through the polar
/// with the wind, current and sea met on it. Legs are taken two miles at a
/// time so the weather moves on as the boat does; a leg the wind heads is
/// timed as the beat (or the run) it becomes, but drawn as the leg, since
/// the tacks themselves are the helm's to place in confined water.
///
/// Returns one point per vertex after the first. Never fails: where the
/// polar has nothing — calm, or no data — the leg is timed under engine at
/// the configured speed (or 4 kn), and the route says so.
fn time_polyline(
    base: &IsochroneInput<'_>,
    pts: &[[f64; 2]],
    depart_ms: i64,
    _k_mid: f64,
    warnings: &mut Vec<String>,
) -> Vec<isochrone::PlannedPoint> {
    use super::model::TackState;
    const CHUNK_NM: f64 = 2.0;
    const FALLBACK_MOTOR_KT: f64 = 4.0;
    // Timing, not deciding: a corridor leg is sailed whatever the limits
    // say, and a leg over them is reported rather than dropped.
    let mut relaxed = base.config.clone();
    relaxed.max_tws_kt = f64::INFINITY;
    relaxed.max_wave_m = f64::INFINITY;
    let input = IsochroneInput {
        grid: None,
        config: &relaxed,
        ..*base
    };
    let tack_pen = base.config.tack_penalty_day_s as i64 * 1000;
    let gybe_pen = base.config.gybe_penalty_day_s as i64 * 1000;

    let mut out = Vec::new();
    let mut t = depart_ms;
    let mut tack = TackState::Starboard;
    let (mut warned_calm, mut warned_limit) = (false, false);
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let (lat, _) = Projection::to_wgs84(a[0], a[1]);
        let k = 1.0 / lat.to_radians().cos();
        let len = (b[0] - a[0]).hypot(b[1] - a[1]);
        let chunks = ((len / (CHUNK_NM * crate::geo::METRES_PER_NM * k)).ceil() as usize).max(1);
        let mut first: Option<(f64, f64, f64, TackState, bool)> = None;
        for i in 0..chunks {
            let p = lerp(a, b, i as f64 / chunks as f64);
            let q = lerp(a, b, (i + 1) as f64 / chunks as f64);
            let (wind_from, tws) = base.wind.wind(p, t);
            let hs = base.waves.and_then(|w| w.wave_height_m(p, t));
            if !warned_limit
                && (tws > base.config.max_tws_kt
                    || hs.is_some_and(|h| h > base.config.max_wave_m))
            {
                warned_limit = true;
                let at = to_latlon(p);
                warnings.push(format!(
                    "wind or sea over your limits in confined water near {} — \
                     check the forecast before committing",
                    crate::nav::pointfx::place_label(at.lat, at.lon)
                ));
            }
            let wave_f = match hs {
                Some(h) if h > 0.0 && base.config.wave_penalty_coef > 0.0 => {
                    1.0 / (1.0 + base.config.wave_penalty_coef * h * h)
                }
                _ => 1.0,
            };
            let current = base
                .current
                .map(|c| c.current(p, t))
                .unwrap_or((0.0, 0.0));
            // With the stream folded in first; if the heading-against-drift
            // iteration will not settle (a cross-stream at the edge of the
            // no-go zone), the leg is timed on the wind alone rather than
            // written off as calm.
            let legs = isochrone::closing_legs(
                &input, p, t, tack, q, k, wind_from, tws, current, wave_f, tack_pen, gybe_pen,
            )
            .or_else(|| {
                isochrone::closing_legs(
                    &input, p, t, tack, q, k, wind_from, tws, (0.0, 0.0), wave_f, tack_pen,
                    gybe_pen,
                )
            });
            match legs {
                Some(legs) if !legs.is_empty() => {
                    let l0 = &legs[0];
                    first.get_or_insert((l0.twa_deg, l0.tws_kt, l0.stw_kt, l0.tack, l0.motoring));
                    let end = legs.last().unwrap();
                    tack = end.tack;
                    t = end.end_time_ms;
                }
                _ => {
                    let v = base.config.motor_speed_kt.unwrap_or(FALLBACK_MOTOR_KT);
                    if !warned_calm {
                        warned_calm = true;
                        warnings.push(format!(
                            "no sailing wind for part of the route; that part is timed \
                             at {v:.0} kn under engine"
                        ));
                    }
                    let ground_m = (q[0] - p[0]).hypot(q[1] - p[1]) / k;
                    t += (ground_m / (v * crate::geo::METRES_PER_NM / 3600.0) * 1000.0) as i64;
                    first.get_or_insert((0.0, tws, v, tack, true));
                }
            }
        }
        let (twa, tws, stw, leg_tack, motoring) = first.unwrap_or((0.0, 0.0, 0.0, tack, false));
        let heading = (b[0] - a[0]).atan2(b[1] - a[1]).to_degrees().rem_euclid(360.0);
        out.push(isochrone::PlannedPoint {
            pos: to_latlon(b),
            time_ms: t,
            heading_deg: heading,
            twa_deg: twa,
            tws_kt: tws,
            stw_kt: stw,
            tack: leg_tack,
            motoring,
        });
    }
    out
}

fn lerp(a: [f64; 2], b: [f64; 2], f: f64) -> [f64; 2] {
    [a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f]
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

/// Find the safe corridor between two points, loading the charts around
/// them — and, when the way round lies outside the first box (a peninsula,
/// an island group), searching wider boxes before giving up. The error then
/// says how far afield the search went.
pub fn find_corridor(
    chart_dir: &std::path::Path,
    start: LatLon,
    finish: LatLon,
    safety: &SafetyConfig,
    progress: &mut impl FnMut(String),
) -> Result<autoroute::Corridor, String> {
    let mut searched_nm = 0.0;
    let mut disconnected = false;
    for (attempt, &factor) in autoroute::MARGIN_FACTORS.iter().enumerate() {
        if isochrone::this_plan_cancelled() {
            return Err("cancelled".into());
        }
        if attempt > 0 {
            progress("no way through nearby — searching a wider area".into());
        }
        let found = with_chart_sources(chart_dir, start, finish, factor, |sources| {
            progress(format!("finding safe water across {} chart(s)", sources.len()));
            autoroute::corridor(sources, start, finish, safety, factor)
        })?;
        match found {
            Ok(c) => return Ok(c),
            Err(e @ (autoroute::PlanError::Unreachable | autoroute::PlanError::Disconnected)) => {
                let span_nm = crate::geo::distance_m(start, finish) / crate::geo::METRES_PER_NM;
                searched_nm = (span_nm * factor).max(8.0);
                disconnected = matches!(e, autoroute::PlanError::Disconnected);
                continue;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    if disconnected {
        return Err(autoroute::PlanError::Disconnected.to_string());
    }
    Err(format!(
        "no safe water connects the two points at {:.1} m draft, searching {searched_nm:.0} NM \
         around them — check the draft in the Boat window, and that both points are in charted water",
        safety.draft_m
    ))
}

/// The motor route from a chart directory: the corridor itself, as a route.
pub fn motor_plan_from_chart_dir(
    chart_dir: &std::path::Path,
    start: LatLon,
    finish: LatLon,
    safety: &SafetyConfig,
    mut progress: impl FnMut(String),
) -> Result<autoroute::Planned, String> {
    let corridor = find_corridor(chart_dir, start, finish, safety, &mut progress)?;
    Ok(autoroute::route_from_corridor(&corridor, start, finish))
}

/// The whole job from a chart directory — what both the CLI and the UI
/// button run, on whatever thread they own.
///
/// Charts first: finding the corridor needs no network, and a passage with
/// no safe water should say so before downloading a forecast for it. The
/// forecast is then fetched for the corridor's own area, which may be far
/// wider than the straight line between the ends.
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

    progress("loading charts".into());
    let corridor = find_corridor(chart_dir, start, finish, safety, &mut progress)?;
    let corridor_ll: Vec<LatLon> = corridor.points.iter().map(|p| to_latlon(*p)).collect();
    let area = super::grib::GeoBox::around(&corridor_ll, 0.75);

    progress(format!("fetching {}", super::grib::GribSource::NoaaGfs025.label()));
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

    progress(format!(
        "routing {:.0} NM",
        polyline_len(&corridor.points) / (crate::geo::METRES_PER_NM * corridor.k)
    ));
    let mut planned = plan(
        &WxJob {
            corridor: &corridor,
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
/// use, and the closure scope is the honest way to say so. The box is the
/// same [`autoroute::passage_box`] the grid is built on, so no part of the
/// grid is left to coarser charts than it should be.
pub fn with_chart_sources<T>(
    chart_dir: &std::path::Path,
    start: LatLon,
    finish: LatLon,
    margin_factor: f64,
    f: impl FnOnce(&[ChartSource<'_>]) -> T,
) -> Result<T, String> {
    use crate::cache::CachedDecryptor;
    use crate::decrypt::KeyStore;
    use crate::senc::ChartCatalog;
    use crate::tiles::builder::TileBuilder;
    use std::collections::HashMap;
    use std::sync::Mutex;

    let mut keys = KeyStore::new();
    let _ = keys.load_keylists_in_dir(chart_dir);
    let mut decryptor = CachedDecryptor::open("license");
    let catalog =
        ChartCatalog::from_directory(chart_dir, &keys, &mut decryptor).map_err(|e| e.to_string())?;

    let k = 1.0 / ((start.lat + finish.lat) / 2.0).to_radians().cos();
    let (min, max) = autoroute::passage_box(merc(start), merc(finish), k, margin_factor);

    let chart_cache: crate::tiles::builder::ChartCache = Mutex::new(HashMap::new());
    let coverage_cache: crate::tiles::builder::CoverageCache = Mutex::new(HashMap::new());
    let decryptor = Mutex::new(decryptor);
    let builder = TileBuilder::with_cache(&catalog, &keys, &decryptor, &chart_cache, &coverage_cache);
    let mut loaded = Vec::new();
    let mut skipped = 0usize;
    for info in catalog.charts.iter().filter(|c| {
        c.extent_mercator.min_x <= max[0]
            && c.extent_mercator.max_x >= min[0]
            && c.extent_mercator.min_y <= max[1]
            && c.extent_mercator.max_y >= min[1]
    }) {
        if isochrone::this_plan_cancelled() {
            return Err("cancelled".into());
        }
        match builder.load_chart(info) {
            Ok(chart) => loaded.push((chart, info)),
            Err(e) => {
                skipped += 1;
                log::warn!("route: skipping {}: {e}", info.name);
            }
        }
    }
    if skipped > 0 {
        // Their water stays closed on the grid, which is safe; it may also
        // be why no route is found.
        log::warn!("route: {skipped} chart(s) could not be read and count as land");
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

    /// A corridor 30 km east along 56°N: land close along its first 3 km
    /// (a harbour approach), open sea after.
    fn corridor() -> autoroute::Corridor {
        use crate::nav::autoroute::grid::{Grid, SearchParams};
        let (x0, y0) = Projection::to_mercator(56.0, 11.0);
        let k = 1.0 / 56f64.to_radians().cos();
        let mut grid = Grid::new([x0 - 5_000.0, y0 - 20_000.0], [x0 + 60_000.0, y0 + 20_000.0], 50.0);
        // Shores 300 m either side of the first 5 km of plane.
        let (a, b) = (x0 - 5_000.0, x0 + 5_000.0);
        grid.stamp_triangle([[a, y0 + 300.0], [b, y0 + 300.0], [b, y0 + 3_000.0]], true);
        grid.stamp_triangle([[a, y0 + 300.0], [b, y0 + 3_000.0], [a, y0 + 3_000.0]], true);
        grid.stamp_triangle([[a, y0 - 300.0], [b, y0 - 300.0], [b, y0 - 3_000.0]], true);
        grid.stamp_triangle([[a, y0 - 300.0], [b, y0 - 3_000.0], [a, y0 - 3_000.0]], true);
        grid.finalize();
        let pts = vec![[x0, y0], [x0 + 55_000.0, y0]];
        autoroute::Corridor {
            grid,
            k,
            params: SearchParams::fixed(0.0, 0.0),
            start: to_latlon(pts[0]),
            finish: to_latlon(pts[1]),
            points: pts,
            warnings: Vec::new(),
        }
    }

    /// The corridor splits where the water opens, the pieces join, and only
    /// the open piece is offered to the isochrones.
    #[test]
    fn a_corridor_splits_into_confined_and_open_stretches() {
        let c = corridor();
        let nm = crate::geo::METRES_PER_NM * c.k;
        let parts = stretches(&c, 0.5 * nm, 5.0 * nm);
        assert!(parts.len() >= 2, "{} stretch(es)", parts.len());
        assert!(!parts[0].open, "a harbour approach is confined");
        assert!(parts.iter().any(|p| p.open), "the sea beyond is open");
        for w in parts.windows(2) {
            assert_eq!(*w[0].pts.last().unwrap(), w[1].pts[0], "stretches must join");
        }
        assert_eq!(parts[0].pts[0], c.points[0]);
        assert_eq!(*parts.last().unwrap().pts.last().unwrap(), c.points[1]);
    }

    /// Timing along a corridor: a beam reach at the polar's speed, and a
    /// dead beat timed as the (slower) beat it is — never refused.
    #[test]
    fn corridor_legs_are_timed_through_the_polar() {
        let c = corridor();
        let polar = Polar::parse(DEFAULT_POLAR).unwrap();
        let config = RoutingConfig::default();
        let input = |wind: &'static dyn isochrone::WindField| IsochroneInput {
            polar: &polar,
            wind,
            grid: None,
            start: c.start,
            finish: c.finish,
            depart_ms: 0,
            config: &config,
            dt_s: 3600,
            offing_min_m: 0.0,
            current: None,
            waves: None,
            gates: &[],
        };
        static BEAM: isochrone::ConstantWind = isochrone::ConstantWind { from_deg: 0.0, tws_kt: 12.0 };
        static HEAD: isochrone::ConstantWind = isochrone::ConstantWind { from_deg: 90.0, tws_kt: 12.0 };
        let mut notes = Vec::new();
        let ground_nm = crate::geo::distance_m(c.start, c.finish) / crate::geo::METRES_PER_NM;

        let reach = time_polyline(&input(&BEAM), &c.points, 0, c.k, &mut notes);
        let hours = reach.last().unwrap().time_ms as f64 / 3.6e6;
        let speed = polar.speed_kt(90.0, 12.0).unwrap();
        assert!((hours - ground_nm / speed).abs() < 0.05 * hours, "{hours} h");

        let beat = time_polyline(&input(&HEAD), &c.points, 0, c.k, &mut notes);
        let beat_h = beat.last().unwrap().time_ms as f64 / 3.6e6;
        assert!(beat_h > hours * 1.2, "a beat is slower: {beat_h} vs {hours}");
        assert!(notes.is_empty(), "wind enough to sail: {notes:?}");
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
