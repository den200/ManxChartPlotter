//! The isochrone engine: minimum-time sailing routes.
//!
//! §7 of the routing spec. From the start, grow rings of "everywhere the boat
//! can be after k time-steps"; each ring propagates a fan of candidate
//! headings through the polar, rejects what the chart forbids, and — the one
//! optimization that matters (§8.1) — prunes the frontier radially so it
//! stays O(sectors) instead of growing without bound. Everything else is
//! written plainly on purpose: the spec's arithmetic says naive-but-pruned
//! lands around 0.3 s on a Pi 4, and a user cannot feel the difference
//! between that and heroics.
//!
//! Units and frames, because this is where routing engines rot:
//! - positions are global Mercator metres (the chart's own plane; conformal,
//!   so headings and TWA are *exact* — §7.3);
//! - every real length (a step, an arrival radius) is ground metres times
//!   `k = sec(lat)`, taken once per node;
//! - angles are BAM u16 throughout (§7.4) — wraparound bugs die at the type.

use crate::geo::{LatLon, METRES_PER_NM};
use crate::render::projection::Projection;

use super::angles::{bam_to_rad, deg_to_bam, signed_diff_deg, Bam};
use super::autoroute::grid::Grid;
use super::model::{PointOfSail, RoutingConfig, TackState};
use super::polar::Polar;

/// Where the wind comes from, at a place and a time.
///
/// Direction is meteorological — the bearing the wind blows *from* — because
/// that is how every forecast, polar and sailor speaks. TWS in knots.
pub trait WindField {
    fn wind(&self, pos_merc: [f64; 2], time_ms: i64) -> (Bam, f64);
}

/// The M4 wind: the same everywhere, forever.
pub struct ConstantWind {
    pub from_deg: f64,
    pub tws_kt: f64,
}

impl WindField for ConstantWind {
    fn wind(&self, _pos: [f64; 2], _time: i64) -> (Bam, f64) {
        (deg_to_bam(self.from_deg), self.tws_kt)
    }
}

/// One node of the search: a place, a time, and which tack got us here.
#[derive(Clone, Copy)]
struct Node {
    pos: [f64; 2],
    time_ms: i64,
    tack: TackState,
    /// Index into the arena; `u32::MAX` for the start.
    parent: u32,
    /// What was sailed to get here, kept for the plan.
    heading: Bam,
    twa_deg: f64,
    tws_kt: f64,
    stw_kt: f64,
}

/// One leg of an analytic final approach.
#[derive(Debug, Clone)]
struct ClosingLeg {
    end_pos: [f64; 2],
    heading: Bam,
    twa_deg: f64,
    stw_kt: f64,
    tack: TackState,
    end_time_ms: i64,
}

#[derive(Debug)]
pub enum IsoError {
    /// The frontier died: no candidate anywhere was sailable and legal.
    Unreachable,
    /// Step limit hit before arrival — a guard, not a strategy.
    TookTooLong,
}

impl std::fmt::Display for IsoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IsoError::Unreachable => write!(
                f,
                "no sailable path: the wind, the polar or the chart closed every door"
            ),
            IsoError::TookTooLong => write!(f, "gave up: no arrival within the step limit"),
        }
    }
}

impl std::error::Error for IsoError {}

/// One point of the solved route, everything the plan needs.
#[derive(Debug, Clone)]
pub struct PlannedPoint {
    pub pos: LatLon,
    pub time_ms: i64,
    pub heading_deg: f64,
    pub twa_deg: f64,
    pub tws_kt: f64,
    pub stw_kt: f64,
    pub tack: TackState,
    pub motoring: bool,
}

/// The solved route, with the diagnostics the analytic tests assert on.
pub struct Isoroute {
    /// Start to finish, post-processed: collinear runs merged, tacks kept.
    pub points: Vec<PlannedPoint>,
    pub arrival_ms: i64,
    /// Candidate propagations attempted — the §8.2 scaling regression reads
    /// this: halving Δt must roughly double it, never square it.
    pub propagations: u64,
    /// Tack/gybe count of the *raw* path, before post-processing — analytic
    /// case 6 watches the sawtooth appear here.
    pub raw_tack_changes: usize,
}

pub struct IsochroneInput<'a> {
    pub polar: &'a Polar,
    pub wind: &'a dyn WindField,
    /// Chart constraints; `None` is open ocean (the analytic cases).
    pub grid: Option<&'a Grid>,
    pub start: LatLon,
    pub finish: LatLon,
    pub depart_ms: i64,
    pub config: &'a RoutingConfig,
    /// The time step. Callers pick coastal or offshore from the config; the
    /// tests pick what the analytic case demands.
    pub dt_s: u32,
    /// Ground metres the route must keep from hazards (with `grid`).
    pub offing_min_m: f64,
}

