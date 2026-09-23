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

/// Water movement, m/s as (east, north) components — the same §8.5 rule as
/// wind: components interpolate, directions do not.
pub trait CurrentField {
    fn current(&self, pos_merc: [f64; 2], time_ms: i64) -> (f64, f64);
    /// The strongest current the field can produce, for the Π₂ = C/V guard.
    fn max_speed_ms(&self) -> f64;
}

/// Sea state, where known. `None` is "no data here" — a land-masked wave
/// cell, or a position outside the fetched box — and honestly means *no
/// penalty*: near-coast cells the global wave model cannot see are usually
/// sheltered water, and inventing waves there would punish exactly the
/// legs that have none.
pub trait WaveField {
    /// Significant wave height, metres.
    fn wave_height_m(&self, pos_merc: [f64; 2], time_ms: i64) -> Option<f64>;
}

/// The same sea everywhere — the analytic test wave field.
pub struct ConstantWaves {
    pub hs_m: f64,
}

impl WaveField for ConstantWaves {
    fn wave_height_m(&self, _pos: [f64; 2], _time: i64) -> Option<f64> {
        Some(self.hs_m)
    }
}

/// §7.5's hard limits at one place and time: wind over `max_tws_kt` or a sea
/// over `max_wave_m` is a wall, not a cost. Absent wave data means no wall —
/// the global models cannot see sheltered water, and inventing a sea there
/// would forbid exactly the passages that have none.
fn conditions_ok(input: &IsochroneInput<'_>, pos: [f64; 2], time_ms: i64) -> bool {
    let (_, tws) = input.wind.wind(pos, time_ms);
    if tws > input.config.max_tws_kt {
        return false;
    }
    match input.waves.and_then(|w| w.wave_height_m(pos, time_ms)) {
        Some(hs) => hs <= input.config.max_wave_m,
        None => true,
    }
}

/// The same limits *along* a segment, walked at about two miles of ground.
///
/// Sampling the ends is not enough and the midpoint is not either: a band of
/// heavy sea narrower than half the leg slips between the samples, and the
/// leg that jumps it is the fastest one on offer, so it wins. Legs here run
/// from a single step (a few miles) to a closing leg's horizon (24 steps),
/// hence the cap — beyond it the walk coarsens rather than growing without
/// bound.
fn conditions_ok_along(
    input: &IsochroneInput<'_>,
    a: [f64; 2],
    b: [f64; 2],
    time_ms: i64,
    k: f64,
) -> bool {
    const SPACING_NM: f64 = 2.0;
    const MAX_SAMPLES: usize = 32;
    let len_ground_nm = (b[0] - a[0]).hypot(b[1] - a[1]) / (METRES_PER_NM * k);
    let steps = ((len_ground_nm / SPACING_NM).ceil() as usize).clamp(1, MAX_SAMPLES);
    (0..=steps).all(|i| {
        let t = i as f64 / steps as f64;
        let p = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
        conditions_ok(input, p, time_ms)
    })
}

/// The §5 wave penalty: a speed factor 1/(1 + c·Hs²), applied to sail and
/// motor alike (a motor boat pitching into a head sea slows too). Heuristic
/// by declaration — see [`RoutingConfig::wave_penalty_coef`].
fn wave_speed_factor(hs_m: Option<f64>, coef: f64) -> f64 {
    match hs_m {
        Some(h) if h > 0.0 && coef > 0.0 => 1.0 / (1.0 + coef * h * h),
        _ => 1.0,
    }
}

/// A uniform set and drift — M6's analytic test current, and a usable model
/// for a strait with a known stream.
pub struct ConstantCurrent {
    /// Direction the water flows TOWARD, degrees true.
    pub set_deg: f64,
    pub drift_kt: f64,
}

impl CurrentField for ConstantCurrent {
    fn current(&self, _pos: [f64; 2], _time: i64) -> (f64, f64) {
        let ms = self.drift_kt / MS_TO_KNOTS_INV;
        let rad = self.set_deg.to_radians();
        (ms * rad.sin(), ms * rad.cos())
    }
    fn max_speed_ms(&self) -> f64 {
        self.drift_kt / MS_TO_KNOTS_INV
    }
}

const MS_TO_KNOTS_INV: f64 = 3600.0 / 1852.0;

