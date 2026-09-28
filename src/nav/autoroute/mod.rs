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
    let grid = build_hazard_grid(sources, min, max, safety, k);
    // NAVCORE_ROUTE_DEBUG=<file.png>: the grid as the search sees it.
    let debug_png = std::env::var("NAVCORE_ROUTE_DEBUG").ok();
    if let Some(ref out) = debug_png {
        grid.debug_png(&[a, b], std::path::Path::new(out), 1600);
    }

    let nm = METRES_PER_NM * k;
    let reach = ENDPOINT_REACH_NM * nm;
    let mut warnings = Vec::new();

    // An end on the hard — a pontoon, a quay, water the chart gives less
    // depth than the boat needs — moves to the nearest water that is not.
    // And that water must connect to the other end: the nearest water to a
    // point on Refshaleøen is an inner basin behind fixed bridges, and a
    // start snapped into it can never leave. So one end is freed first, its
    // connected water found, and the other end freed into that.
    let loose = SearchParams::fixed(0.0, 0.0);
    let start_cell = grid.cell_of(a).ok_or(PlanError::OffGrid)?;
    let goal_cell = grid.cell_of(b).ok_or(PlanError::OffGrid)?;
    for (which, cell) in [("start", start_cell), ("finish", goal_cell)] {
        if !grid.is_covered(cell.0, cell.1)
            && grid::nudge(&grid, cell, &loose, reach)
                .is_none_or(|c| !grid.is_covered(c.0, c.1))
        {
            return Err(PlanError::Uncharted { which });
        }
    }
    let goal_plain =
        grid::nudge(&grid, goal_cell, &loose, reach).ok_or(PlanError::EndpointBuried { which: "finish" })?;
    let start_plain =
        grid::nudge(&grid, start_cell, &loose, reach).ok_or(PlanError::EndpointBuried { which: "start" })?;
    let (start_free, goal_free) = {
        let goal_water = grid.component(goal_plain, false);
        match grid::nudge_into(&grid, start_cell, &loose, reach, Some(&goal_water)) {
            Some(s) => (s, goal_plain),
            None => {
                let start_water = grid.component(start_plain, false);
                match grid::nudge_into(&grid, goal_cell, &loose, reach, Some(&start_water)) {
                    Some(g) => (start_plain, g),
                    // Neither end reaches the other's water within a mile:
                    // a wider box may join them round the outside.
                    None => return Err(PlanError::Disconnected),
                }
            }
        }
    };
    for (which, from, to) in [("start", start_cell, start_free), ("finish", goal_cell, goal_free)] {
        if from != to {
            let why = if grid.is_blocked(from.0, from.1) {
                "is on land or in water charted shallower than the boat needs"
            } else {
                "is in water that does not connect to the rest of the passage \
                 (a fixed bridge, a lock or a closed basin)"
            };
            warnings.push(format!(
                "the {which} {why}; the route {} the nearest water that does, {:.0} m away",
                if which == "start" { "leaves from" } else { "ends at" },
                cell_dist_nm(&grid, from, to) * METRES_PER_NM / k
            ));
        }
    }
    let a_free = if start_free == start_cell { a } else { grid.centre(start_free.0, start_free.1) };
    let b_free = if goal_free == goal_cell { b } else { grid.centre(goal_free.0, goal_free.1) };

    let params = SearchParams {
        // The least water between the hull and anything the chart calls
        // unsafe, anywhere away from the ends. Not rounded up to the cell
        // size: a sound two cells wide has only cells beside a hazard.
        offing_hard_m: safety.offing_hard_nm * nm,
        offing_min_m: safety.offing_min_nm * nm,
        offing_soft_m: safety.offing_soft_nm * nm,
        ends: vec![a_free, b_free],
        taper_m: reach,
        gates: grid.gates.clone(),
        gate_taper_m: GATE_TAPER_NM * nm,
        loose: false,
    };

    let path = match grid::find_path(&grid, start_free, goal_free, &params) {
        Ok(p) => p,
        Err(SearchError::Unreachable) | Err(SearchError::OffGrid) => {
            return Err(PlanError::Unreachable)
        }
        Err(SearchError::Cancelled) => return Err(PlanError::Cancelled),
    };
    let mut points = grid::string_pull(&grid, &path, &params);

    // The ends the user actually asked for, where they are water: the cell
    // centre is up to half a cell off, and a route should end on its mark.
    // Only where the straight line from the exact point is itself clear.
    if let (Some(first), Some(second)) = (points.first().copied(), points.get(1).copied()) {
        let _ = first;
        if start_free == start_cell && grid.segment_clear_by(a, second, &params) {
            points[0] = a;
        }
    }
    let n = points.len();
    if n >= 2 && goal_free == goal_cell && grid.segment_clear_by(points[n - 2], b, &params) {
        points[n - 1] = b;
    }

    // Belt and braces: every emitted leg re-checked against the raster. The
    // pull already guarantees this; the check is the standing guard against
    // a future refactor, cheap enough to keep on.
    for w in points.windows(2) {
        debug_assert!(grid.segment_clear_by(w[0], w[1], &params));
        if !grid.segment_clear_by(w[0], w[1], &params) {
            return Err(PlanError::Unreachable);
        }
    }

    if let Some(ref out) = debug_png {
        grid.debug_png(&points, std::path::Path::new(out), 1600);
    }
    if points.windows(2).any(|w| grid.segment_touches_soft(w[0], w[1])) {
        warnings.push(CAUTION_AREA_NOTE.into());
    }
    let to_latlon = |p: [f64; 2]| {
        let (lat, lon) = Projection::to_wgs84(p[0], p[1]);
        LatLon::new(lat, lon)
    };
    let (start_at, finish_at) = (to_latlon(points[0]), to_latlon(points[points.len() - 1]));
    Ok(Corridor {
        grid,
        k,
        params,
        points,
        start: start_at,
        finish: finish_at,
        warnings,
    })
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
    // Water no chart covers is not water anyone has vouched for. The grid
    // starts closed and each chart opens only its own coverage — so a gap
    // between surveys, or a chart that failed to load, is a wall, never a
    // shortcut across unsurveyed ground.
    grid.close_all();
    let mut ordered: Vec<&ChartSource> = sources.iter().collect();
    ordered.sort_by(|x, y| y.info.native_scale.cmp(&x.info.native_scale));
    let mut gates = Vec::new();
    for source in &ordered {
        stamp_chart(&mut grid, source, safety, &mut gates);
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
    grid
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