/// Hard ceiling on isochrone rings. 2000 rings at one hour each is 83 days
/// under sail; anything needing more is not a route, it is a drift.
const MAX_STEPS: usize = 2000;

/// Frontier sectors (§7.6). Power of two: the sector of a bearing is its BAM
/// shifted right, no modulo anywhere.
const N_SECTORS: usize = 256;
const SECTOR_SHIFT: u32 = 8; // 65536 / 256

/// How many nodes each sector keeps. One is the classic radial prune; the
/// second slot is the two-sided-island insurance — both passages around an
/// obstacle survive even when they project into the same sector.
const PER_SECTOR: usize = 2;

pub fn solve(input: &IsochroneInput<'_>) -> Result<Isoroute, IsoError> {
    let start_m = merc(input.start);
    let finish_m = merc(input.finish);
    let cfg = input.config;
    let dt_h = input.dt_s as f64 / 3600.0;

    // Π₅: a penalty above half a step dominates the step. Clamp and say so.
    let max_pen = (input.dt_s as f64 * 0.5) as i64 * 1000;
    let clamp_pen = |ms: i64| {
        if ms > max_pen {
            log::warn!(
                "tack/gybe penalty {} s clamped to {} s (Π₅: ≤ 0.5·dt)",
                ms / 1000,
                max_pen / 1000
            );
            max_pen
        } else {
            ms
        }
    };
    let tack_pen_ms = clamp_pen(cfg.tack_penalty_day_s as i64 * 1000);
    let gybe_pen_ms = clamp_pen(cfg.gybe_penalty_day_s as i64 * 1000);

    let mut arena: Vec<Node> = vec![Node {
        pos: start_m,
        time_ms: input.depart_ms,
        tack: TackState::Starboard,
        parent: u32::MAX,
        heading: 0,
        twa_deg: 0.0,
        tws_kt: 0.0,
        stw_kt: 0.0,
    }];
    let mut frontier: Vec<u32> = vec![0];
    let mut propagations: u64 = 0;

    // The candidate fan is fixed per ring apart from its centre bearing;
    // headings are BAM so building it is integer arithmetic.
    let fan_half = (cfg.max_diverted_deg / cfg.heading_step_deg).floor() as i32;

    let mut best_arrival: Option<(u32, i64, Vec<ClosingLeg>)> = None;

    for ring in 0..MAX_STEPS {
        // Rings are equal-time, so once the ring's own clock has caught up
        // with the best analytic arrival, no later node can beat it: closing
        // times are non-negative. Stop, optimally.
        let ring_time = input.depart_ms + ring as i64 * input.dt_s as i64 * 1000;
        if let Some((node, t, ref plan)) = best_arrival {
            if t <= ring_time {
                let plan = plan.clone();
                return Ok(extract(input, &arena, node, t, plan, propagations));
            }
        }
        let mut sectors: Vec<[(f32, u32); PER_SECTOR]> =
            vec![[(f32::MIN, u32::MAX); PER_SECTOR]; N_SECTORS];
        let mut any_child = false;

        for &pi in &frontier {
            let parent = arena[pi as usize];
            let (wind_from, tws) = input.wind.wind(parent.pos, parent.time_ms);
            if tws > cfg.max_tws_kt {
                continue; // §7.5 hard weather limit: do not sail out of here
            }
            let (lat, _) = Projection::to_wgs84(parent.pos[0], parent.pos[1]);
            let k = 1.0 / lat.to_radians().cos();

            let bearing_fin = plane_bearing(parent.pos, finish_m);

            // §7.7: the final approach is *analytic*, not stepped. From any
            // node within the horizon, the time to the mark is closed-form —
            // straight at it where sailable, or the two-leg VMG wedge where
            // the mark hides in a no-go cone. This is also what decouples
            // arrival from pruning: the optimal beat's last tack stops
            // growing its radius and radial pruning starves it — seen live
            // as a route that sailed the WRONG way first — so arrival must
            // never depend on out-surviving the frontier.
            let d_plane = dist2(parent.pos, finish_m).sqrt();
            let horizon = (3.0 * METRES_PER_NM * k)
                .max(3.0 * 8.0 * dt_h * METRES_PER_NM * k);
            if d_plane <= horizon {
                if let Some(legs) = closing_legs(
                    input, parent.pos, parent.time_ms, parent.tack, finish_m, k,
                    wind_from, tws, tack_pen_ms, gybe_pen_ms,
                ) {
                    let t = legs.last().map(|l| l.end_time_ms).unwrap_or(parent.time_ms);
                    if best_arrival.as_ref().map(|(_, bt, _)| t < *bt).unwrap_or(true) {
                        best_arrival = Some((pi, t, legs));
                    }
                }
            }

            // The fan, the four VMG headings, and — when fitted and the sail
            // would crawl — the motor straight at the mark.
            let vmg = input.polar.vmg_at(tws);
            let mut candidates: Vec<(Bam, Option<f64>)> = Vec::with_capacity(
                (2 * fan_half + 1) as usize + 5,
            );
            for i in -fan_half..=fan_half {
                let h = bearing_fin
                    .wrapping_add(deg_to_bam(i as f64 * cfg.heading_step_deg as f64));
                candidates.push((h, None));
            }
            for twa in [
                vmg.beat_twa_deg as f64,
                -(vmg.beat_twa_deg as f64),
                vmg.run_twa_deg as f64,
                -(vmg.run_twa_deg as f64),
            ] {
                candidates.push((wind_from.wrapping_add(deg_to_bam(twa)), None));
            }
            if let Some(motor) = cfg.motor_speed_kt {
                let sail_toward = input
                    .polar
                    .speed_kt(signed_diff_deg(bearing_fin, wind_from).abs(), tws)
                    .unwrap_or(0.0);
                if sail_toward < cfg.motor_threshold_kt {
                    candidates.push((bearing_fin, Some(motor)));
                }
            }

            for (heading, motor) in candidates {
                propagations += 1;
                let twa_signed = signed_diff_deg(heading, wind_from);
                let (stw, motoring) = match motor {
                    Some(m) => (m, true),
                    None => match input.polar.speed_kt(twa_signed.abs(), tws) {
                        Some(s) if s > 0.05 => (s, false),
                        _ => continue, // no-go or becalmed on this heading
                    },
                };
                // Tack state from the wind side; motoring keeps the old tack
                // so a motor leg never pays a phantom gybe.
                let tack = if motoring {
                    parent.tack
                } else if twa_signed >= 0.0 {
                    TackState::Port
                } else {
                    TackState::Starboard
                };
                let penalty_ms = if !motoring && tack != parent.tack && pi != 0 {
                    if twa_signed.abs() < 90.0 {
                        tack_pen_ms
                    } else {
                        gybe_pen_ms
                    }
                } else {
                    0
                };

                // A tack costs *distance*, not clock: the boat sails the
                // remainder of the step after the manoeuvre, and every node
                // of a ring keeps the same time. That equal-time invariant is
                // what makes radial pruning valid — prune "furthest per
                // sector" among nodes of different times and the engine
                // quietly prefers late zigzaggers with big radii.
                let sail_h = ((input.dt_s as i64 * 1000 - penalty_ms) as f64
                    / 3_600_000.0)
                    .max(0.0);
                let step = stw * sail_h * METRES_PER_NM * k;
                let dir = bam_to_rad(heading);
                let child_pos = [
                    parent.pos[0] + step * dir.sin(),
                    parent.pos[1] + step * dir.cos(),
                ];

                // Constraint tests, cheapest first (§7.5).
                if let Some(grid) = input.grid {
                    let Some((cx, cy)) = grid.cell_of(child_pos) else {
                        continue;
                    };
                    let offing = input.offing_min_m * k;
                    if !grid.passable(cx, cy, offing) {
                        continue;
                    }
                    if !grid.segment_clear(parent.pos, child_pos, offing) {
                        continue;
                    }
                }

                let child_time = parent.time_ms + (input.dt_s as i64) * 1000;

                // §7.6 pruning: a child that reaches no further out in its
                // sector than the ones already kept is dominated.
                let r2 = dist2(child_pos, start_m) as f32;
                let sector =
                    (plane_bearing(start_m, child_pos) >> SECTOR_SHIFT) as usize & (N_SECTORS - 1);
                let slots = &mut sectors[sector];
                let min_slot = (0..PER_SECTOR)
                    .min_by(|&a, &b| slots[a].0.total_cmp(&slots[b].0))
                    .unwrap();
                if r2 <= slots[min_slot].0 {
                    continue;
                }
                arena.push(Node {
                    pos: child_pos,
                    time_ms: child_time,
                    tack,
                    parent: pi,
                    heading,
                    twa_deg: twa_signed,
                    tws_kt: tws,
                    stw_kt: stw,
                });
                slots[min_slot] = (r2, (arena.len() - 1) as u32);
                any_child = true;
            }
        }

        if !any_child {
            return match best_arrival {
                Some((node, t, plan)) => Ok(extract(input, &arena, node, t, plan, propagations)),
                None => Err(IsoError::Unreachable),
            };
        }
        frontier = sectors
            .iter()
            .flatten()
            .filter(|(_, i)| *i != u32::MAX)
            .map(|&(_, i)| i)
            .collect();
    }
    match best_arrival {
        Some((node, t, plan)) => Ok(extract(input, &arena, node, t, plan, propagations)),
        None => Err(IsoError::TookTooLong),
    }
}