/// A tidal gate: a stretch of water that may only be transited inside its
/// time windows — the hour either side of slack in a sound, the flood
/// through a narrows. §8.3's Π₂ note is why gates come before full current
/// fields: in Danish waters the gates carry most of the value.
#[derive(Debug, Clone)]
pub struct TidalGate {
    pub pos: crate::geo::LatLon,
    pub radius_nm: f64,
    /// Open intervals, ms UTC, ascending.
    pub windows: Vec<(i64, i64)>,
}

impl TidalGate {
    fn open_at(&self, time_ms: i64) -> bool {
        self.windows.iter().any(|(a, b)| (*a..=*b).contains(&time_ms))
    }
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
    tws_kt: f64,
    stw_kt: f64,
    tack: TackState,
    end_time_ms: i64,
    motoring: bool,
}

#[derive(Debug)]
pub enum IsoError {
    /// The frontier died: no candidate anywhere was sailable and legal.
    Unreachable,
    /// Step limit hit before arrival — a guard, not a strategy.
    TookTooLong,
    /// The user cancelled the plan.
    Cancelled,
}

impl std::fmt::Display for IsoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IsoError::Unreachable => write!(
                f,
                "no sailable path: the wind, the polar or the chart closed every door"
            ),
            IsoError::TookTooLong => write!(f, "gave up: no arrival within the step limit"),
            IsoError::Cancelled => write!(f, "cancelled"),
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
    /// Π₂ tripped: current can outrun the boat somewhere, so radial pruning
    /// is not strictly valid and the frontier was widened. Logged too.
    pub pruning_fallback: bool,
    /// Candidate propagations attempted — the §8.2 scaling regression reads
    /// this: halving Δt must roughly double it, never square it.
    pub propagations: u64,
    /// Tack/gybe count of the *raw* path, before post-processing — analytic
    /// case 6 watches the sawtooth appear here.
    pub raw_tack_changes: usize,
}

#[derive(Clone, Copy)]
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
    /// Water movement, if any.
    pub current: Option<&'a dyn CurrentField>,
    /// Sea state, if known; the §7.5 wave limit and the §5 speed penalty
    /// both live off it.
    pub waves: Option<&'a dyn WaveField>,
    /// Tidal gates the passage must respect.
    pub gates: &'a [TidalGate],
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

/// Bumped to abandon every plan in progress. A plan's worker thread records
/// the value it started under ([`begin_cancellable`]); once the two differ,
/// [`solve`] stops at its next ring. A thread that never called
/// `begin_cancellable` — the CLI, the tests — is never cancelled.
static PLAN_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

