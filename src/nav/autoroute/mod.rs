//! M3: the shortest safe route between two points, from the charts alone.
//!
//! No wind here — this router answers "how do I get there without hitting
//! anything", which is also its permanent job in confined waters once the
//! isochrone engine exists: the spec's Π₆ analysis shows a 5° heading fan at
//! sailing time-steps simply steps over a 0.2 nm channel, so narrow waters
//! stay grid-searched forever and the handover happens out at sea.
//!
//! The pipeline: quilt the charts' hard obstacles onto a raster (finer charts
//! erase and restate their own coverage), build the distance-to-hazard field,
//! A* with an offing floor and soft costs, pull the path taut, and hand back
//! ordinary waypoints — the same `Route` a hand-drawn passage uses.

pub mod grid;
pub mod hazards;

pub use hazards::SafetyConfig;

use crate::geo::{LatLon, METRES_PER_NM};
use crate::render::projection::Projection;
use crate::senc::{s57_code_to_acronym, ChartData, ChartInfo};

use super::model::{Route, Waypoint};
use grid::{Grid, SearchError, SearchParams};
use hazards::Severity;

/// One chart, ready to rasterize.
pub struct ChartSource<'a> {
    pub data: &'a ChartData,
    pub info: &'a ChartInfo,
}

#[derive(Debug)]
pub enum PlanError {
    /// No charts intersect the passage at all.
    NoCharts,
    /// An endpoint sits outside every chart given.
    OffGrid,
    /// An endpoint is buried in hazard further than the nudge will reach.
    EndpointBuried { which: &'static str },
    /// An endpoint lies outside every chart loaded.
    Uncharted { which: &'static str },
    /// Both ends are in water, but neither is within a mile of water that
    /// connects to the other — a closed bay, a lake, a basin behind a
    /// fixed bridge. A wider search can still join them round the outside.
    Disconnected,
    /// The sea between the endpoints is closed at this draft and offing.
    Unreachable,
    /// The user cancelled the plan.
    Cancelled,
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::NoCharts => write!(f, "no charts cover the passage"),
            PlanError::OffGrid => write!(f, "an endpoint lies outside the charted area"),
            PlanError::EndpointBuried { which } => write!(
                f,
                "the {which} is more than a mile from any water deep enough for the boat"
            ),
            PlanError::Disconnected => write!(
                f,
                "the start and finish are not in connected water: one of them is in a \
                 closed bay, a lake or a basin behind a fixed bridge — pick a point in \
                 open water or the channel"
            ),
            PlanError::Uncharted { which } => write!(
                f,
                "the {which} lies outside your charts — install the chart set that covers it"
            ),
            PlanError::Cancelled => write!(f, "cancelled"),
            PlanError::Unreachable => {
                write!(f, "no safe water connects the endpoints at this draft and offing")
            }
        }
    }
}

impl std::error::Error for PlanError {}

/// What planning produced: a route, its waypoints, and anything worth saying.
pub struct Planned {
    pub route: Route,
    pub waypoints: Vec<Waypoint>,
    pub warnings: Vec<String>,
}

/// The safe corridor between two points: the hazard grid it was found on,
/// and the path pulled taut through it.
///
/// Both routers start here. The motor router's answer *is* the corridor;
/// the weather router sails the open stretches of it by isochrones and times
/// the confined ones — so neither can produce a route this search would not,
/// and both fail, when they fail, for the same honest reason.
pub struct Corridor {
    pub grid: Grid,
    /// sec(latitude) at the middle of the passage: plane metres per ground
    /// metre, one value for the whole job.
    pub k: f64,
    pub params: SearchParams,
    /// Mercator, start to finish, every leg clear under `params`.
    pub points: Vec<[f64; 2]>,
    /// The ends as planned: the requested ones, or the nearest navigable
    /// water when a requested end was on the hard.
    pub start: LatLon,
    pub finish: LatLon,
    pub warnings: Vec<String>,
}

/// Said when a route crosses an area the chart asks care in.
pub const CAUTION_AREA_NOTE: &str =
    "the route crosses a caution, restricted or exercise area — check the chart for what applies there";

/// How far an endpoint may be moved off the hard, and how far from each end
/// the offing fades in. A mile: the length of a harbour and its approach.
const ENDPOINT_REACH_NM: f64 = 1.0;

/// How far either side of an opening bridge the offing fades in: the fender
/// walls and the approach, a couple of cable lengths.
const GATE_TAPER_NM: f64 = 0.2;

/// Half the width of the fine corridor round the coarse pass's way. A mile:
/// room for the offing to push the route off a shore the way hugs, and the
/// tile rounding adds more.
const CORRIDOR_NM: f64 = 1.0;

/// How many times the coarse pass may be sent round a place the fine grid
/// found closed, and how many coarse cells round it are closed each time.
const REPAIRS: usize = 6;
const REPAIR_RADIUS: i64 = 2;