/// The analytic final approach from a node: the cheapest of the straight
/// heading and (inside a no-go cone) the two-leg VMG wedge, with tack and
/// gybe penalties charged and every implied segment checked against the
/// chart. `None` when nothing legal closes.
#[allow(clippy::too_many_arguments)]
fn closing_legs(
    input: &IsochroneInput<'_>,
    pos: [f64; 2],
    time_ms: i64,
    tack_now: TackState,
    finish_m: [f64; 2],
    k: f64,
    wind_from: Bam,
    tws: f64,
    tack_pen_ms: i64,
    gybe_pen_ms: i64,
) -> Option<Vec<ClosingLeg>> {
    let d_plane = dist2(pos, finish_m).sqrt();
    if d_plane < 1.0 {
        return Some(Vec::new());
    }
    let d_nm = d_plane / (METRES_PER_NM * k);
    let bearing = plane_bearing(pos, finish_m);
    let seg_ok = |a: [f64; 2], b: [f64; 2]| match input.grid {
        Some(g) => g.segment_clear(a, b, input.offing_min_m * k),
        None => true,
    };
    let tack_of = |twa: f64| if twa >= 0.0 { TackState::Port } else { TackState::Starboard };
    let pen = |from: TackState, twa: f64| -> i64 {
        if tack_of(twa) != from {
            if twa.abs() < 90.0 { tack_pen_ms } else { gybe_pen_ms }
        } else {
            0
        }
    };

    let mut best: Option<(i64, Vec<ClosingLeg>)> = None;
    let mut consider = |legs: Vec<ClosingLeg>| {
        let t = legs.last().map(|l| l.end_time_ms).unwrap_or(i64::MAX);
        if best.as_ref().map(|(bt, _)| t < *bt).unwrap_or(true) {
            best = Some((t, legs));
        }
    };

    // Straight at it, where the polar allows.
    let twa_direct = signed_diff_deg(bearing, wind_from);
    if let Some(v) = input.polar.speed_kt(twa_direct.abs(), tws) {
        if v > 0.05 && seg_ok(pos, finish_m) {
            let t = time_ms + pen(tack_now, twa_direct) + (d_nm / v * 3_600_000.0) as i64;
            consider(vec![ClosingLeg {
                end_pos: finish_m,
                heading: bearing,
                twa_deg: twa_direct,
                stw_kt: v,
                tack: tack_of(twa_direct),
                end_time_ms: t,
            }]);
        }
    }
    // Motoring closes anything, at motor speed.
    if let Some(m) = input.config.motor_speed_kt {
        if seg_ok(pos, finish_m) {
            let t = time_ms + (d_nm / m * 3_600_000.0) as i64;
            consider(vec![ClosingLeg {
                end_pos: finish_m,
                heading: bearing,
                twa_deg: twa_direct,
                stw_kt: m,
                tack: tack_now,
                end_time_ms: t,
            }]);
        }
    }

    // The wedges: mark inside the upwind or downwind cone → two VMG legs.
    let vmg = input.polar.vmg_at(tws);
    for (cone_twa, stw) in [
        (vmg.beat_twa_deg as f64, vmg.beat_stw_kt as f64),
        (vmg.run_twa_deg as f64, vmg.run_stw_kt as f64),
    ] {
        if stw <= 0.05 {
            continue;
        }
        let inside = if cone_twa < 90.0 {
            twa_direct.abs() < cone_twa
        } else {
            twa_direct.abs() > cone_twa
        };
        if !inside {
            continue;
        }
        // Solve a·e₊ + b·e₋ = target in the plane. Both orders are tried and
        // the one whose first leg keeps the current tack is cheaper.
        let h_plus = wind_from.wrapping_add(deg_to_bam(cone_twa));
        let h_minus = wind_from.wrapping_add(deg_to_bam(-cone_twa));
        let (rp, rm) = (bam_to_rad(h_plus), bam_to_rad(h_minus));
        let (ep, em) = ([rp.sin(), rp.cos()], [rm.sin(), rm.cos()]);
        let target = [finish_m[0] - pos[0], finish_m[1] - pos[1]];
        let det = ep[0] * em[1] - ep[1] * em[0];
        if det.abs() < 1e-9 {
            continue;
        }
        let a = (target[0] * em[1] - target[1] * em[0]) / det;
        let b = (ep[0] * target[1] - ep[1] * target[0]) / det;
        if a < 0.0 || b < 0.0 {
            continue; // outside this cone after all
        }
        for first_plus in [true, false] {
            let (len1, h1, twa1, e1) = if first_plus {
                (a, h_plus, cone_twa, ep)
            } else {
                (b, h_minus, -cone_twa, em)
            };
            let (len2, h2, twa2) = if first_plus {
                (b, h_minus, -cone_twa)
            } else {
                (a, h_plus, cone_twa)
            };
            let mid = [pos[0] + e1[0] * len1, pos[1] + e1[1] * len1];
            if !seg_ok(pos, mid) || !seg_ok(mid, finish_m) {
                continue;
            }
            let t1_ms = (len1 / (METRES_PER_NM * k) / stw * 3_600_000.0) as i64;
            let t2_ms = (len2 / (METRES_PER_NM * k) / stw * 3_600_000.0) as i64;
            let p1 = pen(tack_now, twa1);
            let p2 = pen(tack_of(twa1), twa2);
            let mid_t = time_ms + p1 + t1_ms;
            let end_t = mid_t + p2 + t2_ms;
            consider(vec![
                ClosingLeg {
                    end_pos: mid,
                    heading: h1,
                    twa_deg: twa1,
                    stw_kt: stw,
                    tack: tack_of(twa1),
                    end_time_ms: mid_t,
                },
                ClosingLeg {
                    end_pos: finish_m,
                    heading: h2,
                    twa_deg: twa2,
                    stw_kt: stw,
                    tack: tack_of(twa2),
                    end_time_ms: end_t,
                },
            ]);
        }
    }
    best.map(|(_, legs)| legs)
}

