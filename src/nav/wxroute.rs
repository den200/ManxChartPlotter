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

/// The polar Manx ships: a generic ~10 m performance cruiser, for trying
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
    if let Ok(out) = std::env::var("MANX_ROUTE_DEBUG") {
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
        // The engine clears its legs along straight Mercator segments.
        route.make_legs_rhumb(&set);
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
    // No tack until the first leg is sailed: leaving on either is free.
    let mut tack: Option<TackState> = None;
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
                    tack = Some(end.tack);
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
                    first.get_or_insert((0.0, tws, v, tack.unwrap_or(TackState::Starboard), true));
                }
            }
        }
        let (twa, tws, stw, leg_tack, motoring) =
            first.unwrap_or((0.0, 0.0, 0.0, tack.unwrap_or(TackState::Starboard), false));
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
        ChartCatalog::from_chart_folder(chart_dir, &keys, &mut decryptor).map_err(|e| e.to_string())?;

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

    /// Timing along a corridor: a beam reach at the polar's speed, a dead
    /// beat at the beat's VMG and a dead run at the run's — never refused.
    /// The beat and the run pin the wind's "from": swapped, each would be
    /// timed as the other.
    #[test]
    fn corridor_legs_are_timed_through_the_polar() {
        let c = corridor();
        let polar = Polar::parse(DEFAULT_POLAR).unwrap();
        // No manoeuvre penalties: the pure geometry of each point of sail.
        let config = RoutingConfig {
            tack_penalty_day_s: 0,
            gybe_penalty_day_s: 0,
            ..RoutingConfig::default()
        };
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
        static ASTERN: isochrone::ConstantWind = isochrone::ConstantWind { from_deg: 270.0, tws_kt: 12.0 };
        let mut notes = Vec::new();
        let ground_nm = crate::geo::distance_m(c.start, c.finish) / crate::geo::METRES_PER_NM;

        let reach = time_polyline(&input(&BEAM), &c.points, 0, c.k, &mut notes);
        let hours = reach.last().unwrap().time_ms as f64 / 3.6e6;
        let speed = polar.speed_kt(90.0, 12.0).unwrap();
        assert!((hours - ground_nm / speed).abs() < 0.05 * hours, "{hours} h");

        let vmg = polar.vmg_at(12.0);
        let beat = time_polyline(&input(&HEAD), &c.points, 0, c.k, &mut notes);
        let beat_h = beat.last().unwrap().time_ms as f64 / 3.6e6;
        let expected = ground_nm / vmg.beat_vmg_kt as f64;
        assert!((beat_h - expected).abs() < 0.03 * expected, "beat {beat_h} h, expected {expected} h");

        let run = time_polyline(&input(&ASTERN), &c.points, 0, c.k, &mut notes);
        let run_h = run.last().unwrap().time_ms as f64 / 3.6e6;
        let expected = ground_nm / vmg.run_vmg_kt as f64;
        assert!((run_h - expected).abs() < 0.03 * expected, "run {run_h} h, expected {expected} h");
        assert!(
            (beat_h - run_h).abs() > 0.1 * run_h,
            "the fixture cannot tell a beat from a run"
        );
        assert!(notes.is_empty(), "wind enough to sail: {notes:?}");
    }

    /// The whole job on a synthetic forecast: the route runs from the
    /// corridor's start to its finish, never across the charted shore, and
    /// every leg carries a plan whose ETA advances to the arrival.
    #[test]
    fn a_plan_is_a_route_with_a_plan_on_every_leg() {
        let c = corridor();
        let polar = Polar::parse(DEFAULT_POLAR).unwrap();
        let config = RoutingConfig::default();
        let safety = SafetyConfig::default();
        // 12 kn from the north everywhere, for six hours: the air moves
        // south, so v is negative.
        let ms = 12.0 * 1852.0 / 3600.0;
        let forecast = GribForecast::synthetic(
            vec![0, 6 * 3_600_000],
            vec![57.0, 55.0],
            vec![10.0, 13.0],
            vec![vec![0.0; 4], vec![0.0; 4]],
            vec![vec![-ms as f32; 4], vec![-ms as f32; 4]],
        );
        let depart = Utc.timestamp_millis_opt(0).single().unwrap();
        let planned = plan(
            &WxJob {
                corridor: &c,
                depart,
                polar: &polar,
                polar_name: "default",
                forecast: &forecast,
                waves: None,
                currents: None,
                config: &config,
                safety: &safety,
            },
            "test",
        )
        .expect("a beam reach along open water");

        let route = &planned.route;
        let set = {
            let mut set = crate::nav::model::WaypointSet::default();
            for wp in &planned.waypoints {
                set.insert(wp.clone());
            }
            set
        };
        let pos = |id| set.get(id).unwrap().position;
        assert!(crate::geo::distance_m(pos(route.waypoints[0]), c.start) < 1.0);
        assert!(crate::geo::distance_m(pos(*route.waypoints.last().unwrap()), c.finish) < 1.0);
        assert_eq!(route.legs.len(), route.waypoints.len() - 1);
        for leg in &route.legs {
            let (a, b) = (merc(pos(leg.from)), merc(pos(leg.to)));
            assert!(c.grid.segment_clear(a, b, 0.0), "a leg crosses the shore");
        }
        let mut eta = depart;
        for leg in &route.legs {
            let p = leg.plan.as_ref().expect("every leg is planned");
            assert!(p.eta > eta, "ETAs must advance");
            eta = p.eta;
            // North wind, heading east: a beam reach on port.
            assert!((p.twa_deg - 90.0).abs() < 15.0, "twa {}", p.twa_deg);
            assert!((p.tws_kt - 12.0).abs() < 0.01);
        }
        assert_eq!(eta, planned.arrival);
        let hours = (planned.arrival - depart).num_milliseconds() as f64 / 3.6e6;
        let ground_nm = crate::geo::distance_m(c.start, c.finish) / crate::geo::METRES_PER_NM;
        let reach = ground_nm / polar.speed_kt(90.0, 12.0).unwrap();
        assert!(hours > 0.95 * reach && hours < 1.2 * reach, "{hours} h vs {reach} h on the beam");
        let provenance = route.generated.as_ref().expect("a generated route says so");
        assert_eq!(provenance.polar, "default");
        assert!(planned.warnings.is_empty(), "{:?}", planned.warnings);
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

    // ---- fixtures for the tests below ----

    use crate::nav::autoroute::grid::{Grid, SearchParams};
    use crate::nav::isochrone::{ConstantWaves, ConstantWind};
    use proptest::prelude::*;

    const H: i64 = 3_600_000;

    fn corridor_on(grid: Grid, k: f64, pts: Vec<[f64; 2]>) -> autoroute::Corridor {
        autoroute::Corridor {
            grid,
            k,
            params: SearchParams::fixed(0.0, 0.0),
            start: to_latlon(pts[0]),
            finish: to_latlon(*pts.last().unwrap()),
            points: pts,
            warnings: Vec::new(),
        }
    }

    /// A corridor `len` plane metres east along 56°N, with shores 300 m
    /// either side of it over each `(from, to)` span (plane metres from the
    /// start) — [`corridor`], with the harbours where a test wants them.
    fn shore_corridor(len: f64, shores: &[(f64, f64)]) -> autoroute::Corridor {
        let (x0, y0) = Projection::to_mercator(56.0, 11.0);
        let k = 1.0 / 56f64.to_radians().cos();
        let mut grid =
            Grid::new([x0 - 5_000.0, y0 - 20_000.0], [x0 + len + 5_000.0, y0 + 20_000.0], 50.0);
        for &(from, to) in shores {
            let (a, b) = (x0 + from, x0 + to);
            for s in [1.0, -1.0] {
                grid.stamp_triangle([[a, y0 + s * 300.0], [b, y0 + s * 300.0], [b, y0 + s * 3_000.0]], true);
                grid.stamp_triangle([[a, y0 + s * 300.0], [b, y0 + s * 3_000.0], [a, y0 + s * 3_000.0]], true);
            }
        }
        grid.finalize();
        corridor_on(grid, k, vec![[x0, y0], [x0 + len, y0]])
    }

    /// A straight corridor across open sea — nothing unsafe anywhere — from
    /// `lat`, `lon`, displaced `d` plane metres, on a grid of `res` metres.
    fn open_corridor(lat: f64, lon: f64, d: [f64; 2], res: f64) -> autoroute::Corridor {
        let (x0, y0) = Projection::to_mercator(lat, lon);
        let end = [x0 + d[0], y0 + d[1]];
        let min = [x0.min(end[0]) - 20_000.0, y0.min(end[1]) - 20_000.0];
        let max = [x0.max(end[0]) + 20_000.0, y0.max(end[1]) + 20_000.0];
        let mut grid = Grid::new(min, max, res);
        grid.finalize();
        let mid_lat = Projection::to_wgs84(x0 + d[0] / 2.0, y0 + d[1] / 2.0).0;
        corridor_on(grid, 1.0 / mid_lat.to_radians().cos(), vec![[x0, y0], end])
    }

    /// `kt` from `from_deg`, the same everywhere and from 0 to `last_ms`.
    fn steady_forecast(from_deg: f64, kt: f64, last_ms: i64) -> GribForecast {
        let ms = kt * 1852.0 / 3600.0;
        // U/V point where the air goes: away from where it comes from.
        let toward = (from_deg + 180.0).to_radians();
        let (u, v) = ((ms * toward.sin()) as f32, (ms * toward.cos()) as f32);
        GribForecast::synthetic(
            vec![0, last_ms],
            vec![70.0, 40.0],
            vec![-10.0, 30.0],
            vec![vec![u; 4], vec![u; 4]],
            vec![vec![v; 4], vec![v; 4]],
        )
    }

    fn plan_with(
        c: &autoroute::Corridor,
        config: &RoutingConfig,
        safety: &SafetyConfig,
        forecast: &GribForecast,
    ) -> WxPlanned {
        let polar = Polar::parse(DEFAULT_POLAR).unwrap();
        plan(
            &WxJob {
                corridor: c,
                depart: Utc.timestamp_millis_opt(0).single().unwrap(),
                polar: &polar,
                polar_name: "default",
                forecast,
                waves: None,
                currents: None,
                config,
                safety,
            },
            "test",
        )
        .expect("a sailable passage")
    }

    fn positions(planned: &WxPlanned) -> Vec<[f64; 2]> {
        planned.waypoints.iter().map(|w| merc(w.position)).collect()
    }

    fn etas_ms(planned: &WxPlanned) -> Vec<i64> {
        planned
            .route
            .legs
            .iter()
            .map(|l| l.plan.as_ref().expect("every leg is planned").eta.timestamp_millis())
            .collect()
    }

    fn count(warnings: &[String], prefix: &str) -> usize {
        warnings.iter().filter(|w| w.starts_with(prefix)).count()
    }

    const NO_BETTER_WAY: &str = "no better way was found across";
    const OVER_LIMITS: &str = "wind or sea over your limits in confined water";
    const NO_WIND: &str = "no sailing wind for part of the route";
    const OUTRUNS: &str = "the passage outruns the forecast";

    // ---- errors ----

    /// A routing error reads as the error underneath it: the user sees why
    /// the engine stopped, not that an engine exists.
    #[test]
    fn a_routing_error_says_what_went_wrong() {
        for e in [isochrone::IsoError::Unreachable, isochrone::IsoError::Cancelled] {
            let said = e.to_string();
            assert!(!said.is_empty());
            assert_eq!(WxError::Engine(e).to_string(), said);
        }
        let Err(polar) = Polar::parse("not a polar") else { panic!("garbage is not a polar") };
        let said = polar.to_string();
        assert_eq!(WxError::Polar(polar).to_string(), said);
    }

    // ---- plan(): where the isochrones sail and where the corridor does ----

    /// Open water begins half a mile from anything unsafe. A shore 300 m off
    /// the track that ends 5 km along it keeps the track confined until
    /// √((0.5 NM)² − (300 m)²) past its end: about 1630 plane metres at 56°N
    /// (half a mile is 0.5 × 1852 × sec 56° = 1656 plane m), give or take the
    /// few per cent of the distance field's chamfer metric. That boundary is
    /// the route's first turn point, and the points are named Start, WX-01…
    /// and Finish.
    #[test]
    fn the_harbour_approach_ends_half_a_mile_from_the_shore() {
        let c = shore_corridor(30_000.0, &[(-5_000.0, 5_000.0)]);
        let planned = plan_with(
            &c,
            &RoutingConfig::default(),
            &SafetyConfig::default(),
            &steady_forecast(0.0, 12.0, 48 * H),
        );
        let [x0, y0] = c.points[0];
        let boundary = positions(&planned)[1];
        let beyond = boundary[0] - (x0 + 5_000.0);
        assert!((1_400.0..1_800.0).contains(&beyond), "open water begins {beyond} m past the shore");
        assert!((boundary[1] - y0).abs() < 1.0, "the boundary lies on the corridor");

        let names: Vec<&str> = planned.waypoints.iter().map(|w| w.name.as_str()).collect();
        assert!(names.len() >= 3, "{names:?}");
        assert_eq!(names[0], "Start");
        assert_eq!(*names.last().unwrap(), "Finish");
        for (i, n) in names.iter().enumerate().take(names.len() - 1).skip(1) {
            assert_eq!(*n, format!("WX-{i:02}"));
        }
    }

    /// An isochrone told to keep 0.6 NM from the shore cannot leave an open
    /// stretch that begins 0.5 NM from it, so that stretch follows the
    /// corridor and the route says so — naming the open water's length:
    /// 30 000 m less the ~6 600 m approach is ~23 400 plane m, 7.1 NM at
    /// 3311 plane m to the mile.
    #[test]
    fn open_water_inside_the_isochrones_offing_follows_the_corridor_and_says_so() {
        let c = shore_corridor(30_000.0, &[(-5_000.0, 5_000.0)]);
        let safety = SafetyConfig { offing_min_nm: 0.6, ..SafetyConfig::default() };
        let planned =
            plan_with(&c, &RoutingConfig::default(), &safety, &steady_forecast(0.0, 12.0, 48 * H));
        let notes: Vec<&String> =
            planned.warnings.iter().filter(|w| w.starts_with(NO_BETTER_WAY)).collect();
        assert_eq!(notes.len(), 1, "{:?}", planned.warnings);
        assert!(notes[0].contains("across 7 NM of open water"), "{}", notes[0]);
        let y0 = c.points[0][1];
        for p in positions(&planned) {
            assert!((p[1] - y0).abs() < 1.0, "off the corridor at {p:?}");
        }
    }

    /// Two miles of open water between two harbour stretches is under the
    /// five-mile minimum: it is timed along the corridor, never offered to
    /// an isochrone — which here, kept 0.6 NM off, could not have sailed it
    /// and would have said so.
    #[test]
    fn a_short_run_of_open_water_is_not_worth_an_isochrone() {
        // Shores everywhere but a 9.9 km gap; half a mile from both its
        // ends leaves ~6.7 km, about 2 NM, of open water.
        let c = shore_corridor(20_000.0, &[(-5_000.0, 4_000.0), (13_900.0, 25_000.0)]);
        let safety = SafetyConfig { offing_min_nm: 0.6, ..SafetyConfig::default() };
        let planned =
            plan_with(&c, &RoutingConfig::default(), &safety, &steady_forecast(0.0, 12.0, 48 * H));
        assert_eq!(count(&planned.warnings, NO_BETTER_WAY), 0, "{:?}", planned.warnings);
        assert_eq!(planned.waypoints.len(), 2, "one confined stretch, one leg");
    }

    /// With harbours at both ends, the isochrones sail only the open water
    /// between them: they stop where the far approach begins, half a mile
    /// short of its shore, and the corridor takes the route in from there.
    /// No leg is sailed twice, so none has no length.
    #[test]
    fn an_open_stretch_ends_where_the_far_approach_begins() {
        let len = 35_000.0;
        let c = shore_corridor(len, &[(-5_000.0, 5_000.0), (len - 5_000.0, len + 5_000.0)]);
        // No offing: the isochrones could go anywhere the corridor goes.
        let safety = SafetyConfig { offing_min_nm: 0.0, ..SafetyConfig::default() };
        let planned =
            plan_with(&c, &RoutingConfig::default(), &safety, &steady_forecast(0.0, 12.0, 48 * H));
        assert!(planned.warnings.is_empty(), "{:?}", planned.warnings);
        let [x0, y0] = c.points[0];
        let far_shore = x0 + len - 5_000.0;
        let pts = positions(&planned);
        assert!(
            pts.iter().any(|p| (1_400.0..1_800.0).contains(&(far_shore - p[0])) && (p[1] - y0).abs() < 1.0),
            "no turn point where the far approach begins: {pts:?}"
        );
        for w in pts.windows(2) {
            assert!((w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]) > 1.0, "a leg of no length");
        }
    }

    /// Times of the isochrones' turn points after the confined first step:
    /// each is a node of a ring, so a whole number of steps after the open
    /// stretch began — which is when the first leg arrived.
    fn ring_offsets_ms(planned: &WxPlanned) -> Vec<i64> {
        let etas = etas_ms(planned);
        let t0 = etas[0];
        etas[1..etas.len() - 1].iter().map(|t| t - t0).collect()
    }

    /// Twenty miles NE in open sea, sampled every ~400 plane metres. The
    /// first sample ends the confined start (the ends are confined by
    /// definition), and at ~399 m — over a cable, 332 plane m here — it stays
    /// a turn point. Under 60 NM the step is the coastal one, 840 s, being
    /// shorter than eight rings at six knots (~1490 s) would be.
    #[test]
    fn an_open_stretch_under_sixty_miles_is_stepped_at_the_coastal_step() {
        let k = 1.0 / 56f64.to_radians().cos();
        let side = 20.0 * 1852.0 * k / 2f64.sqrt();
        let c = open_corridor(56.0, 11.0, [side, side], 400.0);
        let config = RoutingConfig { dt_coastal_s: 840, dt_offshore_s: 10_800, ..RoutingConfig::default() };
        let planned =
            plan_with(&c, &config, &SafetyConfig::default(), &steady_forecast(315.0, 12.0, 48 * H));
        assert!(planned.warnings.is_empty(), "{:?}", planned.warnings);

        let [w0, w1] = [c.points[0], c.points[1]];
        let len = (w1[0] - w0[0]).hypot(w1[1] - w0[1]);
        let step = len / (len / 400.0).ceil();
        let first = positions(&planned)[1];
        let out = (first[0] - w0[0]).hypot(first[1] - w0[1]);
        assert!((out - step).abs() < 0.01, "first turn point {out} m out, the first step is {step} m");

        let offsets = ring_offsets_ms(&planned);
        assert!(!offsets.is_empty(), "the isochrones left no ring node");
        for o in offsets {
            assert_eq!(o % 840_000, 0, "a ring node {o} ms in: not a whole number of 840 s steps");
        }
    }

    /// Π₃, bounded: the configured step, but no longer than eight rings at
    /// six knots across the stretch. About 54 NM of open water would take
    /// 54 / 6 / 8 h ≈ 4050 s rings, under the 7200 s coastal step.
    #[test]
    fn an_open_stretch_is_crossed_in_eight_rings_at_six_knots_when_the_step_allows() {
        let k = 1.0 / 56f64.to_radians().cos();
        let side = 54.5 * 1852.0 * k / 2f64.sqrt();
        let c = open_corridor(56.0, 11.0, [side, side], 400.0);
        let config = RoutingConfig { dt_coastal_s: 7_200, dt_offshore_s: 10_800, ..RoutingConfig::default() };
        let planned =
            plan_with(&c, &config, &SafetyConfig::default(), &steady_forecast(315.0, 12.0, 48 * H));
        assert!(planned.warnings.is_empty(), "{:?}", planned.warnings);

        // The open stretch runs from the first sample, one grid step out,
        // to the end.
        let [w0, w1] = [c.points[0], c.points[1]];
        let n = ((w1[0] - w0[0]).hypot(w1[1] - w0[1]) / 400.0).ceil();
        let f = 1.0 / n;
        let s1 = [w0[0] + (w1[0] - w0[0]) * f, w0[1] + (w1[1] - w0[1]) * f];
        let open_nm = (w1[0] - s1[0]).hypot(w1[1] - s1[1]) / (crate::geo::METRES_PER_NM * c.k);
        assert!(open_nm > 53.0 && open_nm < 55.0, "{open_nm}");
        let dt = (open_nm / 6.0 / 8.0 * 3600.0) as u32;
        assert!(dt > 3_600 && dt < 7_200, "{dt}");

        let offsets = ring_offsets_ms(&planned);
        assert!(!offsets.is_empty(), "the isochrones left no ring node");
        for o in offsets {
            assert_eq!(o % (dt as i64 * 1000), 0, "a ring node {o} ms in: not a whole number of {dt} s steps");
        }
    }

    /// The confined first step of a corridor sampled every 200 plane metres
    /// ends inside a cable (332 plane m at 56°N) of the start: the point
    /// goes, and the open water is sailed from the start.
    #[test]
    fn a_turn_point_closer_than_a_cable_to_the_last_goes() {
        let k = 1.0 / 56f64.to_radians().cos();
        let c = open_corridor(56.0, 11.0, [6.0 * 1852.0 * k, 0.0], 200.0);
        let planned = plan_with(
            &c,
            &RoutingConfig::default(),
            &SafetyConfig::default(),
            &steady_forecast(0.0, 12.0, 48 * H),
        );
        let pts = positions(&planned);
        let cable = 0.1 * crate::geo::METRES_PER_NM * c.k;
        for w in pts.windows(2) {
            let d = (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]);
            assert!(d >= cable, "a leg of {d} m, under a cable ({cable} m), survived");
        }
    }

    /// "Closer than a cable" goes; a point exactly a cable from the last is
    /// not closer, and stays. The corridor's scale is chosen so that a cable
    /// (0.1 NM of ground, in plane metres) is exactly its first leg.
    #[test]
    fn a_turn_point_exactly_a_cable_from_the_last_stays() {
        let y0 = Projection::to_mercator(62.4, 0.0).1;
        let (a, v, b) = ([0.0, y0], [400.0, y0], [4_000.0, y0]);
        let (pa, pv) = (merc(to_latlon(a)), merc(to_latlon(v)));
        let d = (pv[0] - pa[0]).hypot(pv[1] - pa[1]);
        let near = (d / (0.1 * crate::geo::METRES_PER_NM)).to_bits();
        let k = (0..2_000u64)
            .flat_map(|i| [near + i, near - i])
            .map(f64::from_bits)
            .find(|&k| 0.1 * (crate::geo::METRES_PER_NM * k) == d)
            .expect("a scale whose cable is exactly the first leg");
        assert!((k - 1.0 / 62.4f64.to_radians().cos()).abs() < 1e-2, "{k} is not about sec 62.4°");
        let mut grid = Grid::new([-5_000.0, y0 - 5_000.0], [10_000.0, y0 + 5_000.0], 50.0);
        grid.finalize();
        let c = corridor_on(grid, k, vec![a, v, b]);
        let planned = plan_with(
            &c,
            &RoutingConfig::default(),
            &SafetyConfig::default(),
            &steady_forecast(0.0, 12.0, 48 * H),
        );
        assert_eq!(planned.waypoints.len(), 3, "the turn point a cable out was dropped");
    }

    /// A jog of 225 m round a hazard: its corner is within a cable of the
    /// turn before it, but cutting it would cross the hazard, so it stays —
    /// "where the leg that replaces it is itself clear" is not met.
    #[test]
    fn a_short_jog_round_a_hazard_is_kept() {
        let (x0, y0) = Projection::to_mercator(56.0, 11.0);
        let mut grid = Grid::new([x0 - 5_000.0, y0 - 5_000.0], [x0 + 10_000.0, y0 + 5_000.0], 50.0);
        let x1 = x0 + 2_000.0;
        grid.stamp_triangle([[x1 + 900.0, y0 + 60.0], [x1 + 1_100.0, y0 + 60.0], [x1 + 1_000.0, y0 + 140.0]], true);
        grid.finalize();
        let pts = vec![[x0, y0], [x1, y0], [x1, y0 + 225.0], [x1 + 2_000.0, y0 + 225.0]];
        assert!(!grid.segment_clear(pts[1], pts[3], 0.0), "the fixture's short cut must be blocked");
        for (i, w) in pts.windows(2).enumerate() {
            assert!(grid.segment_clear(w[0], w[1], 0.0), "the fixture's leg {i} must be clear");
        }
        let c = corridor_on(grid, 1.0 / 56f64.to_radians().cos(), pts.clone());
        let planned = plan_with(
            &c,
            &RoutingConfig::default(),
            &SafetyConfig::default(),
            &steady_forecast(0.0, 12.0, 48 * H),
        );
        let names: Vec<&str> = planned.waypoints.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["Start", "WX-01", "WX-02", "Finish"]);
        for (p, q) in positions(&planned).iter().zip(&pts) {
            assert!((p[0] - q[0]).hypot(p[1] - q[1]) < 0.01, "{p:?} is not the corridor's {q:?}");
        }
    }

    /// A corridor 4 km east through a caution area, carrying `warnings`.
    fn caution_corridor(warnings: Vec<String>) -> autoroute::Corridor {
        let (x0, y0) = Projection::to_mercator(56.0, 11.0);
        let mut grid = Grid::new([x0 - 5_000.0, y0 - 5_000.0], [x0 + 10_000.0, y0 + 5_000.0], 50.0);
        grid.stamp_triangle([[x0 + 1_900.0, y0 - 500.0], [x0 + 2_100.0, y0 - 500.0], [x0 + 2_000.0, y0 + 500.0]], false);
        grid.finalize();
        let mut c = corridor_on(grid, 1.0 / 56f64.to_radians().cos(), vec![[x0, y0], [x0 + 4_000.0, y0]]);
        c.warnings = warnings;
        c
    }

    /// A route through a caution area says so — once, whether or not the
    /// corridor search already did, and beside whatever else it said.
    #[test]
    fn a_route_through_a_caution_area_says_so_once() {
        let note = autoroute::CAUTION_AREA_NOTE.to_string();
        let other = "an unrelated corridor note".to_string();
        for given in [vec![], vec![note.clone()], vec![other.clone()]] {
            let c = caution_corridor(given.clone());
            let planned = plan_with(
                &c,
                &RoutingConfig::default(),
                &SafetyConfig::default(),
                &steady_forecast(0.0, 12.0, 48 * H),
            );
            let notes = planned.warnings.iter().filter(|w| **w == note).count();
            assert_eq!(notes, 1, "given {given:?}: {:?}", planned.warnings);
            for g in &given {
                assert!(planned.warnings.contains(g), "lost {g:?}");
            }
        }
    }

    /// The forecast warning is for a passage that sails past the last
    /// field. Arriving at the moment of the last field sails nothing on a
    /// frozen forecast; arriving after it does.
    #[test]
    fn a_passage_outruns_the_forecast_only_when_it_arrives_after_the_last_field() {
        let k = 1.0 / 56f64.to_radians().cos();
        // 3 NM: confined end to end, so timed along the corridor.
        let c = open_corridor(56.0, 11.0, [3.0 * 1852.0 * k, 0.0], 200.0);
        let config = RoutingConfig::default();
        let safety = SafetyConfig::default();
        let long = plan_with(&c, &config, &safety, &steady_forecast(0.0, 12.0, 48 * H));
        assert_eq!(count(&long.warnings, OUTRUNS), 0);
        let arrival = long.arrival.timestamp_millis();
        assert!(arrival > 10 * 60_000, "{arrival}");

        let exact = plan_with(&c, &config, &safety, &steady_forecast(0.0, 12.0, arrival));
        assert_eq!(exact.arrival.timestamp_millis(), arrival, "the same wind, the same passage");
        assert_eq!(count(&exact.warnings, OUTRUNS), 0, "{:?}", exact.warnings);

        let short = plan_with(&c, &config, &safety, &steady_forecast(0.0, 12.0, 10 * 60_000));
        assert_eq!(count(&short.warnings, OUTRUNS), 1, "{:?}", short.warnings);
    }

    // ---- stretches(): the corridor cut where the water changes ----

    /// An L-shaped corridor on whole-metre plane coordinates: 12 010 m north,
    /// then 12 010 m east, a small hazard 300–500 m off each end.
    fn l_corridor() -> autoroute::Corridor {
        let (x, y) = (1_200_000.0, 7_500_000.0);
        let leg = 12_010.0;
        let mut grid = Grid::new([x - 5_000.0, y - 5_000.0], [x + 20_000.0, y + 20_000.0], 50.0);
        grid.stamp_triangle([[x - 500.0, y - 200.0], [x - 300.0, y - 200.0], [x - 400.0, y + 200.0]], true);
        let b = [x + leg, y + leg];
        grid.stamp_triangle([[b[0] + 300.0, b[1] - 200.0], [b[0] + 500.0, b[1] - 200.0], [b[0] + 400.0, b[1] + 200.0]], true);
        grid.finalize();
        corridor_on(grid, 1.8, vec![[x, y], [x, y + leg], b])
    }

    /// Is `p` one of the walk's samples along `a`→`b` — a whole number of
    /// steps of length/⌈length/res⌉ from `a`, short of `b`?
    fn on_walk(p: [f64; 2], a: [f64; 2], b: [f64; 2], res: f64) -> bool {
        let len = (b[0] - a[0]).hypot(b[1] - a[1]);
        let step = len / (len / res).ceil();
        let along = (p[0] - a[0]).hypot(p[1] - a[1]);
        let off_line = ((b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])).abs() / len;
        let i = along / step;
        off_line < 1e-6 && along > 0.0 && along < len && (i - i.round()).abs() < 1e-6
    }

    /// The open water begins on the north leg and ends on the east leg; the
    /// open stretch between carries the corridor's own corner, each
    /// boundary is a sample of the cell-by-cell walk, and the pieces join.
    /// The open run is ~22 km of corridor — over the 15 km minimum only when
    /// both legs count toward it.
    #[test]
    fn an_open_stretch_round_a_corner_keeps_the_corner() {
        let c = l_corridor();
        let [a, v, b] = [c.points[0], c.points[1], c.points[2]];
        let parts = stretches(&c, 1_000.0, 15_000.0);
        let open: Vec<bool> = parts.iter().map(|p| p.open).collect();
        assert_eq!(open, [false, true, false]);
        let (b1, b2) = (parts[1].pts[0], *parts[1].pts.last().unwrap());
        assert_eq!(parts[0].pts, vec![a, b1]);
        assert_eq!(parts[1].pts, vec![b1, v, b2], "the corner belongs to the open stretch");
        assert_eq!(parts[2].pts, vec![b2, b]);
        assert!(on_walk(b1, a, v, 50.0), "{b1:?} is not a sample of the north leg");
        assert!(on_walk(b2, v, b, 50.0), "{b2:?} is not a sample of the east leg");
        // Half a mile — here 1000 m — from the hazards, give or take a cell.
        assert!((b1[1] - a[1] - 1_120.0).abs() < 150.0, "{}", b1[1] - a[1]);
        assert!((b[0] - b2[0] - 700.0).abs() < 150.0, "{}", b[0] - b2[0]);
    }

    /// An open run shorter than the minimum is confined, and confined runs
    /// either side of it merge with it: one stretch, end to end. The run is
    /// ~18 km of a 20 km corridor whose open water begins ~1 km in; the
    /// minimum is 19 km.
    #[test]
    fn a_short_open_run_merges_into_the_confined_water_around_it() {
        let (x, y) = (1_200_000.0, 7_500_000.0);
        let len = 20_000.0;
        let mut grid = Grid::new([x - 5_000.0, y - 5_000.0], [x + len + 5_000.0, y + 5_000.0], 50.0);
        grid.stamp_triangle([[x - 500.0, y - 200.0], [x - 300.0, y - 200.0], [x - 400.0, y + 200.0]], true);
        grid.stamp_triangle([[x + len + 300.0, y - 200.0], [x + len + 500.0, y - 200.0], [x + len + 400.0, y + 200.0]], true);
        grid.finalize();
        let c = corridor_on(grid, 1.8, vec![[x, y], [x + len, y]]);
        // Long enough, it is open water.
        let open: Vec<bool> = stretches(&c, 1_000.0, 17_000.0).iter().map(|p| p.open).collect();
        assert_eq!(open, [false, true, false]);
        let parts = stretches(&c, 1_000.0, 19_000.0);
        assert_eq!(parts.len(), 1);
        assert!(!parts[0].open);
        assert_eq!(parts[0].pts, c.points);
    }

    /// "Shorter than the minimum" is confined; an open run exactly as long
    /// as the minimum is not shorter, and stays open. 6400 m sampled every
    /// 50 m: every arc length is a whole multiple of 50, exactly.
    #[test]
    fn an_open_run_exactly_the_minimum_long_stays_open() {
        let (x, y) = (1_200_000.0, 7_500_000.0);
        let len = 6_400.0;
        let mut grid = Grid::new([x - 5_000.0, y - 5_000.0], [x + len + 5_000.0, y + 5_000.0], 50.0);
        grid.stamp_triangle([[x - 500.0, y - 200.0], [x - 300.0, y - 200.0], [x - 400.0, y + 200.0]], true);
        grid.stamp_triangle([[x + len + 300.0, y - 200.0], [x + len + 500.0, y - 200.0], [x + len + 400.0, y + 200.0]], true);
        grid.finalize();
        let c = corridor_on(grid, 1.8, vec![[x, y], [x + len, y]]);
        let parts = stretches(&c, 1_000.0, 0.0);
        assert_eq!(parts.iter().map(|p| p.open).collect::<Vec<_>>(), [false, true, false]);
        // The open stretch reaches one sample past its run, to the first
        // sample of the confined water after it.
        let run = parts[1].pts[1][0] - parts[1].pts[0][0] - 50.0;
        assert!(run > 3_000.0 && run.fract() == 0.0, "{run}");
        let kept = stretches(&c, 1_000.0, run);
        assert_eq!(kept.iter().map(|p| p.open).collect::<Vec<_>>(), [false, true, false]);
        assert_eq!(stretches(&c, 1_000.0, run + 0.5).len(), 1);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]

        /// Whatever the chart, the stretches tile the corridor: they start
        /// where it starts and end where it ends, each begins where the last
        /// ended, open and confined alternate from a confined start, every
        /// point lies on the corridor, and no open stretch is under the
        /// minimum.
        #[test]
        fn stretches_tile_the_corridor(
            leg1 in 3_000.0..9_000.0f64,
            leg2 in 3_000.0..9_000.0f64,
            turn in -1.2..1.2f64,
            hazards in proptest::collection::vec((0.05..0.95f64, 150.0..2_500.0f64, any::<bool>()), 0..4),
            open_m in 200.0..1_500.0f64,
            min_open_m in 0.0..6_000.0f64,
        ) {
            let (x, y) = (1_200_000.0, 7_500_000.0);
            let v = [x + leg1, y];
            let b = [v[0] + leg2 * turn.cos(), v[1] + leg2 * turn.sin()];
            let pts = vec![[x, y], v, b];
            let mut grid = Grid::new([x - 4_000.0, y - 13_000.0], [x + 22_000.0, y + 13_000.0], 50.0);
            // Small hazards beside the track, a fraction of the way along it.
            for &(t, off, left) in &hazards {
                let (p, dir) = if t * (leg1 + leg2) < leg1 {
                    ([x + t * (leg1 + leg2), y], [1.0, 0.0])
                } else {
                    let s = t * (leg1 + leg2) - leg1;
                    ([v[0] + s * turn.cos(), v[1] + s * turn.sin()], [turn.cos(), turn.sin()])
                };
                let side = if left { 1.0 } else { -1.0 };
                let n = [-dir[1] * side, dir[0] * side];
                let c0 = [p[0] + n[0] * off, p[1] + n[1] * off];
                grid.stamp_triangle([c0, [c0[0] + n[0] * 100.0 + dir[0] * 60.0, c0[1] + n[1] * 100.0 + dir[1] * 60.0], [c0[0] + n[0] * 100.0 - dir[0] * 60.0, c0[1] + n[1] * 100.0 - dir[1] * 60.0]], true);
            }
            grid.finalize();
            let c = corridor_on(grid, 1.8, pts.clone());
            let parts = stretches(&c, open_m, min_open_m);
            prop_assert!(!parts.is_empty());
            prop_assert_eq!(parts[0].pts[0], pts[0]);
            prop_assert_eq!(*parts.last().unwrap().pts.last().unwrap(), b);
            prop_assert!(!parts[0].open, "the ends are confined");
            for w in parts.windows(2) {
                prop_assert_eq!(*w[0].pts.last().unwrap(), w[1].pts[0]);
                prop_assert!(w[0].open != w[1].open, "two {} stretches side by side", w[0].open);
            }
            let on_corridor = |p: [f64; 2]| {
                pts.windows(2).any(|s| {
                    let (a, b) = (s[0], s[1]);
                    let len = (b[0] - a[0]).hypot(b[1] - a[1]);
                    let t = ((p[0] - a[0]) * (b[0] - a[0]) + (p[1] - a[1]) * (b[1] - a[1])) / (len * len);
                    let q = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
                    (-1e-9..=1.0 + 1e-9).contains(&t) && (p[0] - q[0]).hypot(p[1] - q[1]) < 1e-6
                })
            };
            for s in &parts {
                prop_assert!(s.pts.len() >= 2);
                for &p in &s.pts {
                    prop_assert!(on_corridor(p), "{:?} is off the corridor", p);
                }
                if s.open {
                    prop_assert!(polyline_len(&s.pts) >= min_open_m - 1e-6);
                }
            }
        }
    }

    // ---- time_polyline(): confined water, timed through the polar ----

    /// A point `nm` miles of ground from 56°N 11°E along `bearing_deg`, in
    /// the plane, by the scale at the start — the scale the timing uses.
    fn sail_from(a: [f64; 2], bearing_deg: f64, nm: f64) -> [f64; 2] {
        let lat = Projection::to_wgs84(a[0], a[1]).0;
        let d = nm * crate::geo::METRES_PER_NM / lat.to_radians().cos();
        let r = bearing_deg.to_radians();
        [a[0] + d * r.sin(), a[1] + d * r.cos()]
    }

    fn here() -> [f64; 2] {
        let (x, y) = Projection::to_mercator(56.0, 11.0);
        [x, y]
    }

    fn timing_input<'a>(
        polar: &'a Polar,
        wind: &'a dyn isochrone::WindField,
        waves: Option<&'a dyn isochrone::WaveField>,
        config: &'a RoutingConfig,
    ) -> IsochroneInput<'a> {
        let at = LatLon::new(56.0, 11.0);
        IsochroneInput {
            polar,
            wind,
            grid: None,
            start: at,
            finish: at,
            depart_ms: 0,
            config,
            dt_s: 3600,
            offing_min_m: 0.0,
            current: None,
            waves,
            gates: &[],
        }
    }

    /// Hours to sail `pts` from time 0, and what the timing said.
    fn hours_and_notes(input: &IsochroneInput<'_>, pts: &[[f64; 2]]) -> (f64, Vec<String>, Vec<isochrone::PlannedPoint>) {
        let mut notes = Vec::new();
        let out = time_polyline(input, pts, 0, 1.0, &mut notes);
        assert_eq!(out.len(), pts.len() - 1, "one point per vertex after the first");
        (out.last().unwrap().time_ms as f64 / 3.6e6, notes, out)
    }

    /// A wind over the limit in confined water is sailed, not refused — the
    /// corridor is the only way through — at the polar's speed (clamped at
    /// its top band: 7.3 kn on the beam), and the route says so, once.
    #[test]
    fn wind_over_the_limit_in_confined_water_is_sailed_and_reported_once() {
        let polar = Polar::parse(DEFAULT_POLAR).unwrap();
        let config = RoutingConfig::default();
        let gale = ConstantWind { from_deg: 0.0, tws_kt: 40.0 };
        let a = here();
        // Two legs, 3 NM east then 3 NM more: several chunks over the limit.
        let pts = [a, sail_from(a, 90.0, 3.0), sail_from(sail_from(a, 90.0, 3.0), 90.0, 3.0)];
        let (h, notes, _) = hours_and_notes(&timing_input(&polar, &gale, None, &config), &pts);
        assert_eq!(count(&notes, OVER_LIMITS), 1, "{notes:?}");
        assert_eq!(count(&notes, NO_WIND), 0, "{notes:?}");
        let v = polar.speed_kt(90.0, 40.0).unwrap();
        assert!((v - 7.3).abs() < 1e-5, "{v}");
        assert!((h - 6.0 / v).abs() < 0.002 * h, "{h} h, expected {} h", 6.0 / v);
    }

    /// The limits are "over": wind at exactly the limit, or a sea at exactly
    /// it, is within them — as the engine's own §7.5 wall has it. A sea over
    /// the limit is reported in light wind too; a calm sea never is.
    #[test]
    fn the_limits_are_what_is_over_them() {
        let polar = Polar::parse(DEFAULT_POLAR).unwrap();
        let config = RoutingConfig::default();
        let a = here();
        let pts = [a, sail_from(a, 90.0, 1.5)];
        let at_limit = ConstantWind { from_deg: 0.0, tws_kt: config.max_tws_kt };
        let (_, notes, _) = hours_and_notes(&timing_input(&polar, &at_limit, None, &config), &pts);
        assert_eq!(count(&notes, OVER_LIMITS), 0, "{notes:?}");

        let breeze = ConstantWind { from_deg: 0.0, tws_kt: 12.0 };
        for (hs, warned) in [(config.max_wave_m, 0), (1.0, 0), (0.0, 0), (5.0, 1)] {
            let sea = ConstantWaves { hs_m: hs };
            let (_, notes, _) = hours_and_notes(&timing_input(&polar, &breeze, Some(&sea), &config), &pts);
            assert_eq!(count(&notes, OVER_LIMITS), warned, "Hs {hs}: {notes:?}");
            assert_eq!(count(&notes, NO_WIND), 0, "Hs {hs}: {notes:?}");
        }
    }

    /// §5's heuristic: a sea of Hs costs speed by 1/(1 + c·Hs²) — at the
    /// default c = 0.03 a 2 m sea makes the same leg take 1.12 times as long.
    #[test]
    fn a_two_metre_sea_slows_the_boat_by_the_configured_penalty() {
        let polar = Polar::parse(DEFAULT_POLAR).unwrap();
        let config = RoutingConfig::default();
        assert_eq!(config.wave_penalty_coef, 0.03);
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 12.0 };
        let a = here();
        let pts = [a, sail_from(a, 90.0, 1.5)];
        let (flat, _, _) = hours_and_notes(&timing_input(&polar, &wind, None, &config), &pts);
        let sea = ConstantWaves { hs_m: 2.0 };
        let (rough, notes, _) = hours_and_notes(&timing_input(&polar, &wind, Some(&sea), &config), &pts);
        assert!(notes.is_empty(), "{notes:?}");
        assert!((rough / flat - 1.12).abs() < 1e-5, "{}", rough / flat);
        assert!((flat - 1.5 / polar.speed_kt(90.0, 12.0).unwrap()).abs() < 1e-5);
    }

    /// A flat sea costs nothing however heavy the coefficient, and a zero
    /// coefficient turns the penalty off however big the sea — even at the
    /// extremes, where the formula itself would be 0·∞.
    #[test]
    fn a_flat_sea_or_a_zero_coefficient_costs_nothing() {
        let polar = Polar::parse(DEFAULT_POLAR).unwrap();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 12.0 };
        let a = here();
        let pts = [a, sail_from(a, 90.0, 1.5)];
        let plain = RoutingConfig::default();
        let (flat, _, _) = hours_and_notes(&timing_input(&polar, &wind, None, &plain), &pts);
        for (hs, coef) in [(0.0, f64::INFINITY), (f64::INFINITY, 0.0), (3.0, 0.0), (0.0, 0.03)] {
            let config = RoutingConfig { wave_penalty_coef: coef, ..RoutingConfig::default() };
            let sea = ConstantWaves { hs_m: hs };
            let (h, notes, _) = hours_and_notes(&timing_input(&polar, &wind, Some(&sea), &config), &pts);
            assert_eq!(count(&notes, NO_WIND), 0, "Hs {hs}, c {coef}: {notes:?}");
            assert!((h - flat).abs() < 1e-6, "Hs {hs}, c {coef}: {h} h, flat {flat} h");
        }
    }

    /// Hours for the open-water engine to sail `a`→`b` alone, and for the
    /// corridor timing to: both routers model the same boat in the same sea,
    /// so on a leg short enough for the engine to close on from its start
    /// they must agree.
    fn engine_and_corridor_hours(hs: f64, coef: f64) -> (f64, f64) {
        let polar = Polar::parse(DEFAULT_POLAR).unwrap();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 12.0 };
        let sea = ConstantWaves { hs_m: hs };
        // No walls, so the engine sails whatever the sea.
        let config = RoutingConfig { wave_penalty_coef: coef, max_wave_m: 100.0, ..RoutingConfig::default() };
        let a = here();
        let b = sail_from(a, 90.0, 1.5);
        let input = IsochroneInput {
            start: to_latlon(a),
            finish: to_latlon(b),
            ..timing_input(&polar, &wind, Some(&sea), &config)
        };
        let engine = isochrone::solve(&input).expect("a beam reach").arrival_ms as f64 / 3.6e6;
        let (corridor, _, _) = hours_and_notes(&input, &[a, b]);
        (engine, corridor)
    }

    /// The penalty's edge cases, as the engine has them: no penalty unless
    /// both the sea and the coefficient are positive.
    #[test]
    fn the_corridor_and_the_engine_agree_on_odd_seas() {
        for (hs, coef) in [(-2.0, 0.03), (2.0, -0.02), (2.0, 0.03), (-1.0, -0.01)] {
            let (engine, corridor) = engine_and_corridor_hours(hs, coef);
            assert!((engine - corridor).abs() < 2e-6, "Hs {hs}, c {coef}: engine {engine} h, corridor {corridor} h");
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(12))]

        /// Whatever the sea and coefficient, a leg the engine closes on from
        /// its start takes the corridor timing just as long.
        #[test]
        fn the_corridor_and_the_engine_agree_on_any_sea(hs in -3.0..5.0f64, coef in -0.02..0.1f64) {
            let (engine, corridor) = engine_and_corridor_hours(hs, coef);
            prop_assert!((engine - corridor).abs() < 2e-6, "engine {} h, corridor {} h", engine, corridor);
        }
    }

    /// A tack costs its penalty, exactly, on top of the sailing: a reach on
    /// one side of the wind then the other (060° then 300° in a northerly).
    /// Leaving on either tack is free.
    #[test]
    fn a_tack_between_corridor_legs_costs_the_tack_penalty() {
        let polar = Polar::parse(DEFAULT_POLAR).unwrap();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 12.0 };
        let a = here();
        let v = sail_from(a, 60.0, 3.0);
        let pts = [a, v, sail_from(v, 300.0, 3.0)];
        let free = RoutingConfig { tack_penalty_day_s: 0, gybe_penalty_day_s: 0, ..RoutingConfig::default() };
        let tack = RoutingConfig { tack_penalty_day_s: 300, ..free.clone() };
        let gybe = RoutingConfig { gybe_penalty_day_s: 300, ..free.clone() };
        let end = |config: &RoutingConfig| {
            let mut notes = Vec::new();
            time_polyline(&timing_input(&polar, &wind, None, config), &pts, 0, 1.0, &mut notes)
                .last()
                .unwrap()
                .time_ms
        };
        assert_eq!(end(&tack) - end(&free), 300_000);
        assert_eq!(end(&gybe), end(&free), "a tack is not a gybe");
    }

    /// And a gybe its own: a broad reach either side of a northerly (120°
    /// then 240°).
    #[test]
    fn a_gybe_between_corridor_legs_costs_the_gybe_penalty() {
        let polar = Polar::parse(DEFAULT_POLAR).unwrap();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 12.0 };
        let a = here();
        let v = sail_from(a, 120.0, 3.0);
        let pts = [a, v, sail_from(v, 240.0, 3.0)];
        let free = RoutingConfig { tack_penalty_day_s: 0, gybe_penalty_day_s: 0, ..RoutingConfig::default() };
        let gybe = RoutingConfig { gybe_penalty_day_s: 200, ..free.clone() };
        let tack = RoutingConfig { tack_penalty_day_s: 200, ..free.clone() };
        let end = |config: &RoutingConfig| {
            let mut notes = Vec::new();
            time_polyline(&timing_input(&polar, &wind, None, config), &pts, 0, 1.0, &mut notes)
                .last()
                .unwrap()
                .time_ms
        };
        assert_eq!(end(&gybe) - end(&free), 200_000);
        assert_eq!(end(&tack), end(&free), "a gybe is not a tack");
    }

    /// Wind from a fixed direction, freshening linearly along a line: `tws.0`
    /// at `origin`, `tws.1` at `length` plane metres along `along`.
    struct RampWind {
        from_deg: f64,
        origin: [f64; 2],
        along: [f64; 2],
        length: f64,
        tws: (f64, f64),
    }

    impl isochrone::WindField for RampWind {
        fn wind(&self, p: [f64; 2], _time_ms: i64) -> (crate::nav::angles::Bam, f64) {
            let s = ((p[0] - self.origin[0]) * self.along[0] + (p[1] - self.origin[1]) * self.along[1])
                / self.length;
            let tws = self.tws.0 + (self.tws.1 - self.tws.0) * s.clamp(0.0, 1.0);
            (crate::nav::angles::deg_to_bam(self.from_deg), tws)
        }
    }

    /// Legs are taken two miles at a time, each piece in the wind where it
    /// begins: a 3.9 NM leg is two pieces of 1.95 NM, sailed in the wind at
    /// its start (6 kn) and at its middle (8 kn) of a breeze freshening
    /// from 6 to 10 kn along it — on the beam, so at the polar's 5.3 and
    /// 6.2 kn. Eastward and northward, so both of the leg's components count.
    #[test]
    fn a_leg_is_sailed_two_miles_at_a_time_in_the_wind_met_on_each() {
        let polar = Polar::parse(DEFAULT_POLAR).unwrap();
        let config = RoutingConfig::default();
        let (v6, v8) = (polar.speed_kt(90.0, 6.0).unwrap(), polar.speed_kt(90.0, 8.0).unwrap());
        assert!((v6 - 5.3).abs() < 1e-5 && (v8 - 6.2).abs() < 1e-5, "{v6} {v8}");
        let expected = 1.95 / v6 + 1.95 / v8;
        for (bearing, from_deg) in [(90.0, 0.0), (0.0, 90.0)] {
            let a = here();
            let b = sail_from(a, bearing, 3.9);
            let length = (b[0] - a[0]).hypot(b[1] - a[1]);
            let along = [(b[0] - a[0]) / length, (b[1] - a[1]) / length];
            let wind = RampWind { from_deg, origin: a, along, length, tws: (6.0, 10.0) };
            let (h, notes, _) = hours_and_notes(&timing_input(&polar, &wind, None, &config), &[a, b]);
            assert!(notes.is_empty(), "{notes:?}");
            assert!(
                (h - expected).abs() < 0.5 / 3600.0,
                "bearing {bearing}: {h} h, expected {expected} h"
            );
        }
    }

    /// Where the polar has nothing — a flat calm — the leg is timed under
    /// engine at 4 kn when none is configured, its point says so, and the
    /// route says so once.
    #[test]
    fn a_calm_is_motored_at_four_knots_and_said_once() {
        let polar = Polar::parse(DEFAULT_POLAR).unwrap();
        let config = RoutingConfig::default();
        assert_eq!(config.motor_speed_kt, None);
        let calm = ConstantWind { from_deg: 0.0, tws_kt: 0.0 };
        let a = here();
        // Two legs that are neither east nor north: 3000 × 4000 and back.
        let v = [a[0] + 3_000.0, a[1] + 4_000.0];
        let b = [v[0] + 4_000.0, v[1] - 3_000.0];
        let (h, notes, out) = hours_and_notes(&timing_input(&polar, &calm, None, &config), &[a, v, b]);
        assert_eq!(count(&notes, NO_WIND), 1, "{notes:?}");
        assert!(notes[0].contains("at 4 kn under engine"), "{}", notes[0]);
        let ground_nm = (crate::geo::distance_m(to_latlon(a), to_latlon(v))
            + crate::geo::distance_m(to_latlon(v), to_latlon(b)))
            / crate::geo::METRES_PER_NM;
        assert!((h - ground_nm / 4.0).abs() < 0.002 * h, "{h} h, expected {} h", ground_nm / 4.0);
        for p in &out {
            assert!(p.motoring);
            assert_eq!(p.stw_kt, 4.0);
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        /// Any leg in a calm takes its ground length at 4 kn, from whenever
        /// it leaves.
        #[test]
        fn any_calm_leg_is_motored_at_four_knots(
            dx in -9_000.0..9_000.0f64,
            dy in -9_000.0..9_000.0f64,
            depart_ms in 0i64..10 * H,
        ) {
            prop_assume!(dx.hypot(dy) > 100.0);
            let polar = Polar::parse(DEFAULT_POLAR).unwrap();
            let config = RoutingConfig::default();
            let calm = ConstantWind { from_deg: 0.0, tws_kt: 0.0 };
            let a = here();
            let b = [a[0] + dx, a[1] + dy];
            let mut notes = Vec::new();
            let out = time_polyline(&timing_input(&polar, &calm, None, &config), &[a, b], depart_ms, 1.0, &mut notes);
            let s = (out[0].time_ms - depart_ms) as f64 / 1000.0;
            let ground = crate::geo::distance_m(to_latlon(a), to_latlon(b));
            let expected = ground / (4.0 * crate::geo::METRES_PER_NM / 3600.0);
            prop_assert!((s - expected).abs() < 0.003 * expected + 0.01, "{} s, expected {} s", s, expected);
        }
    }

    /// Each point carries the heading of the leg it ends, as drawn: true
    /// bearings, since the plane is conformal.
    #[test]
    fn each_timed_point_carries_its_legs_heading() {
        let polar = Polar::parse(DEFAULT_POLAR).unwrap();
        let config = RoutingConfig::default();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 12.0 };
        let a = here();
        let v = [a[0] + 3_000.0, a[1] + 3_000.0];
        let b = [v[0] + 3_000.0, v[1] - 3_000.0];
        let c = [b[0] - 3_000.0, b[1] - 3_000.0];
        let (_, _, out) = hours_and_notes(&timing_input(&polar, &wind, None, &config), &[a, v, b, c]);
        for (p, expected) in out.iter().zip([45.0, 135.0, 225.0]) {
            assert!((p.heading_deg - expected).abs() < 1e-6, "{} vs {expected}", p.heading_deg);
        }
    }

    /// A leg under a metre long is no reason to fail: the timing still
    /// returns a point per vertex, and the sub-metre leg takes no time to
    /// speak of.
    #[test]
    fn a_sub_metre_leg_is_timed_without_failing() {
        let polar = Polar::parse(DEFAULT_POLAR).unwrap();
        let config = RoutingConfig::default();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 12.0 };
        let a = here();
        let v = [a[0] + 0.5, a[1]];
        let b = sail_from(v, 90.0, 1.0);
        let (h, _, out) = hours_and_notes(&timing_input(&polar, &wind, None, &config), &[a, v, b]);
        assert!(out[0].time_ms < 1_000, "{} ms for half a metre", out[0].time_ms);
        let reach = 1.0 / polar.speed_kt(90.0, 12.0).unwrap();
        assert!((h - reach).abs() < 2.0 / 3600.0, "{h} h, expected {reach} h");
    }
}