/// Find the corridor. `margin_factor` sizes the search box as a fraction of
/// the passage's span on every side; the callers try a small box first and
/// widen it when the way round lies outside (a peninsula, an island group).
pub fn corridor(
    sources: &[ChartSource<'_>],
    start: LatLon,
    finish: LatLon,
    safety: &SafetyConfig,
    margin_factor: f64,
) -> Result<Corridor, PlanError> {
    if sources.is_empty() {
        return Err(PlanError::NoCharts);
    }
    let a = merc(start);
    let b = merc(finish);
    // Mercator metres are not ground metres: at this latitude every distance
    // in the plane is stretched by k = sec(lat). Grid arithmetic stays in the
    // plane — conformal, so *directions* are exact — while every length a
    // human named (offing, resolution, nudge reach) is converted through k.
    // One k for the whole job: over a routing box a degree tall, sec(lat)
    // varies ~2 %, noise against a 2× error ignored.
    let mid_lat = ((start.lat + finish.lat) / 2.0).to_radians();
    let k = 1.0 / mid_lat.cos();
    let (min, max) = passage_box(a, b, k, margin_factor);
    let mut coarse = build_hazard_grid(sources, min, max, safety, k);
    // MANX_ROUTE_DEBUG=<file.png>: the coarse grid as the search sees it.
    let debug_png = std::env::var("MANX_ROUTE_DEBUG").ok();
    if let Some(ref out) = debug_png {
        coarse.debug_png(&[a, b], std::path::Path::new(out), 1600);
    }

    let nm = METRES_PER_NM * k;
    let reach = ENDPOINT_REACH_NM * nm;

    let zero = SearchParams::fixed(0.0, 0.0);
    let start_cell = coarse.cell_of(a).ok_or(PlanError::OffGrid)?;
    let goal_cell = coarse.cell_of(b).ok_or(PlanError::OffGrid)?;
    for (which, cell) in [("start", start_cell), ("finish", goal_cell)] {
        if !coarse.is_covered(cell.0, cell.1)
            && grid::nudge(&coarse, cell, &zero, reach)
                .is_none_or(|c| !coarse.is_covered(c.0, c.1))
        {
            return Err(PlanError::Uncharted { which });
        }
    }

    // Two levels (spec §4.9, "tiles + adaptive resolution"). The coarse
    // grid covers the whole box but, stamped conservatively, closes any
    // channel narrower than two or three of its cells — and a fine grid over
    // the whole box would not fit in memory. So the coarse grid's *loose*
    // raster, where only a hazard's cell centres count, picks the way; a
    // fine grid held only along that way finds the route, under the strict
    // rules. Where the fine grid cannot get through, the coarse pass is told
    // so and picks again.
    for attempt in 0..=REPAIRS {
        let (cs, cg) = free_ends(&coarse, start_cell, goal_cell, reach, true)?;
        let way_params = SearchParams {
            offing_hard_m: 0.0,
            offing_min_m: safety.offing_min_nm * nm,
            offing_soft_m: safety.offing_soft_nm * nm,
            ends: vec![a, b],
            taper_m: reach,
            gates: coarse.gates.clone(),
            gate_taper_m: GATE_TAPER_NM * nm,
            loose: true,
        };
        let way = match grid::find_path(&coarse, cs, cg, &way_params) {
            Ok(p) => p,
            Err(SearchError::Cancelled) => return Err(PlanError::Cancelled),
            Err(_) => return Err(PlanError::Unreachable),
        };

        let mut along: Vec<[f64; 2]> = way.iter().map(|&(x, y)| coarse.centre(x, y)).collect();
        along.extend([a, b]);
        let mut fine = Grid::corridor(min, max, safety.fine_res_m * k, &along, CORRIDOR_NM * nm);
        stamp_hazards(&mut fine, sources, safety);

        match fine_route(&fine, a, b, reach, nm, k, safety) {
            Ok((points, params, mut warnings)) => {
                if let Some(ref out) = debug_png {
                    coarse.debug_png(&points, std::path::Path::new(out), 1600);
                    // MANX_ROUTE_DEBUG_AT=lat,lon: the fine grid there,
                    // a cell a pixel, beside the whole-box picture.
                    let at = std::env::var("MANX_ROUTE_DEBUG_AT").ok().and_then(|v| {
                        let (lat, lon) = v.split_once(',')?;
                        Some(merc(LatLon::new(lat.trim().parse().ok()?, lon.trim().parse().ok()?)))
                    });
                    if let Some(at) = at {
                        let out = std::path::Path::new(out).with_extension("fine.png");
                        fine.debug_png_at(&points, at, 200, &out);
                    }
                }
                if points.windows(2).any(|w| fine.segment_touches_soft(w[0], w[1])) {
                    warnings.push(CAUTION_AREA_NOTE.into());
                }
                let to_latlon = |p: [f64; 2]| {
                    let (lat, lon) = Projection::to_wgs84(p[0], p[1]);
                    LatLon::new(lat, lon)
                };
                let (start_at, finish_at) =
                    (to_latlon(points[0]), to_latlon(points[points.len() - 1]));
                return Ok(Corridor {
                    grid: coarse,
                    k,
                    params,
                    points,
                    start: start_at,
                    finish: finish_at,
                    warnings,
                });
            }
            Err(FineError::Plan(e)) => return Err(e),
            Err(FineError::Blocked { from }) => {
                // Teach the coarse pass what the fine grid knows along the
                // way: a coarse cell holding no fine water at all — a
                // pontoon, a quay, a wall thinner than a coarse cell — is
                // closed. Failing that, the first cell of the way the
                // start's fine water does not reach, and a little round it.
                let mut closed = 0;
                for &(x, y) in &way {
                    if !coarse.is_blocked_loose(x, y) && !holds_fine_water(&coarse, &fine, x, y) {
                        coarse.block_loose(x, y);
                        closed += 1;
                    }
                }
                if closed == 0 {
                    let water = fine.component(from, false);
                    let near_start =
                        |p: [f64; 2]| (p[0] - a[0]).hypot(p[1] - a[1]) < 3.0 * coarse.res;
                    let Some(&(bx, by)) = way.iter().find(|&&(x, y)| {
                        let p = coarse.centre(x, y);
                        !near_start(p) && fine.cell_of(p).is_none_or(|(fx, fy)| !water.get(fx, fy))
                    }) else {
                        return Err(PlanError::Unreachable);
                    };
                    for dy in -REPAIR_RADIUS..=REPAIR_RADIUS {
                        for dx in -REPAIR_RADIUS..=REPAIR_RADIUS {
                            let (x, y) = (bx as i64 + dx, by as i64 + dy);
                            if x >= 0 && y >= 0 && (x as usize) < coarse.w && (y as usize) < coarse.h {
                                coarse.block_loose(x as usize, y as usize);
                            }
                        }
                    }
                    closed = ((2 * REPAIR_RADIUS + 1) * (2 * REPAIR_RADIUS + 1)) as usize;
                }
                log::info!(
                    "auto-route: attempt {} — the fine grid found no way along the coarse one; \
                     {closed} coarse cell(s) closed",
                    attempt + 1
                );
            }
        }
    }
    Err(PlanError::Unreachable)
}

/// Does any fine cell inside coarse cell `(x, y)` hold water the strict
/// rules allow?
fn holds_fine_water(coarse: &Grid, fine: &Grid, x: usize, y: usize) -> bool {
    let lo = [coarse.origin[0] + x as f64 * coarse.res, coarse.origin[1] + y as f64 * coarse.res];
    let hi = [lo[0] + coarse.res, lo[1] + coarse.res];
    // The fine cells whose centres lie in this coarse cell: half a fine cell
    // in from each edge. Looked up at the edges themselves, rounding at
    // Mercator magnitudes put the near corner in the fine row *below* about
    // half the time, and the coarse cell was judged by its neighbour's water.
    let inset = fine.res * 0.5;
    let (Some(f0), Some(f1)) = (
        fine.cell_of([lo[0] + inset, lo[1] + inset]),
        fine.cell_of([hi[0] - inset, hi[1] - inset]),
    ) else {
        return false;
    };
    (f0.1..=f1.1).any(|fy| (f0.0..=f1.0).any(|fx| !fine.is_blocked(fx, fy)))
}

/// Why the fine pass gave no route.
enum FineError {
    /// The fine water does not join the ends; `from` is the start's fine
    /// cell, for finding where it stops.
    Blocked { from: (usize, usize) },
    /// Anything else, reported as it is.
    Plan(PlanError),
}

/// The route on the fine grid, strict rules: the ends freed into connected
/// water, A* with the offing, pulled taut, every leg re-checked. Returns the
/// points, the search's parameters, and what should be said about the ends.
fn fine_route(
    fine: &Grid,
    a: [f64; 2],
    b: [f64; 2],
    reach: f64,
    nm: f64,
    k: f64,
    safety: &SafetyConfig,
) -> Result<(Vec<[f64; 2]>, SearchParams, Vec<String>), FineError> {
    let start_cell = fine.cell_of(a).ok_or(FineError::Plan(PlanError::OffGrid))?;
    let goal_cell = fine.cell_of(b).ok_or(FineError::Plan(PlanError::OffGrid))?;
    let (start_free, goal_free) = match free_ends(fine, start_cell, goal_cell, reach, false) {
        Ok(ends) => ends,
        Err(PlanError::Disconnected) => {
            let zero = SearchParams::fixed(0.0, 0.0);
            let from = grid::nudge(fine, start_cell, &zero, reach).unwrap_or(start_cell);
            return Err(FineError::Blocked { from });
        }
        Err(e) => return Err(FineError::Plan(e)),
    };
    let mut warnings = Vec::new();
    for (which, from, to) in [("start", start_cell, start_free), ("finish", goal_cell, goal_free)] {
        if from != to {
            let why = if fine.is_blocked(from.0, from.1) {
                "is on land or in water charted shallower than the boat needs"
            } else {
                "is in water that does not connect to the rest of the passage \
                 (a fixed bridge, a lock or a closed basin)"
            };
            warnings.push(format!(
                "the {which} {why}; the route {} the nearest water that does, {:.0} m away",
                if which == "start" { "leaves from" } else { "ends at" },
                cell_dist_nm(fine, from, to) * METRES_PER_NM / k
            ));
        }
    }
    let a_free = if start_free == start_cell { a } else { fine.centre(start_free.0, start_free.1) };
    let b_free = if goal_free == goal_cell { b } else { fine.centre(goal_free.0, goal_free.1) };

    let params = SearchParams {
        // The least water between the hull and anything the chart calls
        // unsafe, anywhere away from the ends. Not rounded up to the cell
        // size: a sound two cells wide has only cells beside a hazard.
        offing_hard_m: safety.offing_hard_nm * nm,
        offing_min_m: safety.offing_min_nm * nm,
        offing_soft_m: safety.offing_soft_nm * nm,
        ends: vec![a_free, b_free],
        taper_m: reach,
        gates: fine.gates.clone(),
        gate_taper_m: GATE_TAPER_NM * nm,
        loose: false,
    };

    let path = match grid::find_path(fine, start_free, goal_free, &params) {
        Ok(p) => p,
        Err(SearchError::Unreachable) | Err(SearchError::OffGrid) => {
            return Err(FineError::Blocked { from: start_free })
        }
        Err(SearchError::Cancelled) => return Err(FineError::Plan(PlanError::Cancelled)),
    };
    let mut points = grid::string_pull(fine, &path, &params);

    // The ends the user actually asked for, where they are water: the cell
    // centre is up to half a cell off, and a route should end on its mark.
    // Only where the straight line from the exact point is itself clear.
    if points.len() >= 2 && start_free == start_cell && fine.segment_clear_by(a, points[1], &params) {
        points[0] = a;
    }
    let n = points.len();
    if n >= 2 && goal_free == goal_cell && fine.segment_clear_by(points[n - 2], b, &params) {
        points[n - 1] = b;
    }

    // Belt and braces: every emitted leg re-checked against the raster. The
    // pull already guarantees this; the check is the standing guard against
    // a future refactor, cheap enough to keep on.
    for w in points.windows(2) {
        debug_assert!(fine.segment_clear_by(w[0], w[1], &params));
        if !fine.segment_clear_by(w[0], w[1], &params) {
            return Err(FineError::Plan(PlanError::Unreachable));
        }
    }
    Ok((points, params, warnings))
}

/// Free both ends of a passage: an end on the hard — a pontoon, a quay,
/// water the chart gives less depth than the boat needs — moves to the
/// nearest water that is not. And that water must connect to the other
/// end: the nearest water to a point on Refshaleøen is an inner basin behind
/// fixed bridges, and a start snapped into it can never leave. So one end is
/// freed first, its connected water found, and the other end freed into
/// that. `loose`: in the loose raster, for the coarse pass.
fn free_ends(
    grid: &Grid,
    start_cell: (usize, usize),
    goal_cell: (usize, usize),
    reach: f64,
    loose: bool,
) -> Result<((usize, usize), (usize, usize)), PlanError> {
    let mut zero = SearchParams::fixed(0.0, 0.0);
    zero.loose = loose;
    let goal_plain = grid::nudge(grid, goal_cell, &zero, reach)
        .ok_or(PlanError::EndpointBuried { which: "finish" })?;
    let start_plain = grid::nudge(grid, start_cell, &zero, reach)
        .ok_or(PlanError::EndpointBuried { which: "start" })?;
    let goal_water = grid.component(goal_plain, loose);
    if let Some(s) = grid::nudge_into(grid, start_cell, &zero, reach, Some(&goal_water)) {
        return Ok((s, goal_plain));
    }
    let start_water = grid.component(start_plain, loose);
    match grid::nudge_into(grid, goal_cell, &zero, reach, Some(&start_water)) {
        Some(g) => Ok((start_plain, g)),
        // Neither end reaches the other's water within a mile: a wider box
        // may join them round the outside.
        None => Err(PlanError::Disconnected),
    }
}

/// The search box: the passage's bounding box grown by `margin_factor` of
/// its span on every side, and never by less than 15 km of ground.
pub fn passage_box(a: [f64; 2], b: [f64; 2], k: f64, margin_factor: f64) -> ([f64; 2], [f64; 2]) {
    let span = ((b[0] - a[0]).hypot(b[1] - a[1])).max(1_000.0);
    let margin = (span * margin_factor).max(15_000.0 * k);
    (
        [a[0].min(b[0]) - margin, a[1].min(b[1]) - margin],
        [a[0].max(b[0]) + margin, a[1].max(b[1]) + margin],
    )
}

/// The box sizes the planners try, smallest first: most passages need no
/// more than the first; a route round Sjælland or up round Skagen needs the
/// wider ones.
pub const MARGIN_FACTORS: [f64; 3] = [0.35, 1.0, 2.5];

/// Plan a safe route from `start` to `finish` across the given charts.
pub fn plan(
    sources: &[ChartSource<'_>],
    start: LatLon,
    finish: LatLon,
    safety: &SafetyConfig,
) -> Result<Planned, PlanError> {
    plan_in_box(sources, start, finish, safety, MARGIN_FACTORS[0])
}

/// [`plan`] in a box of a given size; see [`MARGIN_FACTORS`].
pub fn plan_in_box(
    sources: &[ChartSource<'_>],
    start: LatLon,
    finish: LatLon,
    safety: &SafetyConfig,
    margin_factor: f64,
) -> Result<Planned, PlanError> {
    let t0 = std::time::Instant::now();
    let corridor = corridor(sources, start, finish, safety, margin_factor)?;
    let planned = route_from_corridor(&corridor, start, finish);
    log::info!(
        "auto-route: {} waypoints over {:.1} nm in {} ms",
        planned.waypoints.len(),
        crate::geo::distance_m(start, finish) / METRES_PER_NM,
        t0.elapsed().as_millis()
    );
    Ok(planned)
}

/// The corridor as an ordinary route: its vertices as waypoints, named for
/// what they are. `start` and `finish` are the requested ends, for the name.
pub fn route_from_corridor(corridor: &Corridor, start: LatLon, finish: LatLon) -> Planned {
    let warnings = corridor.warnings.clone();
    let points: Vec<LatLon> = corridor
        .points
        .iter()
        .map(|p| {
            let (lat, lon) = Projection::to_wgs84(p[0], p[1]);
            LatLon::new(lat, lon)
        })
        .collect();

    let mut waypoints = Vec::new();
    // "to", not "→": the arrow is not in egui's default font and a route
    // name full of boxes helps nobody.
    let mut route = Route::new(format!(
        "Auto {:.3},{:.3} to {:.3},{:.3}",
        start.lat, start.lon, finish.lat, finish.lon
    ));
    for (i, p) in points.iter().enumerate() {
        let name = if i == 0 {
            "Start".to_string()
        } else if i + 1 == points.len() {
            "Finish".to_string()
        } else {
            format!("AR-{i:02}")
        };
        let wp = Waypoint::new(name, p.lat, p.lon);
        route.waypoints.push(wp.id);
        waypoints.push(wp);
    }

    // Derive the legs before handing the route out: a Planned with empty
    // legs is a route that reads as zero miles long.
    {
        let mut set = super::model::WaypointSet::default();
        for wp in &waypoints {
            set.insert(wp.clone());
        }
        route.recompute_legs(&set);
        // The corridor was cleared along straight Mercator segments.
        route.make_legs_rhumb(&set);
    }

    Planned {
        route,
        waypoints,
        warnings,
    }
}

fn merc(p: LatLon) -> [f64; 2] {
    let (x, y) = Projection::to_mercator(p.lat, p.lon);
    [x, y]
}

fn cell_dist_nm(grid: &Grid, a: (usize, usize), b: (usize, usize)) -> f64 {
    let ca = grid.centre(a.0, a.1);
    let cb = grid.centre(b.0, b.1);
    (cb[0] - ca[0]).hypot(cb[1] - ca[1]) / METRES_PER_NM
}

/// The quilted hazard grid over a box — shared by this router and the
/// weather router, so both forbid exactly the same water.
///
/// Coarse first, fine last: a finer chart erases its own coverage before
/// stamping, so where surveys disagree the better survey wins — that is
/// quilting, the same rule the display uses.
pub fn build_hazard_grid(
    sources: &[ChartSource<'_>],
    min: [f64; 2],
    max: [f64; 2],
    safety: &SafetyConfig,
    k: f64,
) -> Grid {
    let mut grid = Grid::new(min, max, safety.grid_res_m * k);
    stamp_hazards(&mut grid, sources, safety);
    grid
}

/// Quilt every chart onto `grid` — dense or a sparse corridor alike — and
/// build its distance field.
fn stamp_hazards(grid: &mut Grid, sources: &[ChartSource<'_>], safety: &SafetyConfig) {
    // Water no chart covers is not water anyone has vouched for. The grid
    // starts closed and each chart opens only its own coverage — so a gap
    // between surveys, or a chart that failed to load, is a wall, never a
    // shortcut across unsurveyed ground.
    grid.close_all();
    let mut ordered: Vec<&ChartSource> = sources.iter().collect();
    ordered.sort_by(|x, y| y.info.native_scale.cmp(&x.info.native_scale));
    let mut gates = Vec::new();
    for source in &ordered {
        stamp_chart(grid, source, safety, &mut gates);
    }
    // Opening bridges last, over every chart: the spans either side of an
    // opening (and a finer chart's piers and fenders) are stamped over the
    // cells it shares with them, and a 30 m opening is a single cell. Carved
    // any earlier, a later chart would stamp it shut again.
    for span in gates {
        if let Some(centre) = grid.open_gate(&span) {
            grid.gates.push(centre);
        }
    }
    grid.finalize();
}

/// Rasterize one chart: erase its own coverage, then stamp what it forbids.
///
/// Opening bridge spans are not carved here but collected into `gates`, one
/// list of triangles per span, for the caller to open once every chart is
/// down.
fn stamp_chart(
    grid: &mut Grid,
    source: &ChartSource<'_>,
    safety: &SafetyConfig,
    gates: &mut Vec<Vec<[[f64; 2]; 3]>>,
) {
    let info = source.info;
    let data = source.data;
    let (ref_mx, ref_my) = crate::tiles::latlon_to_mercator(info.ref_lat, info.ref_lon);

    // The eraser pass: within this chart's stated coverage, this chart is
    // the truth, so whatever a coarser chart said there is wiped — and on a
    // grid that starts closed, this is also what opens charted water.
    let mut covered = false;
    for feature in &data.features {
        if !feature.is_coverage() || feature.attribute_int("CATCOV") != Some(1) {
            continue;
        }
        if let Some(geom) = &feature.area_geometry {
            geom.for_each_triangle_global(info.ref_lat, info.ref_lon, |tri| {
                grid.clear_triangle(tri);
                covered = true;
            });
        }
    }
    if !covered {
        // A cell with no stated coverage: its extent is the best claim it
        // makes. Its own land and shoals are stamped next either way.
        let e = &info.extent_mercator;
        let (sw, se, ne, nw) = (
            [e.min_x, e.min_y],
            [e.max_x, e.min_y],
            [e.max_x, e.max_y],
            [e.min_x, e.max_y],
        );
        grid.clear_triangle([sw, se, ne]);
        grid.clear_triangle([sw, ne, nw]);
    }

    let contour = safety.safety_contour_m();
    for feature in &data.features {
        // Soundings are per-point depths, not classified features.
        if s57_code_to_acronym(feature.type_code) == "SOUNDG" {
            if let Some(mp) = &feature.multipoint_geometry {
                for p in &mp.points {
                    if (p[2] as f64) < contour {
                        grid.stamp_point([ref_mx + p[0] as f64, ref_my + p[1] as f64], true);
                    }
                }
            }
            continue;
        }
        let Some(severity) = hazards::classify(feature, safety) else {
            continue;
        };
        let hard = severity == Severity::Hard;
        if hazards::is_gate(feature, safety) {
            if let Some(geom) = &feature.area_geometry {
                let mut span = Vec::new();
                geom.for_each_triangle_global(info.ref_lat, info.ref_lon, |tri| span.push(tri));
                gates.push(span);
            }
        }
        if let Some(geom) = &feature.area_geometry {
            geom.for_each_triangle_global(info.ref_lat, info.ref_lon, |tri| {
                grid.stamp_triangle(tri, hard);
            });
        }
        if let Some(geom) = &feature.line_geometry {
            for ring in geom.resolve(&data.edge_table) {
                for w in ring.windows(2) {
                    grid.stamp_segment(
                        [ref_mx + w[0][0] as f64, ref_my + w[0][1] as f64],
                        [ref_mx + w[1][0] as f64, ref_my + w[1][1] as f64],
                        hard,
                    );
                }
            }
        }
        if let Some(pg) = &feature.point_geometry {
            // Point geometry stores lat in x, lon in y — the same convention
            // the picker reads.
            let (mx, my) = crate::tiles::latlon_to_mercator(pg.x, pg.y);
            grid.stamp_point([mx, my], hard);
        }
    }
}

#[cfg(test)]
mod tests {
    //! The router is tested on charts built by hand — coverage, land and
    //! soundings placed in plane metres and stored the way a SENC stores them
    //! — and on a public-domain NOAA harbour cell. Expected values come from
    //! what the module promises (the exact ends, every leg clear, lengths
    //! named in ground units stretched by sec(latitude), a repair that closes
    //! only what the fine grid found shut) and from geometry worked out by
    //! hand, never from what the code happens to return.

    use super::*;
    use crate::senc::{
        s57_acronym_to_code, AreaGeometry, AttributeValue, Attributes, BBox, CellExtent, EdgeRef,
        EdgeTable, Feature, FeatureType, LineGeometry, ObjectClass, SencHeader, TriPrim,
        TriPrimType,
    };
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    // ---- the test sea ----

    /// The test sea lies at 55°N, where a plane (Mercator) metre is only
    /// cos 55° = 0.57 of a ground metre: a length the router forgot to
    /// stretch by k = sec(latitude) comes out 43 % short, which no tolerance
    /// below hides.
    const LAT0: f64 = 55.0;
    const LON0: f64 = 11.0;

    /// The chart reference point, on the plane.
    fn origin() -> [f64; 2] {
        let (x, y) = crate::tiles::latlon_to_mercator(LAT0, LON0);
        [x, y]
    }

    /// Plane metres east and north of the reference point.
    fn at(east: f64, north: f64) -> [f64; 2] {
        let o = origin();
        [o[0] + east, o[1] + north]
    }

    /// The rectangle spanned by two plane positions: [min x, min y, max x, max y].
    fn rect(p: [f64; 2], q: [f64; 2]) -> [f64; 4] {
        [p[0].min(q[0]), p[1].min(q[1]), p[0].max(q[0]), p[1].max(q[1])]
    }

    fn latlon(p: [f64; 2]) -> LatLon {
        let (lat, lon) = Projection::to_wgs84(p[0], p[1]);
        LatLon::new(lat, lon)
    }

    /// An end as the router is given it, and where the router puts it.
    fn end(p: [f64; 2]) -> (LatLon, [f64; 2]) {
        let ll = latlon(p);
        (ll, merc(ll))
    }

    /// The default boat on coarser grids — 50 m and 25 m of ground — so a
    /// plan takes seconds, not minutes, in a debug build. The fine grid
    /// stays exactly half the coarse one, as in the defaults.
    fn safety() -> SafetyConfig {
        SafetyConfig { grid_res_m: 50.0, fine_res_m: 25.0, ..SafetyConfig::default() }
    }

    fn feature(acronym: &str, attrs: &[(&str, AttributeValue)]) -> Feature {
        let type_code =
            s57_acronym_to_code(acronym).unwrap_or_else(|| panic!("unknown acronym {acronym}"));
        let mut attributes = Attributes::new();
        for (k, v) in attrs {
            attributes.insert(k, v.clone());
        }
        Feature {
            type_code,
            object_class: ObjectClass::from_code(type_code),
            feature_type: FeatureType::Area,
            attributes,
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        }
    }

    /// A SENC stores geometry as f32 offsets from the chart's reference point.
    fn local(p: [f64; 2]) -> [f32; 2] {
        let o = origin();
        [(p[0] - o[0]) as f32, (p[1] - o[1]) as f32]
    }

    fn triangles(r: [f64; 4]) -> [[[f64; 2]; 3]; 2] {
        let (sw, se, ne, nw) = ([r[0], r[1]], [r[2], r[1]], [r[2], r[3]], [r[0], r[3]]);
        [[sw, se, ne], [sw, ne, nw]]
    }

    struct TestChart {
        data: ChartData,
        info: ChartInfo,
    }

    impl TestChart {
        fn source(&self) -> ChartSource<'_> {
            ChartSource { data: &self.data, info: &self.info }
        }
    }

    /// A chart built by hand, feature by feature.
    struct Sea {
        features: Vec<Feature>,
        edges: EdgeTable,
        scale: u32,
    }

    impl Sea {
        fn new() -> Self {
            Self { features: Vec::new(), edges: EdgeTable::new(), scale: 50_000 }
        }

        fn area(mut self, acronym: &str, attrs: &[(&str, AttributeValue)], r: [f64; 4]) -> Self {
            let mut f = feature(acronym, attrs);
            f.area_geometry = Some(AreaGeometry {
                extent: BBox::default(),
                triangles: vec![TriPrim {
                    prim_type: TriPrimType::Triangles,
                    vertices: triangles(r).iter().flatten().map(|&p| local(p)).collect(),
                }],
                edge_refs: Vec::new(),
            });
            self.features.push(f);
            self
        }

        /// Coverage this chart vouches for (M_COVR, CATCOV 1).
        fn covered(self, r: [f64; 4]) -> Self {
            self.area("M_COVR", &[("CATCOV", AttributeValue::Integer(1))], r)
        }

        fn land(self, r: [f64; 4]) -> Self {
            self.area("LNDARE", &[], r)
        }

        fn line(mut self, acronym: &str, attrs: &[(&str, AttributeValue)], pts: &[[f64; 2]]) -> Self {
            let id = self.edges.edges.len() as u32 + 1;
            self.edges.edges.insert(id, pts.iter().map(|&p| local(p)).collect());
            let mut f = feature(acronym, attrs);
            f.feature_type = FeatureType::Line;
            f.line_geometry = Some(LineGeometry {
                extent: BBox::default(),
                edge_refs: vec![EdgeRef {
                    start_node: 0,
                    edge_index: id as i32,
                    end_node: 0,
                    reversed: Some(false),
                }],
            });
            self.features.push(f);
            self
        }

        fn with(mut self, f: Feature) -> Self {
            self.features.push(f);
            self
        }

        fn scale(mut self, scale: u32) -> Self {
            self.scale = scale;
            self
        }

        fn chart(self, extent: [f64; 4]) -> TestChart {
            let (min_lat, min_lon) = crate::tiles::mercator_to_latlon(extent[0], extent[1]);
            let (max_lat, max_lon) = crate::tiles::mercator_to_latlon(extent[2], extent[3]);
            TestChart {
                data: ChartData {
                    header: SencHeader::default(),
                    features: self.features,
                    edge_table: self.edges,
                },
                info: ChartInfo {
                    id: 1,
                    path: "TEST.000".into(),
                    name: "TEST".into(),
                    native_scale: self.scale,
                    extent_wgs84: CellExtent { min_lat, max_lat, min_lon, max_lon },
                    extent_mercator: crate::tiles::TileBounds::new(
                        extent[0], extent[2], extent[1], extent[3],
                    ),
                    ref_lat: LAT0,
                    ref_lon: LON0,
                },
            }
        }
    }

    /// The NOAA harbour cell (US5BOSBE, Boston), read once per test run.
    fn boston() -> &'static (ChartData, ChartInfo) {
        static CELL: std::sync::OnceLock<(ChartData, ChartInfo)> = std::sync::OnceLock::new();
        CELL.get_or_init(|| {
            use crate::cache::CachedDecryptor;
            use crate::decrypt::KeyStore;
            use crate::senc::ChartCatalog;
            let dir = std::path::Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/s57-harbour"
            ));
            let cache = tempfile::tempdir().unwrap();
            let mut decryptor = CachedDecryptor::s57_only_in(cache.path().to_path_buf());
            let catalog = ChartCatalog::from_directory(dir, &KeyStore::new(), &mut decryptor)
                .expect("the harbour fixture catalogs");
            let info = catalog.charts.into_iter().next().expect("one cell");
            let data = ChartData::parse(decryptor.s57_senc(&info.path).unwrap()).unwrap();
            (data, info)
        })
    }

    /// Soundings at plane positions. A multipoint geometry can only come
    /// out of the SENC reader, so the feature is the harbour cell's own
    /// sounding feature, emptied and refilled.
    fn soundings(points: &[([f64; 2], f32)]) -> Feature {
        let mut f = boston()
            .0
            .features
            .iter()
            .find(|f| s57_code_to_acronym(f.type_code) == "SOUNDG" && f.multipoint_geometry.is_some())
            .expect("the harbour cell has soundings")
            .clone();
        f.attributes = Attributes::new();
        f.multipoint_geometry.as_mut().unwrap().points = points
            .iter()
            .map(|&(p, depth)| {
                let l = local(p);
                [l[0], l[1], depth]
            })
            .collect();
        f
    }

    // ---- geometry checks, independent of the raster ----

    /// Does the segment touch the closed rectangle? Liang–Barsky.
    fn crosses(a: [f64; 2], b: [f64; 2], r: [f64; 4]) -> bool {
        let d = [b[0] - a[0], b[1] - a[1]];
        let (mut t0, mut t1) = (0.0f64, 1.0f64);
        for axis in 0..2 {
            let (lo, hi) = (r[axis], r[axis + 2]);
            if d[axis] == 0.0 {
                if a[axis] < lo || a[axis] > hi {
                    return false;
                }
                continue;
            }
            let (mut ta, mut tb) = ((lo - a[axis]) / d[axis], (hi - a[axis]) / d[axis]);
            if ta > tb {
                std::mem::swap(&mut ta, &mut tb);
            }
            t0 = t0.max(ta);
            t1 = t1.min(tb);
            if t0 > t1 {
                return false;
            }
        }
        true
    }

    fn assert_clear_of(points: &[[f64; 2]], land: &[[f64; 4]]) {
        for w in points.windows(2) {
            for r in land {
                assert!(!crosses(w[0], w[1], *r), "leg {:?} → {:?} crosses {r:?}", w[0], w[1]);
            }
        }
    }

    fn length(points: &[[f64; 2]]) -> f64 {
        points.windows(2).map(|w| (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1])).sum()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
    }

    /// The two grids `corridor` lays over a passage, as its documentation
    /// describes them: the box from `passage_box`, cells of `grid_res_m` and
    /// `fine_res_m` ground metres stretched by k = sec(mid-latitude). Used to
    /// place test features against cell boundaries — never as an expected
    /// value.
    struct Lattice {
        min: [f64; 2],
        max: [f64; 2],
        k: f64,
        cres: f64,
        fres: f64,
    }

    fn lattice(a: [f64; 2], b: [f64; 2], safety: &SafetyConfig) -> Lattice {
        lattice_in(a, b, safety, MARGIN_FACTORS[0])
    }

    fn lattice_in(a: [f64; 2], b: [f64; 2], safety: &SafetyConfig, margin_factor: f64) -> Lattice {
        let (la, lb) = (latlon(a), latlon(b));
        let k = 1.0 / ((la.lat + lb.lat) / 2.0).to_radians().cos();
        let (min, max) = passage_box(a, b, k, margin_factor);
        Lattice { min, max, k, cres: safety.grid_res_m * k, fres: safety.fine_res_m * k }
    }

    /// Coarse cells the plan closed on its way: loosely blocked on the grid
    /// it hands back, open on a fresh grid of the same charts.
    fn closed_by_repairs(
        c: &Corridor,
        sources: &[ChartSource<'_>],
        safety: &SafetyConfig,
        lat: &Lattice,
    ) -> BTreeSet<(usize, usize)> {
        let fresh = build_hazard_grid(sources, lat.min, lat.max, safety, c.k);
        assert_eq!((fresh.w, fresh.h, fresh.origin), (c.grid.w, c.grid.h, c.grid.origin));
        let mut closed = BTreeSet::new();
        for y in 0..fresh.h {
            for x in 0..fresh.w {
                if c.grid.is_blocked_loose(x, y) && !fresh.is_blocked_loose(x, y) {
                    closed.insert((x, y));
                }
            }
        }
        closed
    }

    /// A sea with nothing in it, 40 km by 30 km.
    fn open_sea_cover() -> [f64; 4] {
        rect(at(-15_000.0, -15_000.0), at(25_000.0, 15_000.0))
    }

    fn open_sea() -> TestChart {
        Sea::new().covered(open_sea_cover()).chart(open_sea_cover())
    }

    // ---- errors, the box, the waypoints ----

    #[test]
    fn every_plan_error_says_what_went_wrong_and_names_the_end() {
        let all = [
            PlanError::NoCharts,
            PlanError::OffGrid,
            PlanError::EndpointBuried { which: "start" },
            PlanError::Uncharted { which: "finish" },
            PlanError::Disconnected,
            PlanError::Unreachable,
            PlanError::Cancelled,
        ];
        let texts: Vec<String> = all.iter().map(|e| e.to_string()).collect();
        for t in &texts {
            assert!(t.len() > 5, "{t:?}");
        }
        assert!(texts[2].contains("start"), "{}", texts[2]);
        assert!(texts[3].contains("finish"), "{}", texts[3]);
        let distinct: BTreeSet<&String> = texts.iter().collect();
        assert_eq!(distinct.len(), texts.len(), "each failure reads differently");
    }

    /// The box is the passage's bounding box grown on every side by
    /// `margin_factor` of its span, or by 15 km of ground (15 km × k of
    /// plane) when that is more. Worked by hand: a 3-4-5 passage 50 km long.
    #[test]
    fn the_search_box_grows_by_the_margin_factor_or_fifteen_km_of_ground() {
        let (a, b) = ([1_000.0, 2_000.0], [31_000.0, 42_000.0]);
        let near = |p: [f64; 2], q: [f64; 2]| (p[0] - q[0]).abs() < 1e-6 && (p[1] - q[1]).abs() < 1e-6;

        // Half the 50 km span is 25 km, more than 15 km × 1.2.
        let (min, max) = passage_box(a, b, 1.2, 0.5);
        assert!(near(min, [-24_000.0, -23_000.0]) && near(max, [56_000.0, 67_000.0]), "{min:?} {max:?}");
        // The ends either way round give the same box.
        let (min2, max2) = passage_box(b, a, 1.2, 0.5);
        assert!(near(min, min2) && near(max, max2));
        // A tenth of the span is 5 km: the 18 km ground floor wins.
        let (min, max) = passage_box(a, b, 1.2, 0.1);
        assert!(near(min, [-17_000.0, -16_000.0]) && near(max, [49_000.0, 60_000.0]), "{min:?} {max:?}");
        // Further north the floor is more plane metres.
        let (min, _) = passage_box(a, b, 2.0, 0.1);
        assert!(near(min, [-29_000.0, -28_000.0]), "{min:?}");
    }

    #[test]
    fn a_corridor_becomes_waypoints_named_start_turns_and_finish() {
        let points = vec![at(0.0, 0.0), at(1_000.0, 0.0), at(2_000.0, 500.0), at(3_000.0, 0.0)];
        let corridor = Corridor {
            grid: grid::Grid::new([0.0, 0.0], [100.0, 100.0], 10.0),
            k: 1.0,
            params: SearchParams::fixed(0.0, 0.0),
            points: points.clone(),
            start: latlon(points[0]),
            finish: latlon(points[3]),
            warnings: vec!["a note".into()],
        };
        let planned = route_from_corridor(&corridor, latlon(points[0]), latlon(points[3]));
        let names: Vec<&str> = planned.waypoints.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["Start", "AR-01", "AR-02", "Finish"]);
        for (w, p) in planned.waypoints.iter().zip(&points) {
            let want = latlon(*p);
            assert!(close(w.position.lat, want.lat) && close(w.position.lon, want.lon));
        }
        let ids: Vec<_> = planned.waypoints.iter().map(|w| w.id).collect();
        assert_eq!(planned.route.waypoints, ids);
        // The legs are there, and measure the passage.
        assert_eq!(planned.route.legs.len(), 3);
        let want: f64 = points
            .windows(2)
            .map(|w| crate::geo::distance_m(latlon(w[0]), latlon(w[1])) / METRES_PER_NM)
            .sum();
        // Generated legs are rhumb lines, measured on the sphere: over legs
        // this short the same length as the geodesic to within the 0.3 %
        // the sphere differs from the ellipsoid.
        assert!(planned.route.legs.iter().all(|l| l.kind == crate::nav::model::LegKind::RhumbLine));
        assert!((planned.route.total_distance_nm() - want).abs() < want * 0.004);
        assert_eq!(planned.warnings, ["a note"]);

        // Two points: a start and a finish, nothing between.
        let two = Corridor { points: vec![points[0], points[3]], ..corridor };
        let planned = route_from_corridor(&two, latlon(points[0]), latlon(points[3]));
        let names: Vec<&str> = planned.waypoints.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["Start", "Finish"]);
    }

    #[test]
    fn no_charts_is_an_error_not_a_route() {
        let (start, _) = end(at(0.0, 0.0));
        let (finish, _) = end(at(5_000.0, 0.0));
        assert!(matches!(corridor(&[], start, finish, &safety(), 0.35), Err(PlanError::NoCharts)));
        assert!(matches!(plan(&[], start, finish, &safety()), Err(PlanError::NoCharts)));
    }

    // ---- the hazard grid ----

    fn cell_of(g: &Grid, p: [f64; 2]) -> (usize, usize) {
        g.cell_of(p).expect("on the grid")
    }

    /// Only the coverage a chart states is opened: a cell inside the
    /// chart's extent but outside its M_COVR, or inside an M_COVR that says
    /// "no coverage" (CATCOV 2), stays closed and uncharted.
    #[test]
    fn a_chart_opens_only_the_coverage_it_states() {
        let extent = rect(at(0.0, 0.0), at(10_000.0, 10_000.0));
        let chart = Sea::new()
            .covered(rect(at(0.0, 0.0), at(5_000.0, 10_000.0)))
            .area(
                "M_COVR",
                &[("CATCOV", AttributeValue::Integer(2))],
                rect(at(6_000.0, 1_000.0), at(9_000.0, 9_000.0)),
            )
            .chart(extent);
        let g = build_hazard_grid(&[chart.source()], at(-1_000.0, -1_000.0), at(11_000.0, 11_000.0), &safety(), 1.0);
        let open = |p: [f64; 2]| {
            let (x, y) = cell_of(&g, p);
            (!g.is_blocked(x, y), g.is_covered(x, y))
        };
        assert_eq!(open(at(2_500.0, 5_000.0)), (true, true), "stated coverage is water");
        assert_eq!(open(at(5_500.0, 5_000.0)), (false, false), "in the extent, not the coverage");
        assert_eq!(open(at(7_500.0, 5_000.0)), (false, false), "stated as not covered");
        assert_eq!(open(at(10_500.0, 5_000.0)), (false, false), "outside the chart");
    }

    /// A chart that states no coverage at all is taken at its extent.
    #[test]
    fn a_chart_without_coverage_records_opens_its_extent() {
        let extent = rect(at(0.0, 0.0), at(10_000.0, 10_000.0));
        let chart = Sea::new().land(rect(at(4_000.0, 4_000.0), at(6_000.0, 6_000.0))).chart(extent);
        let g = build_hazard_grid(&[chart.source()], at(-1_000.0, -1_000.0), at(11_000.0, 11_000.0), &safety(), 1.0);
        let (x, y) = cell_of(&g, at(2_000.0, 2_000.0));
        assert!(!g.is_blocked(x, y) && g.is_covered(x, y));
        let (x, y) = cell_of(&g, at(5_000.0, 5_000.0));
        assert!(g.is_blocked(x, y), "its own land is still stamped");
        let (x, y) = cell_of(&g, at(10_500.0, 5_000.0));
        assert!(g.is_blocked(x, y) && !g.is_covered(x, y));
    }

    /// What one chart forbids, cell by cell, on a 50 m grid (k = 1): land,
    /// a depth area shallower than the boat's 2.5 m contour, a sounding
    /// shallower than it (but not one exactly at it: §4.3 rejects a cell
    /// with a sounding *less than* the safety depth), and every cell a pier
    /// runs through. A firing range is a cost, not a wall.
    #[test]
    fn land_shoals_soundings_and_piers_close_their_cells_and_a_firing_range_only_costs() {
        let cover = rect(at(-1_000.0, -1_000.0), at(11_000.0, 11_000.0));
        let (pier_a, pier_b) = (at(6_130.0, 7_270.0), at(8_910.0, 8_340.0));
        let (shoal, at_contour, deep) = (at(1_230.0, 8_770.0), at(2_340.0, 8_620.0), at(3_410.0, 8_710.0));
        let chart = Sea::new()
            .covered(cover)
            .land(rect(at(500.0, 500.0), at(2_000.0, 2_000.0)))
            .area("DEPARE", &[("DRVAL1", AttributeValue::Float(1.0))], rect(at(3_000.0, 500.0), at(4_000.0, 2_000.0)))
            .area("DEPARE", &[("DRVAL1", AttributeValue::Float(10.0))], rect(at(5_000.0, 500.0), at(6_000.0, 2_000.0)))
            .area("MIPARE", &[], rect(at(7_000.0, 500.0), at(8_000.0, 2_000.0)))
            .line("SLCONS", &[], &[pier_a, pier_b])
            .with(soundings(&[(shoal, 1.0), (at_contour, 2.5), (deep, 10.0)]))
            .chart(cover);
        let s = safety();
        assert_eq!(s.safety_contour_m(), 2.5);
        let g = build_hazard_grid(&[chart.source()], at(-2_000.0, -2_000.0), at(12_000.0, 12_000.0), &s, 1.0);
        assert!(close(g.res, 50.0), "k = 1: cells of grid_res_m");
        let blocked = |p: [f64; 2]| {
            let (x, y) = cell_of(&g, p);
            g.is_blocked(x, y)
        };
        assert!(blocked(at(1_250.0, 1_250.0)), "land");
        assert!(blocked(at(3_500.0, 1_250.0)), "a 1 m depth area");
        assert!(!blocked(at(5_500.0, 1_250.0)), "a 10 m depth area");
        assert!(!blocked(at(7_500.0, 1_250.0)), "a firing range is no wall");
        let (x, y) = cell_of(&g, at(7_500.0, 1_250.0));
        assert!(g.soft_cost(x, y) > 0.0, "but it costs");
        let (x, y) = cell_of(&g, at(1_250.0, 1_250.0));
        assert_eq!(g.soft_cost(x, y), 0.0, "land is no cost, it is a wall");
        assert!(!blocked(at(9_000.0, 5_000.0)), "open water");

        assert!(blocked(shoal), "a 1 m sounding");
        assert!(!blocked(at_contour), "a sounding exactly at the contour");
        assert!(!blocked(deep), "a 10 m sounding");
        // The sounding's own cell, not a neighbour.
        assert!(!blocked([shoal[0] + 120.0, shoal[1]]) && !blocked([shoal[0], shoal[1] - 120.0]));

        for t in [0.0, 0.02, 0.5, 0.98, 1.0] {
            let p = [pier_a[0] + (pier_b[0] - pier_a[0]) * t, pier_a[1] + (pier_b[1] - pier_a[1]) * t];
            assert!(blocked(p), "the pier at {t}");
        }
        let mid = [(pier_a[0] + pier_b[0]) / 2.0, (pier_a[1] + pier_b[1]) / 2.0];
        assert!(!blocked([mid[0], mid[1] + 200.0]), "beside the pier");
    }

    /// The coarse grid's cells are `grid_res_m` of ground, stretched to the
    /// plane by k.
    #[test]
    fn hazard_grid_cells_are_grid_res_of_ground_stretched_by_k() {
        let chart = open_sea();
        let g = build_hazard_grid(&[chart.source()], at(0.0, 0.0), at(5_000.0, 5_000.0), &safety(), 1.75);
        assert!(close(g.res, 50.0 * 1.75));
        assert_eq!(g.origin, at(0.0, 0.0));
    }

    /// Quilting: a finer chart (smaller scale number) is stamped after a
    /// coarser one and its coverage wipes what the coarse chart said there.
    #[test]
    fn a_finer_chart_reopens_water_a_coarser_one_called_land() {
        let cover = rect(at(0.0, 0.0), at(10_000.0, 10_000.0));
        let coarse = Sea::new().covered(cover).land(rect(at(4_000.0, 0.0), at(6_000.0, 10_000.0))).scale(200_000).chart(cover);
        let fine_cover = rect(at(3_000.0, 3_000.0), at(7_000.0, 7_000.0));
        let fine = Sea::new().covered(fine_cover).scale(20_000).chart(fine_cover);
        // Either order in the list: the scale decides.
        for sources in [[coarse.source(), fine.source()], [fine.source(), coarse.source()]] {
            let g = build_hazard_grid(&sources, at(-1_000.0, -1_000.0), at(11_000.0, 11_000.0), &safety(), 1.0);
            let (x, y) = cell_of(&g, at(5_000.0, 5_000.0));
            assert!(!g.is_blocked(x, y), "the finer survey wins");
            let (x, y) = cell_of(&g, at(5_000.0, 9_000.0));
            assert!(g.is_blocked(x, y), "outside it the coarse land stands");
        }
    }

    // ---- planning in open water ----

    /// Nothing in the way: one leg, from exactly the requested start to
    /// exactly the requested finish, and every length the search uses is a
    /// ground length stretched to the plane by k = sec(mid-latitude).
    #[test]
    fn open_water_is_one_straight_leg_between_the_exact_ends() {
        let chart = open_sea();
        let sources = [chart.source()];
        let (start, a) = end(at(0.0, 0.0));
        let (finish, b) = end(at(10_000.0, 3_000.0));
        let s = safety();
        let c = corridor(&sources, start, finish, &s, MARGIN_FACTORS[0]).expect("open water");
        assert_eq!(c.points, vec![a, b]);
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        assert!(close(c.start.lat, start.lat) && close(c.start.lon, start.lon));
        assert!(close(c.finish.lat, finish.lat) && close(c.finish.lon, finish.lon));

        let k = 1.0 / ((start.lat + finish.lat) / 2.0).to_radians().cos();
        assert!((k - 1.743).abs() < 0.01, "sec 55° is 1.743: {k}");
        assert!(close(c.k, k), "{} vs {k}", c.k);
        let nm = 1852.0 * k;
        assert!(close(c.params.offing_hard_m, 0.005 * nm), "{}", c.params.offing_hard_m);
        assert!(close(c.params.offing_min_m, 0.2 * nm), "{}", c.params.offing_min_m);
        assert!(close(c.params.offing_soft_m, 0.5 * nm), "{}", c.params.offing_soft_m);
        assert!(close(c.params.taper_m, nm), "the offing fades over a mile: {}", c.params.taper_m);
        assert!(close(c.params.gate_taper_m, 0.2 * nm), "{}", c.params.gate_taper_m);
        assert_eq!(c.params.ends, vec![a, b]);
        assert!(!c.params.loose);
        assert!(close(c.grid.res, 50.0 * k), "{}", c.grid.res);
        assert_eq!(c.grid.origin, passage_box(a, b, k, MARGIN_FACTORS[0]).0);

        let planned = plan(&sources, start, finish, &s).expect("open water");
        let names: Vec<&str> = planned.waypoints.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["Start", "Finish"]);
        let want = crate::geo::distance_m(start, finish) / METRES_PER_NM;
        // Generated legs are rhumb lines, measured on the sphere: over legs
        // this short the same length as the geodesic to within the 0.3 %
        // the sphere differs from the ellipsoid.
        assert!(planned.route.legs.iter().all(|l| l.kind == crate::nav::model::LegKind::RhumbLine));
        assert!((planned.route.total_distance_nm() - want).abs() < want * 0.004);
        assert!(planned.warnings.is_empty());
    }

    fn island() -> [f64; 4] {
        rect(at(4_000.0, -1_500.0), at(6_000.0, 1_500.0))
    }

    fn island_sea() -> TestChart {
        Sea::new().covered(open_sea_cover()).land(island()).chart(open_sea_cover())
    }

    /// Is a point the centre of a cell of the fine grid — `fine_res_m` of
    /// ground, stretched by k, laid from the box's corner?
    fn on_fine_lattice(p: [f64; 2], lat: &Lattice) -> bool {
        (0..2).all(|i| {
            let n = (p[i] - lat.min[i]) / lat.fres - 0.5;
            (n - n.round()).abs() < 1e-6
        })
    }

    #[test]
    fn a_route_goes_round_an_island_and_never_crosses_it() {
        let chart = island_sea();
        let sources = [chart.source()];
        let (start, a) = end(at(0.0, 0.0));
        let (finish, b) = end(at(10_000.0, 0.0));
        let s = safety();
        let c = corridor(&sources, start, finish, &s, MARGIN_FACTORS[0]).expect("round the island");
        let n = c.points.len();
        assert!(n >= 3, "it must turn: {:?}", c.points);
        assert_eq!((c.points[0], c.points[n - 1]), (a, b));
        assert_clear_of(&c.points, &[island()]);
        let len = length(&c.points);
        assert!(len > 10_000.0 && len < 13_000.0, "{len}");
        // The turns are where the fine grid's search turned.
        let lat = lattice(a, b, &s);
        for p in &c.points[1..n - 1] {
            assert!(on_fine_lattice(*p, &lat), "{p:?} is no fine cell centre");
        }
    }

    /// Three walls, alternately from the south and the north: the route
    /// snakes, and still ends exactly on its mark.
    #[test]
    fn a_winding_route_still_ends_exactly_on_its_mark() {
        let walls = [
            rect(at(3_000.0, -20_000.0), at(3_300.0, 1_000.0)),
            rect(at(6_000.0, -1_000.0), at(6_300.0, 20_000.0)),
            rect(at(9_000.0, -20_000.0), at(9_300.0, 1_000.0)),
        ];
        let mut sea = Sea::new().covered(open_sea_cover());
        for w in walls {
            sea = sea.land(w);
        }
        let chart = sea.chart(open_sea_cover());
        let sources = [chart.source()];
        let (start, a) = end(at(0.0, -3_000.0));
        let (finish, b) = end(at(12_000.0, -3_000.0));
        let c = corridor(&sources, start, finish, &safety(), MARGIN_FACTORS[0]).expect("a way through");
        let n = c.points.len();
        assert!(n >= 5, "{:?}", c.points);
        assert_eq!((c.points[0], c.points[n - 1]), (a, b));
        assert_clear_of(&c.points, &walls);
    }

    // ---- ends on the hard, off the chart, buried ----

    /// The number of metres a warning reports.
    fn metres_in(warning: &str) -> f64 {
        let before = warning.split(" m away").next().expect("a distance");
        before.rsplit(' ').next().unwrap().parse().expect("a number")
    }

    /// A start 300 m (of ground) inside a coast moves to the nearest water,
    /// and the skipper is told how far — 300 m, give or take the 25 m
    /// cells either side of the shore.
    #[test]
    fn a_start_on_land_leaves_from_the_nearest_water_and_says_how_far() {
        let cover = open_sea_cover();
        let k = 1.0 / LAT0.to_radians().cos();
        let chart = Sea::new().covered(cover).land(rect(at(-20_000.0, -20_000.0), at(0.0, 20_000.0))).chart(cover);
        let sources = [chart.source()];
        let (start, a) = end(at(-300.0 * k, 0.0));
        let (finish, b) = end(at(10_000.0, 0.0));
        let s = safety();
        let c = corridor(&sources, start, finish, &s, MARGIN_FACTORS[0]).expect("off the beach");
        assert_eq!(c.warnings.len(), 1, "{:?}", c.warnings);
        let w = &c.warnings[0];
        assert!(w.starts_with("the start is on land") && w.contains("leaves from"), "{w}");
        let m = metres_in(w);
        assert!((295.0..=355.0).contains(&m), "{m} m in {w:?}");
        // The route starts in the water, where it says.
        let p0 = c.points[0];
        assert!(p0 != a && p0[0] > at(0.0, 0.0)[0], "{p0:?}");
        let moved = (p0[0] - a[0]).hypot(p0[1] - a[1]) / k;
        assert!((moved - m).abs() < 30.0, "{moved} vs {m}");
        assert_eq!(merc(c.start)[0].round(), p0[0].round());
        assert_eq!(*c.points.last().unwrap(), b);
        assert_eq!(c.params.ends[0], p0, "the offing fades round where the route starts");
        assert!(on_fine_lattice(p0, &lattice(a, b, &s)), "{p0:?}");

        let planned = plan(&sources, start, finish, &s).unwrap();
        assert_eq!(planned.warnings, c.warnings);
    }

    /// The same at the finish, on a coast running east–west: the finish
    /// moves north, off the beach, and the skipper is told how far.
    #[test]
    fn a_finish_on_land_ends_at_the_nearest_water_and_says_how_far() {
        let cover = open_sea_cover();
        let k = 1.0 / LAT0.to_radians().cos();
        let chart = Sea::new().covered(cover).land(rect(at(-20_000.0, -20_000.0), at(30_000.0, 0.0))).chart(cover);
        let sources = [chart.source()];
        let (start, a) = end(at(0.0, 8_000.0));
        let (finish, b) = end(at(6_000.0, -300.0 * k));
        let s = safety();
        let c = corridor(&sources, start, finish, &s, MARGIN_FACTORS[0]).expect("onto the beach");
        assert_eq!(c.warnings.len(), 1, "{:?}", c.warnings);
        let w = &c.warnings[0];
        assert!(w.starts_with("the finish is on land") && w.contains("ends at"), "{w}");
        let m = metres_in(w);
        assert!((295.0..=355.0).contains(&m), "{m} m in {w:?}");
        let last = *c.points.last().unwrap();
        assert!(last != b && last[1] > at(0.0, 0.0)[1], "{last:?}");
        let moved = (last[0] - b[0]).hypot(last[1] - b[1]) / c.k;
        assert!((moved - m).abs() < 30.0, "{moved} vs {m}");
        assert_eq!(c.points[0], a);
        assert_eq!(c.params.ends[1], last);
    }

    #[test]
    fn an_end_outside_every_chart_is_uncharted_and_named() {
        let chart = open_sea();
        let sources = [chart.source()];
        let s = safety();
        let (inside, _) = end(at(0.0, 0.0));
        let (outside, _) = end(at(40_000.0, 0.0));
        let e = corridor(&sources, inside, outside, &s, 0.35).err().expect("uncharted");
        assert!(matches!(e, PlanError::Uncharted { which: "finish" }), "{e:?}");
        let e = corridor(&sources, outside, inside, &s, 0.35).err().expect("uncharted");
        assert!(matches!(e, PlanError::Uncharted { which: "start" }), "{e:?}");
    }

    /// Half a kilometre off the chart's edge is within the mile an end may
    /// be moved: the route finishes in the charted water nearest it.
    #[test]
    fn an_end_just_off_the_chart_moves_onto_it() {
        let chart = open_sea();
        let sources = [chart.source()];
        let (start, a) = end(at(15_000.0, 0.0));
        let (finish, b) = end(at(25_500.0, 0.0));
        let c = corridor(&sources, start, finish, &safety(), 0.35).expect("onto the chart");
        let last = *c.points.last().unwrap();
        // Within the chart, give or take the half of a fine cell its edge
        // opens whole.
        assert!(last[0] <= at(25_000.0, 0.0)[0] + 25.0 && (last[0] - b[0]).abs() < 700.0, "{last:?}");
        assert_eq!(c.points[0], a);
        assert_eq!(c.warnings.len(), 1);
        assert!(c.warnings[0].starts_with("the finish"), "{:?}", c.warnings);
    }

    /// An end in the middle of a 15 km island, inside the chart, is buried
    /// — more than the mile's reach from any water — not uncharted.
    #[test]
    fn an_end_deep_inland_is_buried_not_uncharted() {
        let cover = open_sea_cover();
        let chart = Sea::new().covered(cover).land(rect(at(5_000.0, -8_000.0), at(20_000.0, 8_000.0))).chart(cover);
        let sources = [chart.source()];
        let s = safety();
        let (sea, _) = end(at(0.0, 0.0));
        let (inland, _) = end(at(12_500.0, 0.0));
        let e = corridor(&sources, sea, inland, &s, 0.35).err().expect("buried");
        assert!(matches!(e, PlanError::EndpointBuried { which: "finish" }), "{e:?}");
        let e = corridor(&sources, inland, sea, &s, 0.35).err().expect("buried");
        assert!(matches!(e, PlanError::EndpointBuried { which: "start" }), "{e:?}");
    }

    /// A lake (8–12 km east, ±2 km) inside 4 km of land all round: water at
    /// both ends, joined by none, and neither within the mile an end may be
    /// moved of the other's water.
    #[test]
    fn ends_in_waters_that_never_meet_are_disconnected() {
        let cover = open_sea_cover();
        let chart = Sea::new()
            .covered(cover)
            .land(rect(at(4_000.0, -6_000.0), at(16_000.0, -2_000.0)))
            .land(rect(at(4_000.0, 2_000.0), at(16_000.0, 6_000.0)))
            .land(rect(at(4_000.0, -2_000.0), at(8_000.0, 2_000.0)))
            .land(rect(at(12_000.0, -2_000.0), at(16_000.0, 2_000.0)))
            .chart(cover);
        let sources = [chart.source()];
        let (in_lake, _) = end(at(10_000.0, 0.0));
        let (at_sea, _) = end(at(22_000.0, 0.0));
        let e = corridor(&sources, in_lake, at_sea, &safety(), 0.35).err().expect("disconnected");
        assert!(matches!(e, PlanError::Disconnected), "{e:?}");
    }

    // ---- teaching the coarse pass what the fine grid found ----

    /// The two axes of a scenario: `v` along the passage and `u` across it.
    /// Unturned, v runs north and u east; turned, v runs east and u north —
    /// the same scenario on its side, so a rule about one axis cannot hide.
    #[derive(Clone, Copy, Debug)]
    struct Axes {
        turned: bool,
    }

    impl Axes {
        fn p(self, u: f64, v: f64) -> [f64; 2] {
            if self.turned { at(v, u) } else { at(u, v) }
        }
        fn r(self, u0: f64, v0: f64, u1: f64, v1: f64) -> [f64; 4] {
            rect(self.p(u0, v0), self.p(u1, v1))
        }
        /// A plane position as (u, v) from the reference point.
        fn uv(self, p: [f64; 2]) -> (f64, f64) {
            let o = origin();
            let (x, y) = (p[0] - o[0], p[1] - o[1]);
            if self.turned { (y, x) } else { (x, y) }
        }
        /// Cell (u index, v index) as grid (x, y).
        fn cell(self, i: usize, j: usize) -> (usize, usize) {
            if self.turned { (j, i) } else { (i, j) }
        }
    }

    struct Scenario {
        chart: TestChart,
        start: LatLon,
        finish: LatLon,
        a: [f64; 2],
        b: [f64; 2],
        land: Vec<[f64; 4]>,
        lat: Lattice,
        /// What the plan should close on the coarse grid.
        closed: BTreeSet<(usize, usize)>,
    }

    impl Scenario {
        fn plan(&self, s: &SafetyConfig) -> (Corridor, BTreeSet<(usize, usize)>) {
            let sources = [self.chart.source()];
            let c = corridor(&sources, self.start, self.finish, s, MARGIN_FACTORS[0])
                .unwrap_or_else(|e| panic!("{e:?}"));
            let closed = closed_by_repairs(&c, &sources, s, &self.lat);
            (c, closed)
        }
    }

    /// A 1 km isthmus across the passage, split along the start's coarse
    /// column by a crack 3 m wide — two quays no hull fits between — and
    /// cut 10–12 km away by a real 2 km channel. The coarse grid samples
    /// cell centres and sees the crack as a way through; the fine grid sees
    /// no water in it at all. The crack's ends sit 2 m inside coarse cell
    /// boundaries, so its last cell has open water just past its edge.
    fn cracked_isthmus(ax: Axes) -> Scenario {
        let s = safety();
        let (start, a) = end(ax.p(0.0, -6_000.0));
        let (finish, b) = end(ax.p(0.0, 6_000.0));
        let lat = lattice(a, b, &s);
        let (u0, v0) = ax.uv(lat.min);
        let (au, _) = ax.uv(a);
        let i = ((au - u0) / lat.cres + 0.5).floor();
        let crack = u0 + (i + 0.5) * lat.cres;
        let m0 = ((-500.0 - v0) / lat.cres).floor();
        let m1 = m0 + 12.0;
        let (south, north) = (v0 + m0 * lat.cres + 2.0, v0 + m1 * lat.cres - 2.0);
        // The quays reach one fine row further south than the isthmus, so
        // the crack's first cell has no water on the fine row just below
        // it either (see `a_coarse_cell_is_judged_by_its_own_fine_cells`).
        let quay_south = south - lat.fres;
        let land = vec![
            ax.r(-40_000.0, quay_south, crack - 1.5, north),
            ax.r(crack + 1.5, quay_south, 10_000.0, north),
            ax.r(12_000.0, south, 40_000.0, north),
        ];
        let cover = ax.r(-20_000.0, -15_000.0, 25_000.0, 15_000.0);
        let mut sea = Sea::new().covered(cover);
        for r in &land {
            sea = sea.land(*r);
        }
        let closed = (m0 as usize..m1 as usize).map(|j| ax.cell(i as usize, j)).collect();
        Scenario { chart: sea.chart(cover), start, finish, a, b, land, lat, closed }
    }

    /// A coarse cell holding no fine water at all is closed on the coarse
    /// grid — exactly those cells of the coarse pass's way, no others — and
    /// the route goes round by the channel.
    #[test]
    fn a_crack_no_hull_fits_is_closed_on_the_coarse_grid_and_the_route_goes_round() {
        let s = safety();
        for ax in [Axes { turned: false }, Axes { turned: true }] {
            let sc = cracked_isthmus(ax);
            let (c, closed) = sc.plan(&s);
            assert_clear_of(&c.points, &sc.land);
            assert_eq!((c.points[0], *c.points.last().unwrap()), (sc.a, sc.b));
            assert!(c.points.iter().any(|&p| ax.uv(p).0 > 9_000.0), "{ax:?}: by the channel");
            assert_eq!(closed, sc.closed, "{ax:?}");
        }
    }

    /// A harbour wall 3 coarse cells thick across the passage with one
    /// entrance a coarse cell wide, closed by a pontoon 2 m thick moored
    /// across it; a real 2 km gap 10–12 km away. The pontoon lies between
    /// coarse cell centres — the coarse grid never sees it — in the fine
    /// row on the start's side of its coarse cell's centre, so the entrance
    /// cell still holds fine water beyond it. The start is to the north.
    ///
    /// `quay`: the start is instead on a quay just east of the entrance's
    /// column, two coarse cells in from the quay's edge: the coarse pass
    /// leaves from the cell at the quay's edge, water whose fine cells the
    /// quay's own edge has closed.
    fn boomed_entrance(ax: Axes, quay: bool) -> Scenario {
        let s = safety();
        let (finish, b) = end(ax.p(0.0, -6_000.0));
        let lat = lattice(merc(latlon(ax.p(0.0, 6_000.0))), b, &s);
        let (u0, v0) = ax.uv(lat.min);
        let i = ((0.0 - u0) / lat.cres).round();
        let j = ((0.0 - v0) / lat.cres).floor();
        let (west, east) = (u0 + i * lat.cres, u0 + (i + 1.0) * lat.cres);
        let (wall_s, wall_n) = (v0 + (j - 1.0) * lat.cres + 2.0, v0 + (j + 2.0) * lat.cres - 2.0);
        let boom = v0 + j * lat.cres + lat.fres * 1.25;
        let mut land = vec![
            ax.r(-40_000.0, wall_s, west - 2.0, wall_n),
            ax.r(east + 2.0, wall_s, 10_000.0, wall_n),
            ax.r(12_000.0, wall_s, 40_000.0, wall_n),
        ];
        let pontoon = ax.r(west - 10.0, boom, east + 10.0, boom + 2.0);
        let mut start_at = ax.p(0.0, 6_000.0);
        if quay {
            assert!(!ax.turned, "the quay variant keeps the lattice still only unturned");
            // A column east of the entrance whose centre falls, on the fine
            // grid, in the fine cell east of it: the one the quay's edge
            // touches.
            let coarse = Grid::new(lat.min, lat.max, lat.cres);
            let fine = Grid::new(lat.min, lat.max, lat.fres);
            let (_, av) = ax.uv(start_at);
            let r = ((av - v0) / lat.cres).floor();
            let c = (i as usize + 1..i as usize + 9)
                .find(|&c| fine.cell_of(coarse.centre(c, r as usize)).unwrap().0 == 2 * c + 1)
                .expect("a column whose centre rounds east");
            let edge = u0 + (c as f64 + 0.5) * lat.cres;
            start_at = ax.p(edge + 2.0 * lat.cres, av);
            land.push(ax.r(edge + 2.0, v0 + (r - 2.0) * lat.cres + 2.0, edge + 3_000.0, v0 + (r + 3.0) * lat.cres - 2.0));
        }
        let (start, a) = end(start_at);
        // The lattice is the one the plan will use: the start lies north and
        // east of the finish, so the box's corner is the finish's.
        let check = lattice(a, b, &s);
        assert!((0..2).all(|n| (check.min[n] - lat.min[n]).abs() < 1e-6), "{:?} {:?}", check.min, lat.min);
        assert!(close(check.cres, lat.cres));
        let lat = check;
        let cover = ax.r(-20_000.0, -15_000.0, 25_000.0, 15_000.0);
        let mut sea = Sea::new().covered(cover).area("PONTON", &[], pontoon);
        for r in &land {
            sea = sea.land(*r);
        }
        let chart = sea.chart(cover);
        // What the repair closes: the (2R+1)² cells round the entrance's
        // cell, where they were open.
        let fresh = build_hazard_grid(&[chart.source()], lat.min, lat.max, &s, lat.k);
        let mut closed = BTreeSet::new();
        for dj in -REPAIR_RADIUS..=REPAIR_RADIUS {
            for di in -REPAIR_RADIUS..=REPAIR_RADIUS {
                let (x, y) = ax.cell((i as i64 + di) as usize, (j as i64 + dj) as usize);
                if !fresh.is_blocked_loose(x, y) {
                    closed.insert((x, y));
                }
            }
        }
        land.push(pontoon);
        Scenario { chart, start, finish, a, b, land, lat, closed }
    }

    /// Where every coarse cell of the way holds fine water but the water
    /// still does not join — a pontoon moored across an entrance — the coarse
    /// pass is sent round the first cell the start's water does not reach:
    /// that cell and the (2R+1)² round it are closed, nothing else, and the
    /// route goes by the real gap.
    #[test]
    fn an_entrance_closed_by_a_pontoon_is_closed_round_where_the_water_stops() {
        let s = safety();
        for ax in [Axes { turned: false }, Axes { turned: true }] {
            let sc = boomed_entrance(ax, false);
            let (c, closed) = sc.plan(&s);
            assert_clear_of(&c.points, &sc.land);
            assert_eq!((c.points[0], *c.points.last().unwrap()), (sc.a, sc.b));
            assert!(c.points.iter().any(|&p| ax.uv(p).0 > 9_000.0), "{ax:?}: by the gap");
            assert_eq!(closed, sc.closed, "{ax:?}");
        }
    }

    /// The same, leaving from a quay: the water round where the route
    /// starts is the start's own, and is never what gets closed.
    #[test]
    fn the_water_round_the_start_is_never_what_a_repair_closes() {
        let s = safety();
        let sc = boomed_entrance(Axes { turned: false }, true);
        let (c, closed) = sc.plan(&s);
        assert_clear_of(&c.points, &sc.land);
        assert_eq!(c.warnings.len(), 1, "{:?}", c.warnings);
        assert_eq!(*c.points.last().unwrap(), sc.b);
        assert_eq!(closed, sc.closed);
    }

    // ---- the exact ends, only where the line from them is clear ----

    /// A rock (a 2 m islet) a few fine cells from the passage's north-east
    /// end, placed so the straight line from the exact end point runs
    /// through it while the line from the centre of the end's cell clears
    /// it. Found by trying the cells along the passage on a grid laid out
    /// like the fine one.
    struct Clip {
        chart: TestChart,
        p: [f64; 2],
        q: [f64; 2],
        rock: [f64; 4],
        reference: Grid,
    }

    fn clipped_end(start_at_q: bool) -> Clip {
        let s = safety();
        let p = merc(latlon(at(0.0, 0.0)));
        let params = SearchParams::fixed(0.0, 0.0);
        for dy in 0..40 {
            let q = merc(latlon(at(12_000.0, 800.0 + 7.0 * dy as f64)));
            let lat = lattice(p, q, &s);
            let n = |v: f64, lo: f64| ((v - lo) / lat.fres).floor() - 3.0;
            let lo = [lat.min[0] + n(p[0], lat.min[0]) * lat.fres, lat.min[1] + n(p[1], lat.min[1]) * lat.fres];
            let hi = [q[0] + 3.0 * lat.fres, q[1] + 3.0 * lat.fres];
            let scratch = Grid::new(lo, hi, lat.fres);
            let (pc, qc) = (cell_of(&scratch, p), cell_of(&scratch, q));
            let (cp, cq) = (scratch.centre(pc.0, pc.1), scratch.centre(qc.0, qc.1));
            for step in 4..160 {
                let t = step as f64 * 0.25 * lat.fres / (q[0] - p[0]).hypot(q[1] - p[1]);
                let on = [q[0] + (p[0] - q[0]) * t, q[1] + (p[1] - q[1]) * t];
                let (rx, ry) = cell_of(&scratch, on);
                let rc = scratch.centre(rx, ry);
                let rock = rect([rc[0] - 1.0, rc[1] - 1.0], [rc[0] + 1.0, rc[1] + 1.0]);
                let mut g = Grid::new(lo, hi, lat.fres);
                for t in triangles(rock) {
                    g.stamp_triangle(t, true);
                }
                let clear = |a: [f64; 2], b: [f64; 2]| g.segment_clear_by(a, b, &params);
                let fits = if start_at_q {
                    clear(cq, cp) && clear(cq, p) && !clear(q, cp) && !clear(q, p)
                } else {
                    clear(cp, cq) && clear(p, cq) && !clear(p, q) && !clear(cp, q)
                };
                if fits {
                    let cover = open_sea_cover();
                    let chart = Sea::new().covered(cover).land(rock).chart(cover);
                    g.finalize();
                    return Clip { chart, p, q, rock, reference: g };
                }
            }
        }
        panic!("no rock placement found");
    }

    /// The route starts on the exact start point only where the straight
    /// line from it is clear; here it is not, so the route leaves from the
    /// centre of the start's cell, and no leg touches the rock.
    #[test]
    fn the_exact_start_is_kept_only_where_the_line_from_it_is_clear() {
        let clip = clipped_end(true);
        let sources = [clip.chart.source()];
        let c = corridor(&sources, latlon(clip.q), latlon(clip.p), &safety(), 0.35).expect("a route");
        assert_ne!(c.points[0], clip.q);
        assert_eq!(*c.points.last().unwrap(), clip.p);
        assert_clear_of(&c.points, &[clip.rock]);
        let params = SearchParams::fixed(0.0, 0.0);
        for w in c.points.windows(2) {
            assert!(clip.reference.segment_clear_by(w[0], w[1], &params));
        }
    }

    #[test]
    fn the_exact_finish_is_kept_only_where_the_line_to_it_is_clear() {
        let clip = clipped_end(false);
        let sources = [clip.chart.source()];
        let c = corridor(&sources, latlon(clip.p), latlon(clip.q), &safety(), 0.35).expect("a route");
        assert_ne!(*c.points.last().unwrap(), clip.q);
        assert_clear_of(&c.points, &[clip.rock]);
        let params = SearchParams::fixed(0.0, 0.0);
        for w in c.points.windows(2) {
            assert!(clip.reference.segment_clear_by(w[0], w[1], &params));
        }
    }

    // ---- which way: the documented costs decide ----

    /// A channel `width` wide and `length` long between a mainland to the
    /// south and an island to the north, the island's far shore
    /// `island_top` north of the channel's centre line; the start 5 km west
    /// of the channel's mouth and the finish 5 km east of its end, on its
    /// centre line. The way round goes north of the island, far enough from
    /// the channel that the fine corridor round either way never holds the
    /// other. `bridges`: opening bridges (bascules, no stated clearance)
    /// across the channel, at these distances along it.
    struct Choice {
        chart: TestChart,
        start: LatLon,
        finish: LatLon,
        land: Vec<[f64; 4]>,
        half_width: f64,
        island_top: f64,
    }

    fn channel_or_round(width: f64, length: f64, island_top: f64, bridges: &[f64]) -> Choice {
        let h = width / 2.0;
        let (x0, x1) = (5_000.0, 5_000.0 + length);
        let cover = rect(at(-15_000.0, -15_000.0), at(x1 + 20_000.0, 15_000.0));
        let land = vec![
            rect(at(x0, -40_000.0), at(x1, -h)),
            rect(at(x0, h), at(x1, island_top)),
        ];
        let mut sea = Sea::new().covered(cover);
        for r in &land {
            sea = sea.land(*r);
        }
        for &d in bridges {
            sea = sea.area(
                "BRIDGE",
                &[("CATBRG", AttributeValue::Integer(5))],
                rect(at(x0 + d - 10.0, -h - 20.0), at(x0 + d + 10.0, h + 20.0)),
            );
        }
        let (start, _) = end(at(0.0, 0.0));
        let (finish, _) = end(at(x1 + 5_000.0, 0.0));
        Choice { chart: sea.chart(cover), start, finish, land, half_width: h, island_top }
    }

    #[test]
    fn a_wide_channel_with_full_sea_room_is_taken() {
        // 4 km wide: its centre line is 2 km of plane from both shores,
        // beyond even the soft 0.5 nm offing (1.6 km of plane at 55°N), so
        // it costs nothing over its length. Round the island (6 km) adds
        // about 2 × (hypot(5, 7.6) − 5) ≈ 8 km.
        let ch = channel_or_round(4_000.0, 30_000.0, 6_000.0, &[]);
        let sources = [ch.chart.source()];
        let c = corridor(&sources, ch.start, ch.finish, &safety(), MARGIN_FACTORS[0]).expect("a route");
        assert_clear_of(&c.points, &ch.land);
        assert!(c.points.iter().all(|p| p[1] - origin()[1] < ch.half_width), "{:?}", c.points);
    }

    #[test]
    fn a_channel_inside_the_soft_offing_is_left_for_a_shorter_detour_in_open_water() {
        // 2 km wide: its centre is 1 km of plane from each shore, inside the
        // soft offing, where each metre costs (1.61 − 1.0) / (1.61 − 0.65)
        // ≈ 0.64 extra: 30 km of it, 19 km extra. Round the island (8 km)
        // adds about 2 × (hypot(5, 9.6) − 5) ≈ 11.6 km, so the route goes
        // round — and keeps clear of the channel all the way.
        let ch = channel_or_round(2_000.0, 30_000.0, 8_000.0, &[]);
        let sources = [ch.chart.source()];
        let c = corridor(&sources, ch.start, ch.finish, &safety(), MARGIN_FACTORS[0]).expect("a route");
        assert_clear_of(&c.points, &ch.land);
        assert!(c.points.iter().any(|p| p[1] - origin()[1] > ch.island_top), "{:?}", c.points);
    }

    #[test]
    fn one_opening_bridge_does_not_make_a_narrow_channel_cheap() {
        // 600 m wide: the centre is 300 m of plane from the shores, inside
        // the 0.2 nm offing (645 m), where every metre costs 1 + 3 × 0.53 ≈
        // 2.6 extra — 15.7 km over 6 km of channel. The offing fades only
        // within 0.2 nm of the bridge, which buys back about 1.7 km. Round
        // the 8 km island adds about 11.6 km: the route goes round.
        let ch = channel_or_round(600.0, 6_000.0, 8_000.0, &[3_000.0]);
        let sources = [ch.chart.source()];
        let c = corridor(&sources, ch.start, ch.finish, &safety(), MARGIN_FACTORS[0]).expect("a route");
        assert_clear_of(&c.points, &ch.land);
        assert!(c.points.iter().any(|p| p[1] - origin()[1] > ch.island_top), "{:?}", c.points);
    }

    #[test]
    fn a_channel_lined_with_opening_bridges_is_taken() {
        // The same channel with five bridges 1.2 km apart: within 0.2 nm
        // (645 m of plane) of each the offing fades, and the fades overlap
        // over the whole channel — about half the 15.7 km extra goes. Round
        // the 9 km island adds about 2 × (hypot(5, 10.6) − 5) ≈ 13 km: the
        // route takes the channel, through every opening.
        let ch = channel_or_round(600.0, 6_000.0, 9_000.0, &[600.0, 1_800.0, 3_000.0, 4_200.0, 5_400.0]);
        let sources = [ch.chart.source()];
        let c = corridor(&sources, ch.start, ch.finish, &safety(), MARGIN_FACTORS[0]).expect("a route");
        assert_clear_of(&c.points, &ch.land);
        assert!(c.points.iter().all(|p| p[1] - origin()[1] < ch.half_width), "{:?}", c.points);
    }

    // ---- the fine corridor: a mile either side of the way ----

    /// A passage at exactly 45° across open water. Its coarse way is a
    /// staircase of diagonal steps, and with 48 m cells from a box corner
    /// 312.5 cells away it steps diagonally through the corners of the fine
    /// grid's tiles: the corridor a mile wide holds the tiles round each
    /// corner, so the route is still one straight leg.
    #[test]
    fn a_diagonal_passage_across_open_water_is_one_straight_leg() {
        let s = SafetyConfig { grid_res_m: 48.0, fine_res_m: 24.0, ..SafetyConfig::default() };
        let chart = open_sea();
        let sources = [chart.source()];
        let (start, a) = end(at(0.0, 0.0));
        let (finish, b) = end(at(10_020.0, 10_020.0));
        let lat = lattice(a, b, &s);
        let coarse = Grid::new(lat.min, lat.max, lat.cres);
        let (ca, cb) = (cell_of(&coarse, a), cell_of(&coarse, b));
        assert!(ca.0 == ca.1 && cb.0 == cb.1 && ca.0 % 32 != 0, "{ca:?} {cb:?}");
        let c = corridor(&sources, start, finish, &s, MARGIN_FACTORS[0]).expect("open water");
        assert_eq!(c.points, vec![a, b]);
    }

    /// A harbour wall across the passage with an entrance one coarse cell
    /// wide, closed by a pontoon (as in `boomed_entrance`), and a second
    /// gap 150 m wide in the wall a little over a mile to the east. The way
    /// runs straight down one coarse column whose centre lies a mile and
    /// 0.3 m short of a fine tile boundary — the fine cell is 1852/74.993 m
    /// of ground, so a mile is 74.993 fine cells, and column 314's centre
    /// (629 fine cells from the box edge, the box sized by its margin
    /// factor to put the start there) plus a mile is 703.993 cells, short
    /// of the boundary at 704 = 11 × 64 — and the gap lies 300 m past it.
    struct BeyondTheCorridor {
        chart: TestChart,
        start: LatLon,
        finish: LatLon,
        safety: SafetyConfig,
        margin_factor: f64,
        lat: Lattice,
        land: Vec<[f64; 4]>,
        gap: (f64, f64),
        /// The repair's square round the entrance, where it was open.
        repair: BTreeSet<(usize, usize)>,
    }

    fn a_gap_just_beyond_the_corridor() -> BeyondTheCorridor {
        let fine_res = METRES_PER_NM / 74.993;
        let s = SafetyConfig { grid_res_m: 2.0 * fine_res, fine_res_m: fine_res, ..SafetyConfig::default() };
        let (start, a) = end(at(0.0, 6_000.0));
        let (finish, b) = end(at(0.0, -6_000.0));
        let span = (a[0] - b[0]).hypot(a[1] - b[1]);
        let k = 1.0 / ((start.lat + finish.lat) / 2.0).to_radians().cos();
        let i = 314.0;
        let mf = (i + 0.5) * s.grid_res_m * k / span;
        let lat = lattice_in(a, b, &s, mf);
        let ax = Axes { turned: false };
        let (u0, v0) = ax.uv(lat.min);
        let (au, _) = ax.uv(a);
        assert!(((au - u0) / lat.cres - (i + 0.5)).abs() < 1e-6, "the start is on the column's centre line");
        let tile_edge = u0 + 704.0 * lat.fres;
        let mile = METRES_PER_NM * lat.k;
        assert!(au + mile < tile_edge && au + mile > tile_edge - 1.0, "{}", tile_edge - au - mile);
        let j = ((0.0 - v0) / lat.cres).floor();
        let (west, east) = (u0 + i * lat.cres, u0 + (i + 1.0) * lat.cres);
        let (wall_s, wall_n) = (v0 + (j - 1.0) * lat.cres + 2.0, v0 + (j + 2.0) * lat.cres - 2.0);
        let boom = v0 + j * lat.cres + lat.fres * 1.25;
        let gap = (tile_edge + 300.0, tile_edge + 450.0);
        let mut land = vec![
            ax.r(-40_000.0, wall_s, west - 2.0, wall_n),
            ax.r(east + 2.0, wall_s, gap.0, wall_n),
            ax.r(gap.1, wall_s, 40_000.0, wall_n),
        ];
        let pontoon = ax.r(west - 10.0, boom, east + 10.0, boom + 2.0);
        let cover = ax.r(-20_000.0, -15_000.0, 25_000.0, 15_000.0);
        let mut sea = Sea::new().covered(cover).area("PONTON", &[], pontoon);
        for r in &land {
            sea = sea.land(*r);
        }
        let chart = sea.chart(cover);
        let fresh = build_hazard_grid(&[chart.source()], lat.min, lat.max, &s, lat.k);
        let mut repair = BTreeSet::new();
        for dj in -REPAIR_RADIUS..=REPAIR_RADIUS {
            for di in -REPAIR_RADIUS..=REPAIR_RADIUS {
                let (x, y) = ((i as i64 + di) as usize, (j as i64 + dj) as usize);
                if !fresh.is_blocked_loose(x, y) {
                    repair.insert((x, y));
                }
            }
        }
        land.push(pontoon);
        BeyondTheCorridor { chart, start, finish, safety: s, margin_factor: mf, lat, land, gap, repair }
    }

    /// The fine corridor is a mile either side of the coarse way, rounded
    /// out to whole tiles — and no wider. The gap lies outside it, so when
    /// the pontoon closes the entrance the fine grid cannot see the gap:
    /// the coarse pass is sent round (the repair closes the entrance)
    /// before the route goes through the gap.
    #[test]
    fn the_fine_corridor_holds_a_mile_round_the_way_and_no_more() {
        let sc = a_gap_just_beyond_the_corridor();
        let sources = [sc.chart.source()];
        let c = corridor(&sources, sc.start, sc.finish, &sc.safety, sc.margin_factor).expect("through the gap");
        let ax = Axes { turned: false };
        assert!(c.points.iter().any(|&p| ax.uv(p).0 > sc.gap.0 - 1.0), "by the gap: {:?}", c.points);
        assert_eq!(closed_by_repairs(&c, &sources, &sc.safety, &sc.lat), sc.repair);
    }

    /// Every leg keeps off the charted land. Here the leg out of the gap
    /// cuts 1 m across the south-east corner of the wall west of it: the
    /// fine cell holding that corner is blocked, yet
    /// `Grid::segment_clear_by` passes the leg — its walk hops diagonally
    /// from the cell north-east of the corner cell to the one south-west of
    /// it and checks only the other elbow cell, so the "supercover" misses
    /// a cell the segment crosses. The belt-and-braces re-check in
    /// `fine_route` uses the same walk, so it cannot catch it.
    #[test]
    fn no_leg_cuts_the_corner_of_a_wall() {
        let sc = a_gap_just_beyond_the_corridor();
        let sources = [sc.chart.source()];
        let c = corridor(&sources, sc.start, sc.finish, &sc.safety, sc.margin_factor).expect("through the gap");
        assert_clear_of(&c.points, &sc.land);
    }

    // ---- a real harbour ----

    /// Segments and triangles: does the segment touch the triangle at all?
    fn touches_triangle(a: [f64; 2], b: [f64; 2], t: [[f64; 2]; 3]) -> bool {
        let orient = |p: [f64; 2], q: [f64; 2], r: [f64; 2]| {
            (q[0] - p[0]) * (r[1] - p[1]) - (q[1] - p[1]) * (r[0] - p[0])
        };
        let inside = |p: [f64; 2]| {
            let d = [orient(t[0], t[1], p), orient(t[1], t[2], p), orient(t[2], t[0], p)];
            !(d.iter().any(|&v| v < 0.0) && d.iter().any(|&v| v > 0.0))
        };
        let cross = |p: [f64; 2], q: [f64; 2]| {
            let (d1, d2) = (orient(a, b, p), orient(a, b, q));
            let (d3, d4) = (orient(p, q, a), orient(p, q, b));
            d1 * d2 <= 0.0 && d3 * d4 <= 0.0
        };
        inside(a) || inside(b) || (0..3).any(|i| cross(t[i], t[(i + 1) % 3]))
    }

    /// Boston harbour (NOAA US5BOSBE, public domain), from the channel at
    /// the cell's north edge to the channel leaving it to the west: the
    /// straight line between them crosses the chart's own land, so the
    /// route must bend through the channels — and no leg may touch the
    /// chart's land, or any area the chart makes a hard obstacle for a boat
    /// with a 2.5 m safety contour.
    #[test]
    fn a_route_across_boston_harbour_keeps_to_charted_safe_water() {
        let (data, info) = boston();
        let e = &info.extent_mercator;
        let (w, h) = (e.max_x - e.min_x, e.max_y - e.min_y);
        let (start, a) = end([e.min_x + 0.404 * w, e.max_y - 0.036 * h]);
        let (finish, b) = end([e.min_x + 0.048 * w, e.max_y - 0.263 * h]);
        let s = SafetyConfig { grid_res_m: 40.0, fine_res_m: 20.0, ..SafetyConfig::default() };
        let mut hard = Vec::new();
        let mut land = Vec::new();
        for f in &data.features {
            let Some(geom) = &f.area_geometry else { continue };
            if hazards::classify(f, &s) != Some(Severity::Hard) {
                continue;
            }
            let is_land = s57_code_to_acronym(f.type_code) == "LNDARE";
            geom.for_each_triangle_global(info.ref_lat, info.ref_lon, |t| {
                hard.push(t);
                if is_land {
                    land.push(t);
                }
            });
        }
        assert!(land.iter().any(|&t| touches_triangle(a, b, t)), "the straight line crosses land");
        for p in [a, b] {
            assert!(!hard.iter().any(|&t| touches_triangle(p, p, t)), "{p:?} is in safe water");
        }

        let sources = [ChartSource { data, info }];
        let c = corridor(&sources, start, finish, &s, MARGIN_FACTORS[0]).expect("through the channels");
        assert_eq!((c.points[0], *c.points.last().unwrap()), (a, b));
        for w in c.points.windows(2) {
            for &t in &hard {
                assert!(!touches_triangle(w[0], w[1], t), "{:?} → {:?} touches {t:?}", w[0], w[1]);
            }
        }
        let straight = (b[0] - a[0]).hypot(b[1] - a[1]);
        let len = length(&c.points);
        assert!(len > straight && len < 2.5 * straight, "{len} vs {straight}");
    }

    // ---- a suspected bug ----

    /// `holds_fine_water` is asked about the fine cells *inside* a coarse
    /// cell: its far corner is pulled in by 1e-6 so the next coarse cell's
    /// fine row is not counted. Its near corner is not pushed in, and on
    /// Mercator-sized coordinates `origin + y × res` rounds down for about
    /// half the rows — and for those the fine row *below* the coarse cell is
    /// counted as well. A coarse cell holding no fine water is then not
    /// closed when the water just south of it is open.
    #[test]
    fn a_coarse_cell_is_judged_by_its_own_fine_cells() {
        let k = 1.0 / LAT0.to_radians().cos();
        let (min, max) = (at(-26_000.0, -26_000.0), at(26_000.0, 26_000.0));
        let coarse = Grid::new(min, max, 50.0 * k);
        let mut fine = Grid::new(min, max, 25.0 * k);
        let x = 300;
        let y = (290..330)
            .find(|&y| {
                let lo = coarse.origin[1] + y as f64 * coarse.res;
                fine.cell_of([coarse.centre(x, y)[0], lo]).unwrap().1 < 2 * y
            })
            .expect("a row whose south edge rounds down");
        // Close the coarse cell's own four fine cells and nothing else.
        let lo = [coarse.origin[0] + x as f64 * coarse.res, coarse.origin[1] + y as f64 * coarse.res];
        let inner = rect([lo[0] + 1.0, lo[1] + 1.0], [lo[0] + coarse.res - 1.0, lo[1] + coarse.res - 1.0]);
        for t in triangles(inner) {
            fine.stamp_triangle(t, true);
        }
        fine.finalize();
        for (fx, fy) in [(2 * x, 2 * y), (2 * x + 1, 2 * y), (2 * x, 2 * y + 1), (2 * x + 1, 2 * y + 1)] {
            assert!(fine.is_blocked(fx, fy));
        }
        assert!(!fine.is_blocked(2 * x, 2 * y - 1), "the water south of the cell is open");
        assert!(!holds_fine_water(&coarse, &fine, x, y), "no fine cell inside coarse cell ({x}, {y}) is water");
    }

    // ---- properties ----

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Both ends lie inside the box, at least 15 km of ground (15 km × k
        /// of plane) and `margin_factor` of the span in from every side.
        #[test]
        fn the_search_box_holds_both_ends_with_room_on_every_side(
            ax in -1e6..1e6f64, ay in -1e6..1e6f64, bx in -1e6..1e6f64, by in -1e6..1e6f64,
            k in 1.0..3.0f64, mf in 0.0..3.0f64,
        ) {
            let (min, max) = passage_box([ax, ay], [bx, by], k, mf);
            let span = (bx - ax).hypot(by - ay);
            let room = (span * mf).max(15_000.0 * k) - 1e-6;
            for (x, y) in [(ax, ay), (bx, by)] {
                prop_assert!(x - min[0] >= room && y - min[1] >= room, "{min:?} {x} {y}");
                prop_assert!(max[0] - x >= room && max[1] - y >= room, "{max:?} {x} {y}");
            }
            // And no more than that: the box is the ends' own box, grown.
            let grown = (span.max(1_000.0) * mf).max(15_000.0 * k);
            prop_assert!((ax.min(bx) - min[0] - grown).abs() < 1e-6);
            prop_assert!((max[1] - ay.max(by) - grown).abs() < 1e-6);
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(8))]

        /// Anywhere in the island sea: a route, starting on the requested
        /// start (or the centre of its fine cell, when the line from the
        /// exact point is not clear), ending likewise, never crossing the
        /// island, and no shorter than the straight line it replaces.
        #[test]
        fn every_route_in_the_island_sea_keeps_off_the_island(
            sx in -5_000.0..15_000.0f64, sy in -8_000.0..8_000.0f64,
            fx in -5_000.0..15_000.0f64, fy in -8_000.0..8_000.0f64,
        ) {
            let near_island = |x: f64, y: f64| (3_700.0..=6_300.0).contains(&x) && (-1_800.0..=1_800.0).contains(&y);
            prop_assume!(!near_island(sx, sy) && !near_island(fx, fy));
            prop_assume!((fx - sx).hypot(fy - sy) > 500.0);
            let s = SafetyConfig { grid_res_m: 100.0, fine_res_m: 50.0, ..SafetyConfig::default() };
            let chart = island_sea();
            let sources = [chart.source()];
            let (start, a) = end(at(sx, sy));
            let (finish, b) = end(at(fx, fy));
            let c = corridor(&sources, start, finish, &s, MARGIN_FACTORS[0]);
            prop_assert!(c.is_ok(), "{:?}", c.err());
            let c = c.unwrap();
            let fres = 50.0 * c.k;
            let n = c.points.len();
            prop_assert!(n >= 2);
            let off = |p: [f64; 2], q: [f64; 2]| (p[0] - q[0]).hypot(p[1] - q[1]);
            prop_assert!(off(c.points[0], a) <= fres * 0.75, "{:?} vs {a:?}", c.points[0]);
            prop_assert!(off(c.points[n - 1], b) <= fres * 0.75, "{:?} vs {b:?}", c.points[n - 1]);
            for w in c.points.windows(2) {
                prop_assert!(!crosses(w[0], w[1], island()), "{:?} → {:?}", w[0], w[1]);
            }
            prop_assert!(length(&c.points) >= off(c.points[0], c.points[n - 1]) - 1e-6);
        }
    }
}
