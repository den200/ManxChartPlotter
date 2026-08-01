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
    /// The sea between the endpoints is closed at this draft and offing.
    Unreachable,
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::NoCharts => write!(f, "no charts cover the passage"),
            PlanError::OffGrid => write!(f, "an endpoint lies outside the charted area"),
            PlanError::EndpointBuried { which } => {
                write!(f, "the {which} point is too far inside a hazard to nudge free")
            }
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

/// Plan a safe route from `start` to `finish` across the given charts.
pub fn plan(
    sources: &[ChartSource<'_>],
    start: LatLon,
    finish: LatLon,
    safety: &SafetyConfig,
) -> Result<Planned, PlanError> {
    if sources.is_empty() {
        return Err(PlanError::NoCharts);
    }
    let t0 = std::time::Instant::now();
    let mut warnings = Vec::new();

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
    // Margin: room to route *around* things near the endpoints. A headland
    // dodge can easily need a third of the passage span sideways.
    let span = ((b[0] - a[0]).hypot(b[1] - a[1])).max(1_000.0);
    let margin = (span * 0.35).max(15_000.0 * k);
    let min = [a[0].min(b[0]) - margin, a[1].min(b[1]) - margin];
    let max = [a[0].max(b[0]) + margin, a[1].max(b[1]) + margin];

    let grid = build_hazard_grid(sources, min, max, safety, k);

    let offing_min_m = safety.offing_min_nm * METRES_PER_NM * k;
    let params = SearchParams {
        offing_min_m,
        offing_soft_m: safety.offing_soft_nm * METRES_PER_NM * k,
    };

    let start_cell = grid.cell_of(a).ok_or(PlanError::OffGrid)?;
    let goal_cell = grid.cell_of(b).ok_or(PlanError::OffGrid)?;

    // §7.7: an endpoint closer to the hard than the offing — a berth, by
    // definition — is nudged to the nearest safe water, with a warning,
    // never an infinite loop.
    let nudge_max = 2.0 * METRES_PER_NM * k;
    let start_free = grid::nudge(&grid, start_cell, offing_min_m, nudge_max)
        .ok_or(PlanError::EndpointBuried { which: "start" })?;
    let goal_free = grid::nudge(&grid, goal_cell, offing_min_m, nudge_max)
        .ok_or(PlanError::EndpointBuried { which: "finish" })?;
    if start_free != start_cell {
        warnings.push(format!(
            "start moved {:.2} nm offshore to clear hazards and offing",
            cell_dist_nm(&grid, start_cell, start_free) / k
        ));
    }
    if goal_free != goal_cell {
        warnings.push(format!(
            "finish moved {:.2} nm offshore to clear hazards and offing",
            cell_dist_nm(&grid, goal_cell, goal_free) / k
        ));
    }

    let path = match grid::find_path(&grid, start_free, goal_free, &params) {
        Ok(p) => p,
        Err(SearchError::Unreachable) | Err(SearchError::OffGrid) => {
            return Err(PlanError::Unreachable)
        }
    };
    let pulled = grid::string_pull(&grid, &path, offing_min_m);

    // Belt and braces: every emitted leg re-checked against the raster. The
    // pull already guarantees this; the assert is the M3 DoD standing guard
    // against a future refactor, cheap enough to keep on.
    for w in pulled.windows(2) {
        debug_assert!(grid.segment_clear(w[0], w[1], offing_min_m));
        if !grid.segment_clear(w[0], w[1], offing_min_m) {
            return Err(PlanError::Unreachable);
        }
    }

    // The endpoints the user actually asked for, restored where the nudge is
    // the arrival circle's business rather than the route's.
    let mut points: Vec<LatLon> = pulled
        .iter()
        .map(|p| {
            let (lat, lon) = Projection::to_wgs84(p[0], p[1]);
            LatLon::new(lat, lon)
        })
        .collect();
    if start_free == start_cell {
        if let Some(first) = points.first_mut() {
            *first = start;
        }
    }
    if goal_free == goal_cell {
        if let Some(last) = points.last_mut() {
            *last = finish;
        }
    }

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

    log::info!(
        "auto-route: {} waypoints over {:.1} nm in {} ms",
        points.len(),
        crate::geo::distance_m(start, finish) / METRES_PER_NM,
        t0.elapsed().as_millis()
    );
    Ok(Planned {
        route,
        waypoints,
        warnings,
    })
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
    let mut ordered: Vec<&ChartSource> = sources.iter().collect();
    ordered.sort_by(|x, y| y.info.native_scale.cmp(&x.info.native_scale));
    for source in &ordered {
        stamp_chart(&mut grid, source, safety);
    }
    grid.finalize();
    grid
}

/// Rasterize one chart: erase its own coverage, then stamp what it forbids.
fn stamp_chart(grid: &mut Grid, source: &ChartSource<'_>, safety: &SafetyConfig) {
    let info = source.info;
    let data = source.data;
    let (ref_mx, ref_my) = crate::tiles::latlon_to_mercator(info.ref_lat, info.ref_lon);

    // The eraser pass: within this chart's stated coverage, this chart is
    // the truth, so whatever a coarser chart said there is wiped.
    for feature in &data.features {
        if !feature.is_coverage() || feature.attribute_int("CATCOV") != Some(1) {
            continue;
        }
        if let Some(geom) = &feature.area_geometry {
            geom.for_each_triangle_global(info.ref_lat, info.ref_lon, |tri| {
                grid.clear_triangle(tri);
            });
        }
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