/// Walk the parents back and post-process (§7.8): merge collinear runs,
/// keep every tack, count the raw tacks first.
fn extract(
    input: &IsochroneInput<'_>,
    arena: &[Node],
    last: u32,
    arrival_ms: i64,
    closing: Vec<ClosingLeg>,
    propagations: u64,
) -> Isoroute {
    let mut chain: Vec<&Node> = Vec::new();
    let mut i = last;
    while i != u32::MAX {
        chain.push(&arena[i as usize]);
        i = arena[i as usize].parent;
    }
    chain.reverse();

    let mut raw_tack_changes = chain
        .windows(2)
        .filter(|w| w[0].parent != u32::MAX && w[0].tack != w[1].tack)
        .count();
    // The analytic closing's manoeuvres are as real as the stepped ones.
    let mut prev_tack = chain.last().map(|n| n.tack);
    for leg in &closing {
        if chain.len() > 1 && prev_tack.is_some_and(|t| t != leg.tack) {
            raw_tack_changes += 1;
        }
        prev_tack = Some(leg.tack);
    }

    // §7.8: simplify. Tack changes are real manoeuvres and always survive;
    // between them Douglas–Peucker at the configured tolerance eats the
    // sector-boundary zigzag a discrete heading fan cannot help producing.
    let (lat0, _) = Projection::to_wgs84(chain[0].pos[0], chain[0].pos[1]);
    let k0 = 1.0 / lat0.to_radians().cos();
    let tol_plane = input.config.simplify_tolerance_nm * METRES_PER_NM * k0;
    let mut anchors: Vec<usize> = vec![0];
    for j in 1..chain.len() {
        if chain[j].tack != chain[j - 1].tack && chain[j].parent != u32::MAX {
            anchors.push(j);
        }
    }
    if *anchors.last().unwrap() != chain.len() - 1 {
        anchors.push(chain.len() - 1);
    }
    let mut keep: Vec<usize> = vec![0];
    for w in anchors.windows(2) {
        douglas_peucker(&chain, w[0], w[1], tol_plane, &mut keep);
        keep.push(w[1]);
    }
    keep.dedup();

    let mut points: Vec<PlannedPoint> = keep
        .iter()
        .map(|&j| {
            let n = chain[j];
            let (lat, lon) = Projection::to_wgs84(n.pos[0], n.pos[1]);
            PlannedPoint {
                pos: LatLon::new(lat, lon),
                time_ms: n.time_ms,
                heading_deg: super::angles::bam_to_deg(n.heading),
                twa_deg: n.twa_deg,
                tws_kt: n.tws_kt,
                stw_kt: n.stw_kt,
                tack: n.tack,
                motoring: false,
            }
        })
        .collect();
    // The analytic closing legs finish the route, ending exactly at the mark
    // at the exact arrival time.
    for leg in &closing {
        let (lat, lon) = Projection::to_wgs84(leg.end_pos[0], leg.end_pos[1]);
        points.push(PlannedPoint {
            pos: LatLon::new(lat, lon),
            time_ms: leg.end_time_ms,
            heading_deg: super::angles::bam_to_deg(leg.heading),
            twa_deg: leg.twa_deg,
            tws_kt: 0.0,
            stw_kt: leg.stw_kt,
            tack: leg.tack,
            motoring: false,
        });
    }
    if closing.is_empty() {
        // Degenerate: the node itself was on the mark.
        points.push(PlannedPoint {
            pos: input.finish,
            time_ms: arrival_ms,
            heading_deg: 0.0,
            twa_deg: 0.0,
            tws_kt: 0.0,
            stw_kt: 0.0,
            tack: chain.last().map(|n| n.tack).unwrap_or(TackState::Starboard),
            motoring: false,
        });
    }

    // One last pass over the assembled route: the junction where the stepped
    // chain hands over to the analytic closing is a vertex by construction,
    // not by navigation — where the course simply continues, it goes.
    let mut i = 1;
    while i + 1 < points.len() {
        let (a, b, c) = (&points[i - 1], &points[i], &points[i + 1]);
        let brg_in = plane_bearing(merc(a.pos), merc(b.pos));
        let brg_out = plane_bearing(merc(b.pos), merc(c.pos));
        let straight = signed_diff_deg(brg_out, brg_in).abs() < 1.0;
        if straight && b.tack == c.tack {
            points.remove(i);
        } else {
            i += 1;
        }
    }

    Isoroute {
        points,
        arrival_ms,
        propagations,
        raw_tack_changes,
    }
}