thread_local! {
    static STARTED_UNDER: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Abandon every plan now running. Their solves return
/// [`IsoError::Cancelled`] at the next ring.
pub fn cancel_plans() {
    PLAN_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// Mark the calling thread's plan as cancellable, and return the generation
/// it belongs to (for [`plan_cancelled`]).
pub fn begin_cancellable() -> u64 {
    let g = PLAN_GENERATION.load(std::sync::atomic::Ordering::SeqCst);
    STARTED_UNDER.with(|c| c.set(Some(g)));
    g
}

/// Whether a plan begun under `generation` has since been cancelled.
pub fn plan_cancelled(generation: u64) -> bool {
    PLAN_GENERATION.load(std::sync::atomic::Ordering::SeqCst) != generation
}

/// Whether the plan running on this thread has been cancelled. Checked by
/// the slow steps before the solve too — the forecast download, the grid
/// search — so Cancel frees the machine, not just the interface.
pub fn this_plan_cancelled() -> bool {
    STARTED_UNDER.with(|c| c.get()).is_some_and(plan_cancelled)
}

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

    // Π₂ = C/V: current faster than the boat makes pockets radial pruning
    // cannot see. The fallback is a wider frontier — more survivors per
    // sector — which is the affordable version of exact region pruning.
    let v_max_kt = (0..=60)
        .map(|tws| input.polar.vmg_at(tws as f64).beat_stw_kt.max(
            input.polar.speed_kt(90.0, tws as f64).unwrap_or(0.0) as f32,
        ))
        .fold(0.0f32, f32::max) as f64;
    let c_max_kt = input
        .current
        .map(|c| c.max_speed_ms() * MS_TO_KNOTS_INV)
        .unwrap_or(0.0);
    let pruning_fallback = c_max_kt > 0.0 && v_max_kt > 0.0 && c_max_kt / v_max_kt > 1.0;
    let per_sector = if pruning_fallback {
        log::warn!(
            "Π₂ guard: current up to {c_max_kt:.1} kt vs boat {v_max_kt:.1} kt —              radial pruning widened (C/V > 1)"
        );
        8
    } else {
        PER_SECTOR
    };

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
    // Closest the frontier has ever come to the mark, and how many rings ago.
    // A frontier that cannot get nearer for this many rings is not working
    // its way round anything — it is milling about behind a closed door, and
    // grinding out the full step limit to say so costs the better part of a
    // minute on a Pi. A beat gains ground every ring; even a foul current
    // does, or the passage was never going to happen.
    const STALL_RINGS: usize = 40;
    let mut nearest_d2 = dist2(start_m, finish_m);
    let mut rings_since_nearer = 0usize;

    for ring in 0..MAX_STEPS {
        if this_plan_cancelled() {
            return Err(IsoError::Cancelled);
        }
        // Rings are equal-time, so once the ring's own clock has caught up
        // with the best analytic arrival, no later node can beat it: closing
        // times are non-negative. Stop, optimally.
        let ring_time = input.depart_ms + ring as i64 * input.dt_s as i64 * 1000;
        if let Some((node, t, ref plan)) = best_arrival {
            if t <= ring_time {
                let plan = plan.clone();
                let mut r = extract(input, &arena, node, t, plan, propagations);
                r.pruning_fallback = pruning_fallback;
                return Ok(r);
            }
        }
        let mut sectors: Vec<Vec<(f32, u32)>> =
            vec![vec![(f32::MIN, u32::MAX); per_sector]; N_SECTORS];
        let mut any_child = false;

        for &pi in &frontier {
            let parent = arena[pi as usize];
            let (wind_from, tws) = input.wind.wind(parent.pos, parent.time_ms);
            if tws > cfg.max_tws_kt {
                continue; // §7.5 hard weather limit: do not sail out of here
            }
            let hs = input
                .waves
                .and_then(|w| w.wave_height_m(parent.pos, parent.time_ms));
            if hs.map(|h| h > cfg.max_wave_m).unwrap_or(false) {
                continue; // §7.5 again: seas over the limit are a wall
            }
            let wave_f = wave_speed_factor(hs, cfg.wave_penalty_coef);
            let current_ms = input
                .current
                .map(|c| c.current(parent.pos, parent.time_ms))
                .unwrap_or((0.0, 0.0));
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
                    wind_from, tws, current_ms, wave_f, tack_pen_ms, gybe_pen_ms,
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
                    .unwrap_or(0.0)
                    * wave_f;
                if sail_toward < cfg.motor_threshold_kt {
                    // The whole fan under engine: in a cross-stream the
                    // compensated heading beats aiming at the mark, and only
                    // a fan candidate can hold it.
                    for i in -fan_half..=fan_half {
                        let h = bearing_fin
                            .wrapping_add(deg_to_bam(i as f64 * cfg.heading_step_deg as f64));
                        candidates.push((h, Some(motor)));
                    }
                }
            }

            for (heading, motor) in candidates {
                propagations += 1;
                let twa_signed = signed_diff_deg(heading, wind_from);
                let (stw, motoring) = match motor {
                    Some(m) => (m * wave_f, true),
                    None => match input.polar.speed_kt(twa_signed.abs(), tws) {
                        Some(s) if s > 0.05 => (s * wave_f, false),
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
                // The water carries the boat the whole step, penalty time
                // included — a tack in a 2 kt stream still drifts.
                let (cu, cv) = current_ms;
                let drift = input.dt_s as f64; // seconds of set and drift
                let child_pos = [
                    parent.pos[0] + step * dir.sin() + cu * drift * k,
                    parent.pos[1] + step * dir.cos() + cv * drift * k,
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
                // The parent's own conditions were checked before the fan was
                // built; the rest of the step is checked here, because a step
                // is up to three hours long and the weather at its far end is
                // not the weather at its start.
                if !conditions_ok_along(input, parent.pos, child_pos, child_time, k) {
                    continue;
                }
                // Tidal gates: a segment through a gate's water outside its
                // window is as blocked as land is.
                if gate_blocks(input.gates, parent.pos, child_pos, child_time, k) {
                    continue;
                }

                // §7.6 pruning: a child that reaches no further out in its
                // sector than the ones already kept is dominated.
                let r2 = dist2(child_pos, start_m) as f32;
                let sector =
                    (plane_bearing(start_m, child_pos) >> SECTOR_SHIFT) as usize & (N_SECTORS - 1);
                let slots = &mut sectors[sector];
                let min_slot = (0..per_sector)
                    .min_by(|&a, &b| slots[a].0.total_cmp(&slots[b].0))
                    .unwrap();
                if r2 <= slots[min_slot].0 {
                    continue;
                }
                let _ = motoring; // recorded via twa/stw; stepped motor legs are rare
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
                Some((node, t, plan)) => {
                    let mut r = extract(input, &arena, node, t, plan, propagations);
                    r.pruning_fallback = pruning_fallback;
                    Ok(r)
                }
                None => Err(IsoError::Unreachable),
            };
        }
        frontier = sectors
            .iter()
            .flatten()
            .filter(|(_, i)| *i != u32::MAX)
            .map(|&(_, i)| i)
            .collect();

        // Progress, or the lack of it. Only while no arrival is in hand —
        // once one is, the loop is already bounded by the optimal-stop rule
        // above and must not be cut short.
        if best_arrival.is_none() {
            let ring_nearest = frontier
                .iter()
                .map(|&i| dist2(arena[i as usize].pos, finish_m))
                .fold(f64::INFINITY, f64::min);
            if ring_nearest < nearest_d2 {
                nearest_d2 = ring_nearest;
                rings_since_nearer = 0;
            } else {
                rings_since_nearer += 1;
                if rings_since_nearer >= STALL_RINGS {
                    log::info!(
                        "isochrone: no ground gained on the mark for {STALL_RINGS} rings — \
                         giving up at ring {ring}"
                    );
                    return Err(IsoError::Unreachable);
                }
            }
        }
    }
    match best_arrival {
        Some((node, t, plan)) => {
            let mut r = extract(input, &arena, node, t, plan, propagations);
            r.pruning_fallback = pruning_fallback;
            Ok(r)
        }
        None => Err(IsoError::TookTooLong),
    }
}

/// Does any gate forbid this segment at this time? The check is coarse on
/// purpose — a segment passing within the gate's radius while it is shut —
/// because a gate is itself a modelling of something coarser still.
fn gate_blocks(
    gates: &[TidalGate],
    a: [f64; 2],
    b: [f64; 2],
    time_ms: i64,
    k: f64,
) -> bool {
    for gate in gates {
        let (gx, gy) = Projection::to_mercator(gate.pos.lat, gate.pos.lon);
        let r = gate.radius_nm * METRES_PER_NM * k;
        if segment_near(a, b, [gx, gy], r) && !gate.open_at(time_ms) {
            return true;
        }
    }
    false
}

fn segment_near(a: [f64; 2], b: [f64; 2], c: [f64; 2], radius: f64) -> bool {
    let ab = [b[0] - a[0], b[1] - a[1]];
    let ac = [c[0] - a[0], c[1] - a[1]];
    let len2 = ab[0] * ab[0] + ab[1] * ab[1];
    let t = if len2 <= f64::EPSILON {
        0.0
    } else {
        ((ac[0] * ab[0] + ac[1] * ab[1]) / len2).clamp(0.0, 1.0)
    };
    let p = [a[0] + ab[0] * t, a[1] + ab[1] * t];
    dist2(p, c) <= radius * radius
}

/// M6's DoD in one function: try several departures, keep the best arrival.
/// A tidal gate that shuts the direct water makes "leave later, arrive
/// earlier" a real phenomenon, and this is how the engine finds it.
pub fn best_departure(
    input: &IsochroneInput<'_>,
    departures_ms: &[i64],
) -> Option<(i64, Isoroute)> {
    let mut best: Option<(i64, Isoroute)> = None;
    for &depart in departures_ms {
        let candidate = IsochroneInput {
            depart_ms: depart,
            ..*input
        };
        match solve(&candidate) {
            Ok(route) => {
                if best
                    .as_ref()
                    .map(|(_, b)| route.arrival_ms < b.arrival_ms)
                    .unwrap_or(true)
                {
                    best = Some((depart, route));
                }
            }
            Err(_) => continue,
        }
    }
    best
}

/// The analytic final approach from a node: the cheapest of the straight
/// heading and (inside a no-go cone) the two-leg VMG wedge — now with the
/// water's own movement folded in exactly.
///
/// With a constant current C this is Zermelo in closed form: a motor leg
/// solves the quadratic |d − C·t| = V·t; a sailing wedge solves the 2×2
/// system (V·e₊ + C)x + (V·e₋ + C)y = d for the two leg times; a sailed
/// straight line iterates heading-against-drift to a fixed point, which
/// converges whenever the boat outruns the stream. Penalties are charged,
/// both wedge orders tried, and every implied segment checked against the
/// chart and the tidal gates.
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
    current_ms: (f64, f64),
    wave_f: f64,
    tack_pen_ms: i64,
    gybe_pen_ms: i64,
) -> Option<Vec<ClosingLeg>> {
    let d_vec = [finish_m[0] - pos[0], finish_m[1] - pos[1]];
    let d_plane = (d_vec[0] * d_vec[0] + d_vec[1] * d_vec[1]).sqrt();
    if d_plane < 1.0 {
        return Some(Vec::new());
    }
    // Everything in plane metres and seconds; k converts ground speeds in.
    let c_pl = [current_ms.0 * k, current_ms.1 * k];
    let kt_to_pl = |kt: f64| kt / (3600.0 / METRES_PER_NM) * k; // kt → plane m/s
    let bearing = plane_bearing(pos, finish_m);
    let seg_ok = |a: [f64; 2], b: [f64; 2], t_ms: i64| {
        let chart = match input.grid {
            Some(g) => g.segment_clear(a, b, input.offing_min_m * k),
            None => true,
        };
        // §7.5's hard limits hold along the leg, not merely where it starts.
        // A closing leg is not short — offshore the horizon is 24·dt, some
        // 72 nm at a three-hour step — so a leg that begins in calm water can
        // cross a gale or a sea over the limit entirely unseen. Worse, a
        // straight line through the weather beats any honest detour around
        // it, so the unchecked leg does not merely slip through: it wins.
        chart
            && conditions_ok_along(input, a, b, t_ms, k)
            && !gate_blocks(input.gates, a, b, t_ms, k)
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

    // Zermelo's quadratic for a fixed speed: (C²−V²)t² − 2(d·C)t + d² = 0,
    // smallest positive root. `None` when the stream wins outright.
    let fixed_speed_time = |v_pl: f64| -> Option<f64> {
        let c2 = c_pl[0] * c_pl[0] + c_pl[1] * c_pl[1];
        let dc = d_vec[0] * c_pl[0] + d_vec[1] * c_pl[1];
        let a = c2 - v_pl * v_pl;
        if a.abs() < 1e-9 {
            let t = d_plane * d_plane / (2.0 * dc);
            return (dc > 0.0 && t > 0.0).then_some(t);
        }
        let disc = dc * dc - a * d_plane * d_plane;
        if disc < 0.0 {
            return None;
        }
        let sq = disc.sqrt();
        [(dc - sq) / a, (dc + sq) / a]
            .into_iter()
            .filter(|t| *t > 0.0)
            .fold(None, |acc: Option<f64>, t| Some(acc.map_or(t, |a| a.min(t))))
    };

    // Straight at it under sail: heading and speed depend on each other
    // through the drift, so iterate — 6 rounds is metres.
    {
        let mut t_s = None;
        let mut heading = bearing;
        let mut stw = 0.0;
        for _ in 0..6 {
            let twa = signed_diff_deg(heading, wind_from);
            let Some(v) = input.polar.speed_kt(twa.abs(), tws).map(|v| v * wave_f) else {
                t_s = None;
                break;
            };
            stw = v;
            let Some(t) = fixed_speed_time(kt_to_pl(v)) else {
                t_s = None;
                break;
            };
            t_s = Some(t);
            // Steer for where the mark will be relative to the water.
            let aim = [d_vec[0] - c_pl[0] * t, d_vec[1] - c_pl[1] * t];
            heading = deg_to_bam(aim[0].atan2(aim[1]).to_degrees());
        }
        if let Some(t) = t_s {
            let twa = signed_diff_deg(heading, wind_from);
            if stw > 0.05 && seg_ok(pos, finish_m, time_ms) {
                let end = time_ms + pen(tack_now, twa) + (t * 1000.0) as i64;
                consider(vec![ClosingLeg {
                    end_pos: finish_m,
                    heading,
                    twa_deg: twa,
                    tws_kt: tws,
                    stw_kt: stw,
                    tack: tack_of(twa),
                    end_time_ms: end,
                    motoring: false,
                }]);
            }
        }
    }
    // Motoring: fixed speed, exact Zermelo.
    if let Some(m) = input.config.motor_speed_kt.map(|m| m * wave_f) {
        if let Some(t) = fixed_speed_time(kt_to_pl(m)) {
            if seg_ok(pos, finish_m, time_ms) {
                let aim = [d_vec[0] - c_pl[0] * t, d_vec[1] - c_pl[1] * t];
                let heading = deg_to_bam(aim[0].atan2(aim[1]).to_degrees());
                consider(vec![ClosingLeg {
                    end_pos: finish_m,
                    heading,
                    twa_deg: signed_diff_deg(heading, wind_from),
                    tws_kt: tws,
                    stw_kt: m,
                    tack: tack_now,
                    end_time_ms: time_ms + (t * 1000.0) as i64,
                    motoring: true,
                }]);
            }
        }
    }

    // The wedges, drift folded into the basis: leg times x, y solve
    // (V·e₊ + C)x + (V·e₋ + C)y = d.
    let vmg = input.polar.vmg_at(tws);
    let twa_direct = signed_diff_deg(bearing, wind_from);
    for (cone_twa, stw) in [
        (vmg.beat_twa_deg as f64, vmg.beat_stw_kt as f64 * wave_f),
        (vmg.run_twa_deg as f64, vmg.run_stw_kt as f64 * wave_f),
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
        let v_pl = kt_to_pl(stw);
        let h_plus = wind_from.wrapping_add(deg_to_bam(cone_twa));
        let h_minus = wind_from.wrapping_add(deg_to_bam(-cone_twa));
        let (rp, rm) = (bam_to_rad(h_plus), bam_to_rad(h_minus));
        let gp = [v_pl * rp.sin() + c_pl[0], v_pl * rp.cos() + c_pl[1]];
        let gm = [v_pl * rm.sin() + c_pl[0], v_pl * rm.cos() + c_pl[1]];
        let det = gp[0] * gm[1] - gp[1] * gm[0];
        if det.abs() < 1e-9 {
            continue;
        }
        let x = (d_vec[0] * gm[1] - d_vec[1] * gm[0]) / det;
        let y = (gp[0] * d_vec[1] - gp[1] * d_vec[0]) / det;
        if x < 0.0 || y < 0.0 {
            continue;
        }
        for first_plus in [true, false] {
            let (t1, h1, twa1, g1) = if first_plus {
                (x, h_plus, cone_twa, gp)
            } else {
                (y, h_minus, -cone_twa, gm)
            };
            let (t2, h2, twa2) = if first_plus {
                (y, h_minus, -cone_twa)
            } else {
                (x, h_plus, cone_twa)
            };
            let mid = [pos[0] + g1[0] * t1, pos[1] + g1[1] * t1];
            let p1 = pen(tack_now, twa1);
            let p2 = pen(tack_of(twa1), twa2);
            let mid_t = time_ms + p1 + (t1 * 1000.0) as i64;
            let end_t = mid_t + p2 + (t2 * 1000.0) as i64;
            if !seg_ok(pos, mid, mid_t) || !seg_ok(mid, finish_m, end_t) {
                continue;
            }
            consider(vec![
                ClosingLeg {
                    end_pos: mid,
                    heading: h1,
                    twa_deg: twa1,
                    tws_kt: tws,
                    stw_kt: stw,
                    tack: tack_of(twa1),
                    end_time_ms: mid_t,
                    motoring: false,
                },
                ClosingLeg {
                    end_pos: finish_m,
                    heading: h2,
                    twa_deg: twa2,
                    tws_kt: tws,
                    stw_kt: stw,
                    tack: tack_of(twa2),
                    end_time_ms: end_t,
                    motoring: false,
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
            tws_kt: leg.tws_kt,
            stw_kt: leg.stw_kt,
            tack: leg.tack,
            motoring: leg.motoring,
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
        pruning_fallback: false, // overwritten by solve()
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
            waves: None,
            current: None,
            gates: &[],
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

    /// §9.1 case 4: constant wind + constant current has Zermelo's closed
    /// form. Under engine at V with a cross-stream C, T = L/√(V²−C²).
    #[test]
    fn case_4_constant_current_matches_zermelo() {
        let l_nm = 10.0;
        let finish = mark(START, 0.0, l_nm);
        let mut cfg = config();
        cfg.motor_speed_kt = Some(5.0);
        let p = polar();
        let current = ConstantCurrent { set_deg: 90.0, drift_kt: 2.0 };
        let r = solve(&IsochroneInput {
            polar: &p,
            wind: &ConstantWind { from_deg: 0.0, tws_kt: 0.0 }, // calm: engine only
            grid: None,
            start: START,
            finish,
            depart_ms: 0,
            config: &cfg,
            dt_s: 300,
            offing_min_m: 0.0,
            waves: None,
            current: Some(&current),
            gates: &[],
        })
        .expect("motors across the stream");
        let expected_h = l_nm / (5.0f64.powi(2) - 2.0f64.powi(2)).sqrt();
        let got = hours(&r);
        assert!(
            (got - expected_h).abs() / expected_h < 0.01,
            "Zermelo says {expected_h:.3} h, engine {got:.3} h"
        );
        assert!(!r.pruning_fallback, "C/V = 0.4 must not trip the guard");
    }

    /// §9.1 case 8: current faster than the boat trips the Π₂ guard and the
    /// engine says so rather than quietly pruning wrong.
    #[test]
    fn case_8_overwhelming_current_engages_the_fallback() {
        let finish = mark(START, 90.0, 8.0);
        let p = polar();
        let cfg = config();
        // 12 kt of stream against a 7.5 kt boat, running WITH us — reachable,
        // but the reachable set is nothing like star-shaped.
        let current = ConstantCurrent { set_deg: 90.0, drift_kt: 12.0 };
        let r = solve(&IsochroneInput {
            polar: &p,
            wind: &ConstantWind { from_deg: 0.0, tws_kt: 10.0 },
            grid: None,
            start: START,
            finish,
            depart_ms: 0,
            config: &cfg,
            dt_s: 300,
            offing_min_m: 0.0,
            waves: None,
            current: Some(&current),
            gates: &[],
        })
        .expect("a following stream can only help");
        assert!(r.pruning_fallback, "C/V > 1 must engage the fallback");
        // Carried by the stream, arrival beats still water.
        let still = run(
            ConstantWind { from_deg: 0.0, tws_kt: 10.0 },
            START,
            finish,
            &config(),
            300,
        )
        .unwrap();
        assert!(r.arrival_ms < still.arrival_ms);
    }

    /// M6's DoD: a tidal gate across the passage delays departure to carry
    /// fair tide — leaving later arrives earlier.
    #[test]
    fn a_tidal_gate_delays_departure_to_carry_fair_tide() {
        let l_nm = 12.0;
        let finish = mark(START, 90.0, l_nm);
        // First attempt at this fixture used a 5 nm gate disc — and the
        // engine sailed around it, which is correct seamanship and a wrong
        // test. The gate must BE the sound: wide enough that every path
        // crosses it.
        let gate = TidalGate {
            pos: mark(START, 90.0, l_nm / 2.0),
            radius_nm: 40.0,
            // Open only from t=2 h to t=4 h.
            windows: vec![(2 * 3_600_000, 4 * 3_600_000)],
        };
        let p = polar();
        let cfg = config();
        let gates = [gate];
        let input = IsochroneInput {
            polar: &p,
            wind: &ConstantWind { from_deg: 0.0, tws_kt: 10.0 },
            grid: None,
            start: START,
            finish,
            depart_ms: 0,
            config: &cfg,
            dt_s: 600,
            offing_min_m: 0.0,
            waves: None,
            current: None,
            gates: &gates,
        };
        // Departing at once, the boat reaches the shut gate and dies there.
        assert!(solve(&input).is_err(), "the gate is shut for an immediate start");

        // The scan finds a departure that carries the tide.
        let departures: Vec<i64> = (0..8).map(|h| h * 1_800_000).collect(); // every 30 min
        let (depart, route) =
            best_departure(&input, &departures).expect("some departure works");
        assert!(
            depart >= 2 * 3_600_000,
            "departure must wait for the window, got {} min",
            depart / 60_000
        );
        // And the transit of the gate itself happened inside the window.
        assert!(route.arrival_ms > 2 * 3_600_000);
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
            waves: None,
            current: None,
            gates: &[],
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

    /// The wave penalty is a pure speed factor, so on case 2's beam reach a
    /// constant 2 m sea must stretch the passage by exactly 1 + c·Hs².
    #[test]
    fn waves_stretch_a_beam_reach_by_the_declared_factor() {
        let l_nm = 20.0;
        let finish = mark(START, 90.0, l_nm);
        let p = polar();
        let cfg = config();
        let waves = ConstantWaves { hs_m: 2.0 };
        let r = solve(&IsochroneInput {
            polar: &p,
            wind: &ConstantWind { from_deg: 0.0, tws_kt: 10.0 },
            grid: None,
            start: START,
            finish,
            depart_ms: 0,
            config: &cfg,
            dt_s: 600,
            offing_min_m: 0.0,
            waves: Some(&waves),
            current: None,
            gates: &[],
        })
        .expect("a rough beam reach still sails");
        let factor = 1.0 + cfg.wave_penalty_coef * 4.0;
        let expected_h = l_nm / 6.0 * factor; // polar(90°, 10 kt) = 6.0
        let got = hours(&r);
        assert!(
            (got - expected_h).abs() / expected_h < 0.005,
            "expected {expected_h:.3} h, got {got:.3} h"
        );
    }

    /// §7.5: seas over the limit are a wall, exactly like wind over the
    /// limit — everywhere over means no route at all.
    #[test]
    fn waves_over_the_limit_block_the_passage() {
        let finish = mark(START, 90.0, 10.0);
        let p = polar();
        let cfg = config();
        let waves = ConstantWaves { hs_m: cfg.max_wave_m + 0.5 };
        let r = solve(&IsochroneInput {
            polar: &p,
            wind: &ConstantWind { from_deg: 0.0, tws_kt: 10.0 },
            grid: None,
            start: START,
            finish,
            depart_ms: 0,
            config: &cfg,
            dt_s: 600,
            offing_min_m: 0.0,
            waves: Some(&waves),
            current: None,
            gates: &[],
        });
        assert!(matches!(r, Err(IsoError::Unreachable)));
    }

    /// A sea over the limit lying between a node and the mark must stop the
    /// route even though the node itself is in calm water.
    ///
    /// This is the case the analytic closing legs used to sail straight
    /// through: the limit was sampled where the leg *began*, and offshore a
    /// closing leg reaches 24 time-steps ahead. Worse than merely missing
    /// the hazard, a straight line through it beats every honest detour, so
    /// the illegal route won.
    #[test]
    fn a_band_of_heavy_sea_across_the_approach_is_a_wall() {
        /// Calm everywhere except a band of latitude before the finish.
        struct BandWaves {
            lat_from: f64,
            lat_to: f64,
            hs_m: f64,
        }
        impl WaveField for BandWaves {
            fn wave_height_m(&self, pos: [f64; 2], _t: i64) -> Option<f64> {
                let (lat, _) = Projection::to_wgs84(pos[0], pos[1]);
                Some(if lat >= self.lat_from && lat <= self.lat_to {
                    self.hs_m
                } else {
                    0.0
                })
            }
        }

        let p = polar();
        let cfg = config();
        // Due north, with the band straddling the middle of the passage.
        let finish = mark(START, 0.0, 24.0);
        let waves = BandWaves {
            lat_from: START.lat + 0.10,
            lat_to: START.lat + 0.20,
            hs_m: cfg.max_wave_m + 2.0,
        };
        let input = IsochroneInput {
            polar: &p,
            wind: &ConstantWind { from_deg: 270.0, tws_kt: 12.0 },
            grid: None,
            start: START,
            finish,
            depart_ms: 0,
            config: &cfg,
            dt_s: 1800,
            offing_min_m: 0.0,
            waves: Some(&waves),
            current: None,
            gates: &[],
        };
        let r = solve(&input);
        assert!(
            r.is_err(),
            "a sea over the limit spanning the whole approach must stop the route"
        );

        // And the control: the same passage with the band merely rough — not
        // over the limit — does sail, so the wall above is the limit doing
        // its work and not the fixture being unsailable.
        let rough = BandWaves {
            lat_from: START.lat + 0.10,
            lat_to: START.lat + 0.20,
            hs_m: cfg.max_wave_m - 1.0,
        };
        let ok = solve(&IsochroneInput { waves: Some(&rough), ..input });
        assert!(ok.is_ok(), "a rough but legal sea must still be sailable");
    }

    #[test]
    fn the_wave_factor_is_neutral_without_data_or_coefficient() {
        assert_eq!(wave_speed_factor(None, 0.03), 1.0);
        assert_eq!(wave_speed_factor(Some(2.0), 0.0), 1.0);
        let f = wave_speed_factor(Some(2.0), 0.03);
        assert!((f - 1.0 / 1.12).abs() < 1e-12);
    }
}