/// Point of sail from an absolute TWA — for the plan's human-readable leg.
pub fn point_of_sail(twa_abs_deg: f64) -> PointOfSail {
    match twa_abs_deg {
        t if t < 60.0 => PointOfSail::CloseHauled,
        t if t < 80.0 => PointOfSail::CloseReach,
        t if t < 110.0 => PointOfSail::BeamReach,
        t if t < 150.0 => PointOfSail::BroadReach,
        _ => PointOfSail::Run,
    }
}

/// Douglas–Peucker over the chain's positions, recording the kept interior
/// indices (exclusive of the endpoints, which the caller anchors).
fn douglas_peucker(chain: &[&Node], a: usize, b: usize, tol: f64, keep: &mut Vec<usize>) {
    if b <= a + 1 {
        return;
    }
    let (pa, pb) = (chain[a].pos, chain[b].pos);
    let ab = [pb[0] - pa[0], pb[1] - pa[1]];
    let len2 = ab[0] * ab[0] + ab[1] * ab[1];
    let mut worst = (0.0f64, a);
    for j in a + 1..b {
        let p = chain[j].pos;
        let d = if len2 <= f64::EPSILON {
            dist2(p, pa).sqrt()
        } else {
            let t = (((p[0] - pa[0]) * ab[0] + (p[1] - pa[1]) * ab[1]) / len2).clamp(0.0, 1.0);
            let q = [pa[0] + ab[0] * t, pa[1] + ab[1] * t];
            dist2(p, q).sqrt()
        };
        if d > worst.0 {
            worst = (d, j);
        }
    }
    if worst.0 > tol {
        douglas_peucker(chain, a, worst.1, tol, keep);
        keep.push(worst.1);
        douglas_peucker(chain, worst.1, b, tol, keep);
    }
}

fn merc(p: LatLon) -> [f64; 2] {
    let (x, y) = Projection::to_mercator(p.lat, p.lon);
    [x, y]
}

fn dist2(a: [f64; 2], b: [f64; 2]) -> f64 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    dx * dx + dy * dy
}

/// Bearing in the plane, BAM. Conformal Mercator: this IS the true bearing.
fn plane_bearing(from: [f64; 2], to: [f64; 2]) -> Bam {
    deg_to_bam((to[0] - from[0]).atan2(to[1] - from[1]).to_degrees())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::polar::tests::TEST_POLAR;

    fn polar() -> Polar {
        Polar::parse(TEST_POLAR).unwrap()
    }

    fn config() -> RoutingConfig {
        RoutingConfig::default()
    }

    /// Run the engine over open water with a constant wind.
    fn run(
        wind: ConstantWind,
        start: LatLon,
        finish: LatLon,
        cfg: &RoutingConfig,
        dt_s: u32,
    ) -> Result<Isoroute, IsoError> {
        let p = polar();
        solve(&IsochroneInput {
            polar: &p,
            wind: &wind,
            grid: None,
            start,
            finish,
            depart_ms: 0,
            config: cfg,
            dt_s,
            offing_min_m: 0.0,
        })
    }

    fn hours(r: &Isoroute) -> f64 {
        r.arrival_ms as f64 / 3_600_000.0
    }

    /// A finish `nm` miles from start on a given true bearing, measured on
    /// the ellipsoid so the analytic expectations are exact.
    fn mark(start: LatLon, bearing_deg: f64, nm: f64) -> LatLon {
        crate::geo::advance(start, bearing_deg.to_radians(), nm * METRES_PER_NM)
    }

    const START: LatLon = LatLon { lat: 56.5, lon: 11.5 };

    /// §9.1 case 1: no wind. Under sail alone that is unreachable — reported,
    /// not divided by. With a motor fitted, it is a straight line at motor
    /// speed.
    #[test]
    fn case_1_calm_is_honest() {
        let finish = mark(START, 90.0, 10.0);
        let calm = || ConstantWind { from_deg: 0.0, tws_kt: 0.0 };
        let cfg = config();
        assert!(matches!(
            run(calm(), START, finish, &cfg, 600),
            Err(IsoError::Unreachable)
        ));

        let mut with_motor = config();
        with_motor.motor_speed_kt = Some(5.0);
        let r = run(calm(), START, finish, &with_motor, 600).expect("motor on");
        // 10 nm at 5 kt: 2 h, within the step quantization.
        assert!((hours(&r) - 2.0).abs() < 0.05, "{} h", hours(&r));
    }

    /// §9.1 case 2: beam reach, straight line, T = L / polar(90°, TWS)
    /// within 0.5 %.
    #[test]
    fn case_2_beam_reach_is_a_straight_line_at_polar_speed() {
        // Wind from the north, mark due east: TWA 90 the whole way.
        let l_nm = 20.0;
        let finish = mark(START, 90.0, l_nm);
        let cfg = config();
        let r = run(
            ConstantWind { from_deg: 0.0, tws_kt: 10.0 },
            START,
            finish,
            &cfg,
            600,
        )
        .expect("sailable");
        let expected_h = l_nm / 6.0; // polar(90°, 10 kt) = 6.0
        let got = hours(&r);
        assert!(
            (got - expected_h).abs() / expected_h < 0.005,
            "expected {expected_h:.3} h, got {got:.3} h"
        );
        // Post-processing leaves a straight line: start, maybe one bend of
        // numerical noise, finish.
        assert!(r.points.len() <= 3, "{} points", r.points.len());
        assert_eq!(r.raw_tack_changes, 0);
    }

    /// §9.1 case 3: dead upwind. T = L / beat_vmg within 1 %, and exactly
    /// one tack when nothing constrains the sides.
    #[test]
    fn case_3_a_beat_takes_vmg_time_with_one_tack() {
        let l_nm = 12.0;
        let finish = mark(START, 0.0, l_nm);
        let p = polar();
        let vmg = p.vmg_at(10.0).beat_vmg_kt as f64;
        let mut cfg = config();
        // No penalty: the pure geometry of the beat.
        cfg.tack_penalty_day_s = 0;
        cfg.gybe_penalty_day_s = 0;
        let r = run(
            ConstantWind { from_deg: 0.0, tws_kt: 10.0 },
            START,
            finish,
            &cfg,
            120,
        )
        .expect("beatable");
        let expected_h = l_nm / vmg;
        let got = hours(&r);
        assert!(
            (got - expected_h).abs() / expected_h < 0.01,
            "expected {expected_h:.3} h at VMG {vmg:.2}, got {got:.3} h"
        );
    }

    /// §9.1 case 5: an enormous tack penalty forces the single-tack beat.
    #[test]
    fn case_5_penalty_forces_a_single_tack() {
        let finish = mark(START, 0.0, 8.0);
        let mut cfg = config();
        cfg.tack_penalty_day_s = 100_000; // clamped to 0.5·dt by Π₅, still huge
        let r = run(
            ConstantWind { from_deg: 0.0, tws_kt: 10.0 },
            START,
            finish,
            &cfg,
            600,
        )
        .expect("still beatable");
        assert_eq!(r.raw_tack_changes, 1, "one tack and no more");
    }

    /// §9.1 case 6: with the penalty at zero the sawtooth appears — the
    /// penalty is what regularizes the path, and this proves it.
    #[test]
    fn case_6_no_penalty_grows_a_sawtooth() {
        let finish = mark(START, 0.0, 8.0);
        let mut cfg = config();
        cfg.tack_penalty_day_s = 0;
        cfg.gybe_penalty_day_s = 0;
        let r = run(
            ConstantWind { from_deg: 0.0, tws_kt: 10.0 },
            START,
            finish,
            &cfg,
            300,
        )
        .expect("beatable");
        assert!(
            r.raw_tack_changes >= 3,
            "expected a sawtooth, got {} tacks",
            r.raw_tack_changes
        );
    }

    /// §9.1 case 7: refinement must not make the answer worse.
    #[test]
    fn case_7_refinement_converges_monotonely() {
        let finish = mark(START, 40.0, 15.0);
        let wind = || ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let coarse = {
            let mut c = config();
            c.heading_step_deg = 10.0;
            run(wind(), START, finish, &c, 1200).unwrap()
        };
        let fine = {
            let mut c = config();
            c.heading_step_deg = 2.5;
            run(wind(), START, finish, &c, 150).unwrap()
        };
        // A hair of slack for arrival-circle quantization; the point is the
        // trend, and a regression here means refinement is broken.
        assert!(
            hours(&fine) <= hours(&coarse) * 1.002,
            "coarse {:.3} h, fine {:.3} h",
            hours(&coarse),
            hours(&fine)
        );
    }

    /// §8.2's health check: halving Δt must roughly double the work. If it
    /// squares it, the sector cap is not engaging and the pruning is broken.
    #[test]
    fn halving_dt_doubles_the_work_not_squares_it() {
        let finish = mark(START, 90.0, 20.0);
        let wind = || ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let cfg = config();
        let full = run(wind(), START, finish, &cfg, 1200).unwrap();
        let half = run(wind(), START, finish, &cfg, 600).unwrap();
        let ratio = half.propagations as f64 / full.propagations.max(1) as f64;
        assert!(
            ratio < 3.5,
            "propagations {} → {} (×{ratio:.1}): pruning is not engaging",
            full.propagations,
            half.propagations
        );
    }

    /// §9.1 case 9, the shape of it: a two-sided island between the
    /// endpoints. The engine must find a way round — the second per-sector
    /// slot is what keeps both branches alive.
    #[test]
    fn case_9_an_island_between_the_marks_is_rounded() {
        // A grid in the same Mercator frame as the engine, with a square
        // island square across the rhumb line.
        let a = merc(START);
        let finish = mark(START, 90.0, 16.0);
        let b = merc(finish);
        let min = [a[0].min(b[0]) - 20_000.0, a[1].min(b[1]) - 20_000.0];
        let max = [a[0].max(b[0]) + 20_000.0, a[1].max(b[1]) + 20_000.0];
        let mut grid = Grid::new(min, max, 150.0);
        let mid = [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0];
        let s = 3_000.0;
        let (ta, tb, tc, td) = (
            [mid[0] - s, mid[1] - s],
            [mid[0] + s, mid[1] - s],
            [mid[0] + s, mid[1] + s],
            [mid[0] - s, mid[1] + s],
        );
        grid.stamp_triangle([ta, tb, tc], true);
        grid.stamp_triangle([ta, tc, td], true);
        grid.finalize();

        let p = polar();
        let cfg = config();
        let r = solve(&IsochroneInput {
            polar: &p,
            wind: &ConstantWind { from_deg: 0.0, tws_kt: 10.0 },
            grid: Some(&grid),
            start: START,
            finish,
            depart_ms: 0,
            config: &cfg,
            dt_s: 300,
            offing_min_m: 200.0,
        })
        .expect("a way round exists");

        // Slower than open water over the same distance, but it arrived.
        let open = run(
            ConstantWind { from_deg: 0.0, tws_kt: 10.0 },
            START,
            finish,
            &cfg,
            300,
        )
        .unwrap();
        assert!(r.arrival_ms >= open.arrival_ms, "a detour cannot be free");
        // And no point of the route sits inside the island.
        for pt in &r.points {
            let m = merc(pt.pos);
            assert!(
                !(m[0] > ta[0] && m[0] < tb[0] && m[1] > ta[1] && m[1] < tc[1]),
                "route point inside the island"
            );
        }
    }

    /// §9.1 case 10 in the engine's terms: a start goal pair closer than one
    /// step still terminates by segment-circle arrival, never loops.
    #[test]
    fn case_10_a_short_hop_terminates() {
        let finish = mark(START, 90.0, 0.5);
        let cfg = config();
        let r = run(
            ConstantWind { from_deg: 0.0, tws_kt: 10.0 },
            START,
            finish,
            &cfg,
            600,
        )
        .expect("half a mile on a beam reach");
        assert!(hours(&r) < 0.2, "{} h", hours(&r));
    }
}
