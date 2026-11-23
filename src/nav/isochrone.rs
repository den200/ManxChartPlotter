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
    t_a: i64,
    t_b: i64,
    k: f64,
) -> bool {
    const SPACING_NM: f64 = 2.0;
    const MAX_SAMPLES: usize = 32;
    let len_ground_nm = (b[0] - a[0]).hypot(b[1] - a[1]) / (METRES_PER_NM * k);
    let steps = ((len_ground_nm / SPACING_NM).ceil() as usize).clamp(1, MAX_SAMPLES);
    (0..=steps).all(|i| {
        let t = i as f64 / steps as f64;
        let p = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
        // Each sample at the time the boat is there: a leg is hours long,
        // and judging all of it at one instant misses the gale that arrives
        // while the boat is halfway along.
        conditions_ok(input, p, t_a + ((t_b - t_a) as f64 * t) as i64)
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
    /// Is the gate open for the whole of a passage through its water? One
    /// window must hold both the moment the boat enters and the moment it
    /// leaves: open at either end alone is a boat in the gate when it shuts.
    fn open_through(&self, enter_ms: i64, leave_ms: i64) -> bool {
        self.windows
            .iter()
            .any(|(a, b)| *a <= enter_ms && leave_ms <= *b)
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
pub(crate) struct ClosingLeg {
    pub(crate) end_pos: [f64; 2],
    pub(crate) heading: Bam,
    pub(crate) twa_deg: f64,
    pub(crate) tws_kt: f64,
    pub(crate) stw_kt: f64,
    pub(crate) tack: TackState,
    pub(crate) end_time_ms: i64,
    pub(crate) motoring: bool,
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
    if pruning_fallback {
        log::warn!(
            "Π₂ guard: current up to {c_max_kt:.1} kt vs boat {v_max_kt:.1} kt — radial pruning off, one node per cell (C/V > 1)"
        );
    }

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
            vec![vec![(f32::MIN, u32::MAX); PER_SECTOR]; N_SECTORS];
        // Under Π₂ the radial prune is off: see the cell test below.
        let mut cells: std::collections::BTreeMap<(i64, i64), (f64, u32)> =
            std::collections::BTreeMap::new();
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
            // A step and a half at a brisk 8 kn, never less than 3 NM. The
            // closing legs sail on the weather of the node they start from,
            // so a longer horizon freezes the forecast over more of the
            // route — at three steps offshore that was 72 NM of it.
            let horizon = (3.0 * METRES_PER_NM * k)
                .max(1.5 * 8.0 * dt_h * METRES_PER_NM * k);
            if d_plane <= horizon {
                if let Some(legs) = closing_legs(
                    input, parent.pos, parent.time_ms, (pi != 0).then_some(parent.tack), finish_m, k,
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
            // And a coarse ring behind the fan: the way out of a bay that
            // opens away from the mark is a heading the fan never offers.
            // Pruning drops them at once where they gain nothing.
            let mut behind = cfg.max_diverted_deg as f64 + 20.0;
            while behind <= 360.0 - cfg.max_diverted_deg as f64 - 20.0 {
                candidates.push((bearing_fin.wrapping_add(deg_to_bam(behind)), None));
                behind += 20.0;
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
                if !conditions_ok_along(input, parent.pos, child_pos, parent.time_ms, child_time, k) {
                    continue;
                }
                // Tidal gates: a segment through a gate's water outside its
                // window is as blocked as land is.
                if gate_blocks(input.gates, parent.pos, child_pos, parent.time_ms, child_time, k) {
                    continue;
                }

                // §7.6 pruning. Rings are of equal time, so a child is
                // dominated only by a child that stands where it does. With
                // the current able to outrun the boat, a node that holds
                // station near the start is not dominated by one farther out
                // in its sector — the stream sets the far one back — so there
                // the test is by place alone: one node per cell, the one
                // nearer the mark. Otherwise a child that reaches no further
                // out in its sector than the ones kept is dominated.
                let index = arena.len() as u32;
                let admitted = if pruning_fallback {
                    let cell_m = 0.25 * METRES_PER_NM * k;
                    let key = (
                        (child_pos[0] / cell_m).floor() as i64,
                        (child_pos[1] / cell_m).floor() as i64,
                    );
                    let to_go = dist2(child_pos, finish_m);
                    match cells.get(&key) {
                        Some(&(kept, _)) if kept <= to_go => false,
                        _ => {
                            cells.insert(key, (to_go, index));
                            true
                        }
                    }
                } else {
                    let r2 = dist2(child_pos, start_m) as f32;
                    let sector =
                        (plane_bearing(start_m, child_pos) >> SECTOR_SHIFT) as usize & (N_SECTORS - 1);
                    let slots = &mut sectors[sector];
                    let min_slot = (0..PER_SECTOR)
                        .min_by(|&a, &b| slots[a].0.total_cmp(&slots[b].0))
                        .unwrap();
                    if r2 <= slots[min_slot].0 {
                        false
                    } else {
                        slots[min_slot] = (r2, index);
                        true
                    }
                };
                if !admitted {
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
        frontier = if pruning_fallback {
            cells.values().map(|&(_, i)| i).collect()
        } else {
            sectors
                .iter()
                .flatten()
                .filter(|(_, i)| *i != u32::MAX)
                .map(|&(_, i)| i)
                .collect()
        };

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
/// Does a leg sailed from `a` at `t_a` to `b` at `t_b` pass through a
/// gate's water while it is shut?
///
/// The boat is taken to move along the leg at a steady pace, so the stretch
/// inside the gate's circle is also a stretch of time. Checking a single
/// instant per leg — its start, or its end — lets a long leg slip through a
/// gate that shuts while the boat is in it, and a closing leg can be tens of
/// miles long.
fn gate_blocks(
    gates: &[TidalGate],
    a: [f64; 2],
    b: [f64; 2],
    t_a: i64,
    t_b: i64,
    k: f64,
) -> bool {
    gates.iter().any(|gate| {
        let (gx, gy) = Projection::to_mercator(gate.pos.lat, gate.pos.lon);
        let r = gate.radius_nm * METRES_PER_NM * k;
        match circle_span(a, b, [gx, gy], r) {
            Some((f_in, f_out)) => {
                let at = |f: f64| t_a + ((t_b - t_a) as f64 * f) as i64;
                !gate.open_through(at(f_in), at(f_out))
            }
            None => false,
        }
    })
}

/// The part of segment a→b inside the circle round `c`, as fractions of the
/// segment, or `None` when the segment never enters it.
fn circle_span(a: [f64; 2], b: [f64; 2], c: [f64; 2], radius: f64) -> Option<(f64, f64)> {
    // |a + f·(b−a) − c|² = r², a quadratic in f.
    let ab = [b[0] - a[0], b[1] - a[1]];
    let ca = [a[0] - c[0], a[1] - c[1]];
    let qa = ab[0] * ab[0] + ab[1] * ab[1];
    let qb = 2.0 * (ab[0] * ca[0] + ab[1] * ca[1]);
    let qc = ca[0] * ca[0] + ca[1] * ca[1] - radius * radius;
    if qa <= f64::EPSILON {
        // A leg that goes nowhere is inside or it is not.
        return (qc <= 0.0).then_some((0.0, 0.0));
    }
    let disc = qb * qb - 4.0 * qa * qc;
    if disc < 0.0 {
        return None;
    }
    let sq = disc.sqrt();
    let (f1, f2) = ((-qb - sq) / (2.0 * qa), (-qb + sq) / (2.0 * qa));
    if f2 < 0.0 || f1 > 1.0 {
        return None;
    }
    Some((f1.max(0.0), f2.min(1.0)))
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
pub(crate) fn closing_legs(
    input: &IsochroneInput<'_>,
    pos: [f64; 2],
    time_ms: i64,
    // `None` when the boat is only now leaving: its first heading is a
    // choice, not a manoeuvre, and costs no tack or gybe.
    tack_now: Option<TackState>,
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
    // `t0` and `t1` are when the leg is begun and finished; the weather and
    // the gates are judged at the time the boat reaches each part of it.
    let seg_ok = |a: [f64; 2], b: [f64; 2], t0: i64, t1: i64| {
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
            && conditions_ok_along(input, a, b, t0, t1, k)
            && !gate_blocks(input.gates, a, b, t0, t1, k)
    };
    let tack_of = |twa: f64| if twa >= 0.0 { TackState::Port } else { TackState::Starboard };
    let pen = |from: Option<TackState>, twa: f64| -> i64 {
        if from.is_some_and(|from| tack_of(twa) != from) {
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
    let fixed_speed_time = |d: [f64; 2], v_pl: f64| -> Option<f64> {
        let d2 = d[0] * d[0] + d[1] * d[1];
        let c2 = c_pl[0] * c_pl[0] + c_pl[1] * c_pl[1];
        let dc = d[0] * c_pl[0] + d[1] * c_pl[1];
        let a = c2 - v_pl * v_pl;
        if a.abs() < 1e-9 {
            let t = d2 / (2.0 * dc);
            return (dc > 0.0 && t > 0.0).then_some(t);
        }
        let disc = dc * dc - a * d2;
        if disc < 0.0 {
            return None;
        }
        let sq = disc.sqrt();
        [(dc - sq) / a, (dc + sq) / a]
            .into_iter()
            .filter(|t| *t > 0.0)
            .fold(None, |acc: Option<f64>, t| Some(acc.map_or(t, |a| a.min(t))))
    };

    // The distance still to sail once the water has carried the boat through
    // the tack or gybe that begins a leg on `twa`: the boat sits in the turn
    // while the stream sets it on, so the sailing covers what is left.
    let through_penalty = |twa: f64| -> [f64; 2] {
        let p = pen(tack_now, twa) as f64 / 1000.0;
        [d_vec[0] - c_pl[0] * p, d_vec[1] - c_pl[1] * p]
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
            let d_eff = through_penalty(twa);
            let Some(t) = fixed_speed_time(d_eff, kt_to_pl(v)) else {
                t_s = None;
                break;
            };
            t_s = Some(t);
            // Steer for where the mark will be relative to the water.
            let aim = [d_eff[0] - c_pl[0] * t, d_eff[1] - c_pl[1] * t];
            heading = deg_to_bam(aim[0].atan2(aim[1]).to_degrees());
        }
        // The loop leaves `heading` one update past the speed it computed:
        // check the final heading is sailable, at the speed it actually
        // gives, and that the iteration settled — a heading that is still
        // swinging, or one the current has pushed into the no-go zone, is
        // not a leg.
        let t_s = t_s.and_then(|t| {
            let twa = signed_diff_deg(heading, wind_from);
            let v = input.polar.speed_kt(twa.abs(), tws)? * wave_f;
            let t2 = fixed_speed_time(through_penalty(twa), kt_to_pl(v))?;
            stw = v;
            ((t2 - t).abs() <= 0.02 * t.max(1.0)).then_some(t2)
        });
        if let Some(t) = t_s {
            let twa = signed_diff_deg(heading, wind_from);
            let start = time_ms + pen(tack_now, twa);
            let end = start + (t * 1000.0) as i64;
            if stw > 0.05 && seg_ok(pos, finish_m, start, end) {
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
        if let Some(t) = fixed_speed_time(d_vec, kt_to_pl(m)) {
            if seg_ok(pos, finish_m, time_ms, time_ms + (t * 1000.0) as i64) {
                let aim = [d_vec[0] - c_pl[0] * t, d_vec[1] - c_pl[1] * t];
                let heading = deg_to_bam(aim[0].atan2(aim[1]).to_degrees());
                consider(vec![ClosingLeg {
                    end_pos: finish_m,
                    heading,
                    twa_deg: signed_diff_deg(heading, wind_from),
                    tws_kt: tws,
                    stw_kt: m,
                    tack: tack_now.unwrap_or_else(|| tack_of(signed_diff_deg(heading, wind_from))),
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
        // Each order of the two legs is its own problem: the penalties differ
        // between them, and so does the drift the water adds during each.
        for first_plus in [true, false] {
            let (h1, twa1, g1, h2, twa2, g2) = if first_plus {
                (h_plus, cone_twa, gp, h_minus, -cone_twa, gm)
            } else {
                (h_minus, -cone_twa, gm, h_plus, cone_twa, gp)
            };
            let p1 = pen(tack_now, twa1);
            let p2 = pen(Some(tack_of(twa1)), twa2);
            // The water carries the boat through both penalties as well.
            let drift = |p: i64| [c_pl[0] * p as f64 / 1000.0, c_pl[1] * p as f64 / 1000.0];
            let (before, between) = (drift(p1), drift(p2));
            let rest = [d_vec[0] - before[0] - between[0], d_vec[1] - before[1] - between[1]];
            let det = g1[0] * g2[1] - g1[1] * g2[0];
            if det.abs() < 1e-9 {
                continue;
            }
            let t1 = (rest[0] * g2[1] - rest[1] * g2[0]) / det;
            let t2 = (g1[0] * rest[1] - g1[1] * rest[0]) / det;
            if t1 < 0.0 || t2 < 0.0 {
                continue;
            }
            let mid = [pos[0] + before[0] + g1[0] * t1, pos[1] + before[1] + g1[1] * t1];
            let mid_t = time_ms + p1 + (t1 * 1000.0) as i64;
            let end_t = mid_t + p2 + (t2 * 1000.0) as i64;
            if !seg_ok(pos, mid, time_ms + p1, mid_t) || !seg_ok(mid, finish_m, mid_t + p2, end_t) {
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
    keep.sort_unstable();
    keep.dedup();
    // Simplifying moves legs sideways by up to the tolerance, and a leg is
    // only known safe where the chart has checked it. Wherever a simplified
    // leg is not clear, put back the dropped vertex that lay furthest off
    // it; every original step was clear, so this ends, at worst, with the
    // raw chain.
    loop {
        let bad = keep.windows(2).position(|w| {
            w[1] > w[0] + 1 && !leg_clear(input, chain[w[0]].pos, chain[w[1]].pos)
        });
        let Some(i) = bad else { break };
        let (a, b) = (keep[i], keep[i + 1]);
        let (pa, pb) = (chain[a].pos, chain[b].pos);
        let worst = (a + 1..b)
            .max_by(|&x, &y| {
                point_segment_dist(chain[x].pos, pa, pb)
                    .total_cmp(&point_segment_dist(chain[y].pos, pa, pb))
            })
            .unwrap();
        keep.insert(i + 1, worst);
    }

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
        if straight && b.tack == c.tack && leg_clear(input, merc(a.pos), merc(c.pos)) {
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

/// Is a leg of the finished route clear of the chart, at the offing the
/// solve used? Without a grid (open ocean, the analytic cases) it is.
fn leg_clear(input: &IsochroneInput<'_>, a: [f64; 2], b: [f64; 2]) -> bool {
    let Some(grid) = input.grid else { return true };
    let (lat, _) = Projection::to_wgs84(a[0], a[1]);
    let k = 1.0 / lat.to_radians().cos();
    grid.segment_clear(a, b, input.offing_min_m * k)
}

fn point_segment_dist(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let ab = [b[0] - a[0], b[1] - a[1]];
    let len2 = ab[0] * ab[0] + ab[1] * ab[1];
    let t = if len2 <= f64::EPSILON {
        0.0
    } else {
        (((p[0] - a[0]) * ab[0] + (p[1] - a[1]) * ab[1]) / len2).clamp(0.0, 1.0)
    };
    dist2(p, [a[0] + ab[0] * t, a[1] + ab[1] * t]).sqrt()
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
        // It starts at the start and ends on the mark, and every leg is the
        // beam reach east with the north wind over the PORT side: wind from
        // 000 on a heading of 090 is a TWA of +90.
        let first = &r.points[0];
        let last = r.points.last().unwrap();
        assert!(crate::geo::distance_m(first.pos, START) < 1.0);
        assert!(crate::geo::distance_m(last.pos, finish) < 1.0, "the route stops short of the mark");
        assert_eq!(last.time_ms, r.arrival_ms);
        for pt in &r.points[1..] {
            assert!((pt.heading_deg - 90.0).abs() < 1.0, "heading {}", pt.heading_deg);
            assert!((pt.twa_deg - 90.0).abs() < 1.0, "twa {}", pt.twa_deg);
            assert_eq!(pt.tack, TackState::Port);
            assert!((pt.stw_kt - 6.0).abs() < 0.05, "stw {}", pt.stw_kt);
        }
    }

    /// §9.1 case 3: dead upwind. T = L / beat_vmg within 1 %. With no tack
    /// penalty the number of tacks is free (case 5 is the one-tack case), but
    /// every leg must be close-hauled, with the tack named for the side the
    /// wind is on.
    #[test]
    fn case_3_a_beat_takes_vmg_time() {
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
        let beat = p.vmg_at(10.0).beat_twa_deg as f64;
        for pt in &r.points[1..] {
            assert!(
                (pt.twa_deg.abs() - beat).abs() < 1.5,
                "a leg at TWA {} is not close-hauled ({beat}°)",
                pt.twa_deg
            );
            // Heading NE into a northerly puts the wind on the port bow.
            let expected = if pt.twa_deg > 0.0 { TackState::Port } else { TackState::Starboard };
            assert_eq!(pt.tack, expected, "heading {} twa {}", pt.heading_deg, pt.twa_deg);
        }
        assert!(r.points.iter().any(|p| p.tack == TackState::Port));
        assert!(r.points[1..].iter().any(|p| p.tack == TackState::Starboard));
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

    /// A tack is a tack and a gybe is a gybe: on a dead run only the gybe
    /// penalty may regularize the path. Charged to the wrong manoeuvre, a
    /// huge tack penalty would straighten the run and a huge gybe penalty
    /// would leave the sawtooth.
    #[test]
    fn a_run_pays_the_gybe_penalty_not_the_tack_penalty() {
        let finish = mark(START, 180.0, 8.0);
        let wind = || ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let mut tack_only = config();
        tack_only.tack_penalty_day_s = 100_000;
        tack_only.gybe_penalty_day_s = 0;
        let r = run(wind(), START, finish, &tack_only, 300).expect("a run");
        assert!(r.raw_tack_changes >= 3, "the tack penalty damped a run: {} gybes", r.raw_tack_changes);

        let mut gybe_only = config();
        gybe_only.tack_penalty_day_s = 0;
        gybe_only.gybe_penalty_day_s = 100_000;
        let r = run(wind(), START, finish, &gybe_only, 300).expect("a run");
        assert_eq!(r.raw_tack_changes, 1, "one gybe and no more");
        // Every leg is a run, broad on one gybe or the other.
        for pt in &r.points[1..] {
            assert!(pt.twa_deg.abs() > 150.0, "twa {}", pt.twa_deg);
        }
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

    /// The set of a current is where the water goes. A fair stream along
    /// the track adds its drift and a foul one takes it away: under engine
    /// at V, T = L/(V + C) and L/(V − C). A cross-stream (case 4) cannot
    /// tell set from its reciprocal; this can, and it pins the knots too.
    #[test]
    fn a_fair_stream_speeds_the_passage_and_a_foul_one_slows_it() {
        let l_nm = 10.0;
        let finish = mark(START, 0.0, l_nm);
        let mut cfg = config();
        cfg.motor_speed_kt = Some(5.0);
        let p = polar();
        let calm = ConstantWind { from_deg: 0.0, tws_kt: 0.0 };
        let hours_with = |set_deg: f64| {
            let current = ConstantCurrent { set_deg, drift_kt: 2.0 };
            let r = solve(&IsochroneInput {
                polar: &p,
                wind: &calm,
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
            .expect("motors along the stream");
            hours(&r)
        };
        // Setting north, the way the boat is going.
        let fair = hours_with(0.0);
        let expected = l_nm / 7.0;
        assert!((fair - expected).abs() / expected < 0.01, "fair: {fair:.3} h, expected {expected:.3} h");
        // Setting south, against her.
        let foul = hours_with(180.0);
        let expected = l_nm / 3.0;
        assert!((foul - expected).abs() / expected < 0.01, "foul: {foul:.3} h, expected {expected:.3} h");
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
        // And the transit of the gate itself happened inside the window:
        // the gate's water covers the whole passage, so the whole passage
        // lies between the gate opening and its shutting.
        assert!(route.arrival_ms > depart);
        assert!(
            route.arrival_ms <= 4 * 3_600_000,
            "arrived at {} min, after the gate shut",
            route.arrival_ms / 60_000
        );
    }

    /// A route may not be sailed through a gate after it shuts. Departing
    /// at 2 h 30 into a 2 h–4 h window, the 2 h passage would still be in the
    /// gate's water at 4 h 30.
    #[test]
    fn no_route_crosses_a_gate_after_it_shuts() {
        let l_nm = 12.0;
        let finish = mark(START, 90.0, l_nm);
        let gates = [TidalGate {
            pos: mark(START, 90.0, l_nm / 2.0),
            radius_nm: 40.0,
            windows: vec![(2 * 3_600_000, 4 * 3_600_000)],
        }];
        let p = polar();
        let cfg = config();
        let r = solve(&IsochroneInput {
            polar: &p,
            wind: &ConstantWind { from_deg: 0.0, tws_kt: 10.0 },
            grid: None,
            start: START,
            finish,
            depart_ms: 9_000_000,
            config: &cfg,
            dt_s: 600,
            offing_min_m: 0.0,
            waves: None,
            current: None,
            gates: &gates,
        });
        // Twelve miles in the ninety minutes left would need eight knots on
        // a beam reach in ten; the boat makes about six. Every route is in
        // the gate's water when it shuts, so there is no route.
        if let Ok(r) = r {
            panic!(
                "arrived at {} min through a gate shut since 240 min",
                r.arrival_ms / 60_000
            );
        }
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
        // The island lies across the rhumb line, so the open-water route
        // would sail straight through it: going round must cost time.
        assert!(r.arrival_ms > open.arrival_ms, "a detour cannot be free");
        // And no LEG of the route crosses the island — checking the points
        // alone would pass a straight line from start to finish.
        assert!(r.points.len() >= 3, "a route round an island has a corner");
        for w in r.points.windows(2) {
            let (a, b) = (merc(w[0].pos), merc(w[1].pos));
            for i in 0..=200 {
                let t = i as f64 / 200.0;
                let m = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
                assert!(
                    !(m[0] > ta[0] && m[0] < tb[0] && m[1] > ta[1] && m[1] < tc[1]),
                    "a leg crosses the island"
                );
            }
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

    /// The start has no tack yet, so leaving it on port is not a tack. The
    /// stepped search already exempts the start; a hop shorter than one
    /// step is closed from the start node directly and must be exempt too.
    #[test]
    fn a_short_hop_on_port_pays_no_phantom_manoeuvre() {
        let finish = mark(START, 90.0, 0.5);
        let r = run(ConstantWind { from_deg: 0.0, tws_kt: 10.0 }, START, finish, &config(), 600)
            .expect("half a mile on a beam reach");
        let expected = 0.5 / 6.0;
        assert!((hours(&r) - expected).abs() / expected < 0.01, "{} h", hours(&r));
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

    /// The limits hold along every leg, not only at the nodes. A band of
    /// heavy sea three miles wide is narrower than one hour's step, and lies
    /// inside the closing horizon of the nodes south of it: unless both the
    /// stepped legs and the analytic closing legs are walked, one of them
    /// jumps it. (Three miles, because the walk samples every two: a forecast
    /// on a quarter-degree grid has no narrower features to miss.)
    #[test]
    fn a_band_of_heavy_sea_narrower_than_a_step_cannot_be_jumped() {
        struct BandWaves {
            lat_from: f64,
            lat_to: f64,
            hs_m: f64,
        }
        impl WaveField for BandWaves {
            fn wave_height_m(&self, pos: [f64; 2], _t: i64) -> Option<f64> {
                let (lat, _) = Projection::to_wgs84(pos[0], pos[1]);
                Some(if lat >= self.lat_from && lat <= self.lat_to { self.hs_m } else { 0.0 })
            }
        }
        let p = polar();
        let cfg = config();
        let finish = mark(START, 0.0, 24.0);
        // From 18 to 21 miles north: inside an hour's six and a half miles
        // and the twelve-mile closing horizon.
        let band = |hs_m| BandWaves {
            lat_from: START.lat + 18.0 / 60.0,
            lat_to: START.lat + 21.0 / 60.0,
            hs_m,
        };
        let wall = band(cfg.max_wave_m + 2.0);
        let input = IsochroneInput {
            polar: &p,
            wind: &ConstantWind { from_deg: 270.0, tws_kt: 12.0 },
            grid: None,
            start: START,
            finish,
            depart_ms: 0,
            config: &cfg,
            dt_s: 3600,
            offing_min_m: 0.0,
            waves: Some(&wall),
            current: None,
            gates: &[],
        };
        assert!(solve(&input).is_err(), "a step or a closing leg jumped the band");
        let rough = band(cfg.max_wave_m - 1.0);
        assert!(solve(&IsochroneInput { waves: Some(&rough), ..input }).is_ok());

        // The same band as a gale, over the wind limit: just as much a wall.
        struct GaleBand {
            lat_from: f64,
            lat_to: f64,
        }
        impl WindField for GaleBand {
            fn wind(&self, pos: [f64; 2], _t: i64) -> (Bam, f64) {
                let (lat, _) = Projection::to_wgs84(pos[0], pos[1]);
                let gale = lat >= self.lat_from && lat <= self.lat_to;
                (deg_to_bam(270.0), if gale { 45.0 } else { 12.0 })
            }
        }
        let gale = GaleBand { lat_from: START.lat + 18.0 / 60.0, lat_to: START.lat + 21.0 / 60.0 };
        assert!(cfg.max_tws_kt < 45.0);
        assert!(
            solve(&IsochroneInput { wind: &gale, waves: None, ..input }).is_err(),
            "a step or a closing leg sailed through the gale"
        );
    }

    /// A gale that arrives while the boat is on a leg stops that leg, even
    /// though the leg began in fair weather. At a one-hour step the closing
    /// horizon is 12 NM, so the start can reach the mark in a single
    /// two-hour leg; judged only at its start, that leg sailed on into the
    /// gale that fills the whole sea from the first hour.
    #[test]
    fn a_gale_that_arrives_mid_leg_stops_the_leg() {
        struct GaleFromOneHour;
        impl WindField for GaleFromOneHour {
            fn wind(&self, _pos: [f64; 2], t: i64) -> (Bam, f64) {
                (deg_to_bam(0.0), if t >= 3_600_000 { 45.0 } else { 10.0 })
            }
        }
        let cfg = config();
        assert!(cfg.max_tws_kt < 45.0);
        let p = polar();
        let input = IsochroneInput {
            polar: &p,
            wind: &GaleFromOneHour,
            grid: None,
            start: START,
            finish: mark(START, 90.0, 12.0),
            depart_ms: 0,
            config: &cfg,
            dt_s: 3600,
            offing_min_m: 0.0,
            waves: None,
            current: None,
            gates: &[],
        };
        if let Ok(r) = solve(&input) {
            panic!("sailed into the gale, arriving at {} min", r.arrival_ms / 60_000);
        }
        // The same passage with the gale held off until after it is over.
        struct GaleFromSixHours;
        impl WindField for GaleFromSixHours {
            fn wind(&self, _pos: [f64; 2], t: i64) -> (Bam, f64) {
                (deg_to_bam(0.0), if t >= 6 * 3_600_000 { 45.0 } else { 10.0 })
            }
        }
        assert!(solve(&IsochroneInput { wind: &GaleFromSixHours, ..input }).is_ok());
    }

    /// A short dead run: one gybe costs more than running dead downwind
    /// saves, so the route runs dead downwind — and that only holds if the
    /// analytic closing charges its gybe as a gybe.
    #[test]
    fn a_short_run_does_not_gybe_when_the_gybe_costs_more_than_it_saves() {
        let finish = mark(START, 180.0, 2.0);
        let mut cfg = config();
        cfg.tack_penalty_day_s = 0;
        cfg.gybe_penalty_day_s = 300; // the most Π₅ allows at a 10-minute step
        let r = run(ConstantWind { from_deg: 0.0, tws_kt: 10.0 }, START, finish, &cfg, 600)
            .expect("a run");
        // Dead downwind the polar gives 4.4 kn; gybing at 163° would make
        // 4.64 kn good, but not by the five minutes the gybe costs.
        let expected = 2.0 / polar().speed_kt(180.0, 10.0).unwrap();
        assert!((hours(&r) - expected).abs() / expected < 0.01, "{} h, expected {expected} h", hours(&r));
    }

    #[test]
    fn the_wave_factor_is_neutral_without_data_or_coefficient() {
        assert_eq!(wave_speed_factor(None, 0.03), 1.0);
        assert_eq!(wave_speed_factor(Some(2.0), 0.0), 1.0);
        let f = wave_speed_factor(Some(2.0), 0.03);
        assert!((f - 1.0 / 1.12).abs() < 1e-12);
    }

    // ---------------------------------------------------------------------
    // Fixtures for the tests below.
    // ---------------------------------------------------------------------

    /// sec φ at a Mercator point: plane metres per ground metre there.
    fn k_of(p: [f64; 2]) -> f64 {
        let (lat, _) = Projection::to_wgs84(p[0], p[1]);
        1.0 / lat.to_radians().cos()
    }

    /// Knots as plane metres per second at scale `k`.
    fn kt_pl(kt: f64, k: f64) -> f64 {
        kt * METRES_PER_NM / 3600.0 * k
    }

    /// Open water from START with nothing but a wind.
    fn open_water<'a>(
        p: &'a Polar,
        wind: &'a dyn WindField,
        cfg: &'a RoutingConfig,
        finish: LatLon,
        dt_s: u32,
    ) -> IsochroneInput<'a> {
        IsochroneInput {
            polar: p,
            wind,
            grid: None,
            start: START,
            finish,
            depart_ms: 0,
            config: cfg,
            dt_s,
            offing_min_m: 0.0,
            current: None,
            waves: None,
            gates: &[],
        }
    }

    /// A chain of nodes through `points`, each the child of the one before,
    /// ten minutes apart, all on one tack.
    fn chain_of(points: &[[f64; 2]], tack: TackState) -> Vec<Node> {
        points
            .iter()
            .enumerate()
            .map(|(i, &pos)| Node {
                pos,
                time_ms: i as i64 * 600_000,
                tack,
                parent: if i == 0 { u32::MAX } else { i as u32 - 1 },
                heading: 0,
                twa_deg: if tack == TackState::Port { 90.0 } else { -90.0 },
                tws_kt: 10.0,
                stw_kt: 6.0,
            })
            .collect()
    }

    /// Douglas–Peucker over a whole chain; the kept interior indices, sorted.
    fn dp(nodes: &[Node], tol: f64) -> Vec<usize> {
        let chain: Vec<&Node> = nodes.iter().collect();
        let mut keep = Vec::new();
        douglas_peucker(&chain, 0, chain.len() - 1, tol, &mut keep);
        keep.sort_unstable();
        keep
    }

    /// Rotate `p` by `deg` about the origin and shift it by `off` — the same
    /// rigid motion applied to a whole figure keeps every distance in it, and
    /// takes it off the axes where a swapped x and y would hide.
    fn moved(p: [f64; 2], deg: f64, off: [f64; 2]) -> [f64; 2] {
        let (s, c) = deg.to_radians().sin_cos();
        [p[0] * c - p[1] * s + off[0], p[0] * s + p[1] * c + off[1]]
    }

    // ---------------------------------------------------------------------
    // Small pieces, worked by hand.
    // ---------------------------------------------------------------------

    /// A 3-4-5 leg, a = (100, 200) to b = (130, 240): length 50, along
    /// (0.6, 0.8), with (−0.8, 0.6) square to it. A circle of radius 25 whose
    /// centre sits 7 off the leg cuts a chord of 2·√(25² − 7²) = 48 — 0.48 of
    /// the leg each side of the centre's foot.
    #[test]
    fn circle_span_is_the_chord_a_leg_cuts_through_the_circle() {
        let (a, b) = ([100.0, 200.0], [130.0, 240.0]);
        let at = |f: f64, off: f64| [100.0 + 30.0 * f - 0.8 * off, 200.0 + 40.0 * f + 0.6 * off];
        let close = |got: Option<(f64, f64)>, want: (f64, f64)| {
            let (f1, f2) = got.expect("the leg meets the circle");
            assert!((f1 - want.0).abs() < 1e-9 && (f2 - want.1).abs() < 1e-9, "({f1}, {f2}) vs {want:?}");
        };
        // Through the middle: in at 0.02, out at 0.98.
        close(circle_span(a, b, at(0.5, 7.0), 25.0), (0.02, 0.98));
        // Foot at 0.3: the leg starts inside, so it is in from 0 to 0.78 …
        close(circle_span(a, b, at(0.3, 7.0), 25.0), (0.0, 0.78));
        // … and sailed the other way, from 0.22 to the end.
        close(circle_span(b, a, at(0.3, 7.0), 25.0), (0.22, 1.0));
        // The far side of the circle, from the other side of the leg.
        close(circle_span(a, b, at(0.5, -7.0), 25.0), (0.02, 0.98));
        // Wholly ahead of the leg, wholly behind it, and wide of it: no.
        assert_eq!(circle_span(a, b, at(1.6, 7.0), 25.0), None);
        assert_eq!(circle_span(a, b, at(-0.6, 7.0), 25.0), None);
        assert_eq!(circle_span(a, b, at(0.5, 30.0), 25.0), None);
    }

    /// The circle is closed: a leg that touches it touches the gate's water.
    /// Every number here is exact in binary, so the edges are hit exactly.
    #[test]
    fn circle_span_counts_a_touch_on_the_edge_as_inside() {
        let (a, b) = ([0.0, 0.0], [10.0, 0.0]);
        // Grazing the top of a circle at the leg's midpoint.
        assert_eq!(circle_span(a, b, [5.0, 2.0], 2.0), Some((0.5, 0.5)));
        // A circle the leg leaves exactly as it starts…
        assert_eq!(circle_span(a, b, [-2.0, 0.0], 2.0), Some((0.0, 0.0)));
        // …and one it reaches exactly as it ends.
        assert_eq!(circle_span(a, b, [12.0, 0.0], 2.0), Some((1.0, 1.0)));
        // A leg that goes nowhere is in or out, edge counting as in.
        let p = [3.0, 4.0];
        assert_eq!(circle_span(p, p, [3.0, 5.0], 1.0), Some((0.0, 0.0)));
        assert_eq!(circle_span(p, p, [3.0, 5.0], 2.0), Some((0.0, 0.0)));
        assert_eq!(circle_span(p, p, [3.0, 9.0], 2.0), None);
    }

    /// Distance from a point to a segment, on the same 3-4-5 leg: square off
    /// the middle it is the offset; past either end it is the distance to
    /// that end; a segment of no length is its point.
    #[test]
    fn point_segment_distance_is_to_the_nearest_point_of_the_segment() {
        let (a, b) = ([100.0, 200.0], [130.0, 240.0]);
        let at = |f: f64, off: f64| [100.0 + 30.0 * f - 0.8 * off, 200.0 + 40.0 * f + 0.6 * off];
        let near = |got: f64, want: f64| assert!((got - want).abs() < 1e-9, "{got} vs {want}");
        near(point_segment_dist(at(0.25, 7.0), a, b), 7.0);
        near(point_segment_dist(at(0.75, -3.0), a, b), 3.0);
        // 10 beyond b along the leg and 5 off it: √125 from b.
        near(point_segment_dist(at(1.2, 5.0), a, b), 125f64.sqrt());
        near(point_segment_dist(at(-0.2, -5.0), a, b), 125f64.sqrt());
        near(point_segment_dist([103.0, 204.0], a, a), 5.0);
    }

    /// Douglas–Peucker keeps a vertex only when it lies further than the
    /// tolerance from the chord it would be dropped onto — the standard
    /// `dmax > ε` — and recurses on both halves.
    #[test]
    fn douglas_peucker_keeps_exactly_what_strays_past_the_tolerance() {
        let m = |p: [f64; 2]| moved(p, 30.0, [1000.0, 2000.0]);
        // A spike 3 off the middle of a 50-long chord.
        let spike = chain_of(&[m([0.0, 0.0]), m([25.0, 3.0]), m([50.0, 0.0])], TackState::Starboard);
        assert_eq!(dp(&spike, 2.0), vec![1]);
        assert_eq!(dp(&spike, 4.0), Vec::<usize>::new());
        // A wiggle 1 off, far from both ends: dropped at a tolerance of 2.
        let wiggle =
            chain_of(&[m([0.0, 0.0]), m([20.0, 1.0]), m([30.0, -1.0]), m([50.0, 0.0])], TackState::Starboard);
        assert_eq!(dp(&wiggle, 2.0), Vec::<usize>::new());
        // A vertex beyond the chord's end is measured to that end, not to
        // the line: 1 off the line but 5.1 from the end.
        let past = chain_of(&[m([0.0, 0.0]), m([15.0, 1.0]), m([10.0, 0.0])], TackState::Starboard);
        assert_eq!(dp(&past, 2.0), vec![1]);
        // Two bumps: 5 at x = 10 goes first; on the chord from it to the end,
        // the bump at x = 30 is 4.6 off and stays, the one at x = 20 then
        // lies 0.46 off its own chord and goes.
        let bumps = chain_of(
            &[m([0.0, 0.0]), m([10.0, 5.0]), m([20.0, 0.5]), m([30.0, -3.0]), m([40.0, 0.0])],
            TackState::Starboard,
        );
        assert_eq!(dp(&bumps, 2.0), vec![1, 3]);
        // A loop back to the start: the chord has no length, so the distance
        // is to the point itself — 5 away.
        let o = [1000.0, 2000.0];
        let lp = chain_of(&[o, [1003.0, 2004.0], o], TackState::Starboard);
        assert_eq!(dp(&lp, 2.0), vec![1]);
        assert_eq!(dp(&lp, 6.0), Vec::<usize>::new());
        // Exactly on the tolerance is within it (axis-aligned, so exact).
        let edge = chain_of(&[[1000.0, 2000.0], [1005.0, 2003.0], [1010.0, 2000.0]], TackState::Starboard);
        assert_eq!(dp(&edge, 3.0), Vec::<usize>::new());
        assert_eq!(dp(&edge, 2.999), vec![1]);
    }

    /// The plan's point of sail: each band from its named angle up to the
    /// next — close-hauled below 60°, close reach to 80°, beam reach to 110°,
    /// broad reach to 150°, a run beyond. The edges are pinned so moving one
    /// is a decision, not an accident.
    #[test]
    fn points_of_sail_are_bands_of_true_wind_angle() {
        use PointOfSail::*;
        for (twa, want) in [
            (0.0, CloseHauled),
            (45.0, CloseHauled),
            (59.9, CloseHauled),
            (60.0, CloseReach),
            (70.0, CloseReach),
            (79.9, CloseReach),
            (80.0, BeamReach),
            (90.0, BeamReach),
            (109.9, BeamReach),
            (110.0, BroadReach),
            (135.0, BroadReach),
            (149.9, BroadReach),
            (150.0, Run),
            (180.0, Run),
        ] {
            assert_eq!(point_of_sail(twa), want, "TWA {twa}");
        }
    }

    /// §2.10: the sea-state factor lives in (0, 1]. A flat sea costs
    /// nothing whatever the coefficient, a zero coefficient switches the
    /// penalty off whatever the sea, and a negative coefficient — a typo in
    /// the settings — must not make waves speed the boat up.
    #[test]
    fn the_wave_factor_never_speeds_the_boat_and_flat_water_is_free() {
        assert_eq!(wave_speed_factor(Some(0.0), f64::INFINITY), 1.0);
        assert_eq!(wave_speed_factor(Some(f64::INFINITY), 0.0), 1.0);
        assert_eq!(wave_speed_factor(Some(2.0), -0.03), 1.0);
        for hs in [0.0, 0.5, 2.0, 6.0] {
            for coef in [-0.5, 0.0, 0.03, 1.0] {
                let f = wave_speed_factor(Some(hs), coef);
                assert!(f > 0.0 && f <= 1.0, "Hs {hs} c {coef}: {f}");
            }
        }
    }

    #[test]
    fn every_routing_failure_says_what_went_wrong() {
        let msgs: Vec<String> = [IsoError::Unreachable, IsoError::TookTooLong, IsoError::Cancelled]
            .iter()
            .map(|e| e.to_string())
            .collect();
        assert!(msgs.iter().all(|m| !m.is_empty()), "{msgs:?}");
        assert!(msgs[0] != msgs[1] && msgs[1] != msgs[2] && msgs[0] != msgs[2]);
        assert!(msgs[0].contains("no sailable path"), "{}", msgs[0]);
        assert!(msgs[1].contains("step limit"), "{}", msgs[1]);
        assert!(msgs[2].contains("cancelled"), "{}", msgs[2]);
    }

    /// Cancel abandons the plans begun before it — and only those: a thread
    /// that never began a cancellable plan (the CLI, the tests) carries on.
    #[test]
    fn cancelling_stops_the_plans_begun_before_it_and_no_others() {
        let hop = || {
            run(ConstantWind { from_deg: 0.0, tws_kt: 10.0 }, START, mark(START, 90.0, 0.5), &config(), 600)
        };
        std::thread::spawn(move || {
            // Two bumps first, so the generation is no constant a stub
            // could return.
            cancel_plans();
            cancel_plans();
            let g = begin_cancellable();
            assert!(!plan_cancelled(g), "a fresh plan is not cancelled");
            assert!(!this_plan_cancelled());
            cancel_plans();
            assert!(plan_cancelled(g), "cancel did not reach the plan");
            assert!(this_plan_cancelled());
            assert!(matches!(hop(), Err(IsoError::Cancelled)), "the solve ran on regardless");
            // A plan begun after the cancel runs.
            let g2 = begin_cancellable();
            assert!(!plan_cancelled(g2));
            assert!(hop().is_ok());
        })
        .join()
        .unwrap();
        std::thread::spawn(move || {
            cancel_plans();
            assert!(!this_plan_cancelled(), "a thread with no plan was cancelled");
            assert!(hop().is_ok());
        })
        .join()
        .unwrap();
    }

    /// Wind recorded at every place and time it is asked for.
    struct RecordingWind {
        asked: std::cell::RefCell<Vec<([f64; 2], i64)>>,
    }

    impl WindField for RecordingWind {
        fn wind(&self, pos: [f64; 2], t: i64) -> (Bam, f64) {
            self.asked.borrow_mut().push((pos, t));
            (deg_to_bam(0.0), 10.0)
        }
    }

    /// The limits are walked along a leg every two miles of ground or less,
    /// ends included, each sample where and when the boat is there: a 9 nm
    /// leg is five spans, six samples, evenly spread in space and time; a
    /// leg of 100 nm is capped at 32 spans; a leg of no length is still
    /// looked at, at both ends.
    #[test]
    fn the_limits_are_walked_every_two_miles_in_space_and_time() {
        let p = polar();
        let cfg = config();
        let a = merc(START);
        let k = k_of(a);
        let (t_a, t_b) = (1_000_000i64, 4_600_000i64);
        let walk = |len_nm: f64| {
            let rec = RecordingWind { asked: Default::default() };
            let input = open_water(&p, &rec, &cfg, START, 600);
            let d = len_nm * METRES_PER_NM * k;
            let b = [a[0] + 0.6 * d, a[1] + 0.8 * d];
            assert!(conditions_ok_along(&input, a, b, t_a, t_b, k));
            (b, rec.asked.into_inner())
        };
        let (b, asked) = walk(9.0);
        assert_eq!(asked.len(), 6, "a 9 nm leg is six samples");
        for (i, (pos, t)) in asked.iter().enumerate() {
            let f = i as f64 / 5.0;
            let want = [a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f];
            assert!((pos[0] - want[0]).abs() < 1e-6 && (pos[1] - want[1]).abs() < 1e-6, "sample {i} at {pos:?}");
            let want_t = t_a as f64 + (t_b - t_a) as f64 * f;
            assert!((*t as f64 - want_t).abs() <= 1.0, "sample {i} at {t} ms, not {want_t}");
        }
        assert_eq!(walk(100.0).1.len(), 33, "the walk is capped at 32 spans");
        assert_eq!(walk(0.0).1.len(), 2, "a leg of no length is still checked");
    }

    /// §6.1: TWS ≤ max_tws and wave ≤ max_wave are allowed — the limit
    /// itself is sailable, only beyond it is a wall.
    #[test]
    fn weather_exactly_at_the_limits_is_still_sailable() {
        let p = polar();
        let cfg = config();
        let finish = mark(START, 90.0, 6.0);
        let at_limit = ConstantWind { from_deg: 0.0, tws_kt: cfg.max_tws_kt };
        let input = open_water(&p, &at_limit, &cfg, finish, 600);
        assert!(conditions_ok(&input, merc(START), 0));
        assert!(solve(&input).is_ok(), "wind at exactly max_tws_kt is not a wall");

        let calm_ten = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let sea = ConstantWaves { hs_m: cfg.max_wave_m };
        let input = IsochroneInput { waves: Some(&sea), ..open_water(&p, &calm_ten, &cfg, finish, 600) };
        assert!(conditions_ok(&input, merc(START), 0));
        let r = solve(&input).expect("a sea of exactly max_wave_m is not a wall");
        // And it is the penalised beam reach: 6 kn × 1/(1 + c·Hs²).
        let expected_h = 6.0 / (6.0 / (1.0 + cfg.wave_penalty_coef * cfg.max_wave_m.powi(2)));
        assert!((hours(&r) - expected_h).abs() / expected_h < 0.01, "{} h vs {expected_h} h", hours(&r));
    }

    /// A gate shut while the boat is in its water blocks the leg; open for
    /// the whole passage through it, it does not. The circle is the gate's
    /// radius in *ground* miles: a leg 0.7 nm off its centre is inside a
    /// one-mile gate and one 1.3 nm off is not.
    #[test]
    fn a_gate_blocks_exactly_the_legs_inside_its_circle_while_it_is_shut() {
        let c = merc(START);
        let k = k_of(c);
        let nm = METRES_PER_NM * k;
        // West to east through the centre, 6 nm in 6 h: in the circle from
        // 2 h to 4 h.
        let (a, b) = ([c[0] - 3.0 * nm, c[1]], [c[0] + 3.0 * nm, c[1]]);
        let h = 3_600_000i64;
        let gate = |windows: Vec<(i64, i64)>| TidalGate { pos: START, radius_nm: 1.0, windows };
        let blocks = |g: TidalGate, a: [f64; 2], b: [f64; 2]| gate_blocks(&[g], a, b, 0, 6 * h, k);
        assert!(!blocks(gate(vec![(2 * h - 60_000, 4 * h + 60_000)]), a, b), "open throughout");
        assert!(blocks(gate(vec![(2 * h + 600_000, 5 * h)]), a, b), "entered before it opened");
        assert!(blocks(gate(vec![(h, 4 * h - 600_000)]), a, b), "still inside when it shut");
        // Two windows that together cover the passage but neither alone
        // does: the boat is in the gate when it shuts between them.
        assert!(blocks(gate(vec![(h, 3 * h), (3 * h + 1, 5 * h)]), a, b));
        // Always shut: inside the radius blocks, outside it does not.
        let off = |n: f64| ([a[0], a[1] + n * nm], [b[0], b[1] + n * nm]);
        let (a7, b7) = off(0.7);
        assert!(blocks(gate(vec![]), a7, b7), "0.7 nm off is inside a 1 nm gate");
        let (a13, b13) = off(-1.3);
        assert!(!blocks(gate(vec![]), a13, b13), "1.3 nm off is outside it");
    }

    // ---------------------------------------------------------------------
    // The search itself.
    // ---------------------------------------------------------------------

    /// The candidates a node offers, counted on a hop the start closes
    /// inside one ring (so exactly one node is propagated): the fan of
    /// 2·⌊100/5⌋ + 1 = 41 headings round the mark, the 4 VMG headings, and
    /// the coarse ring behind the fan every 20° from 120° to 240° off the
    /// mark — 7 more. With an engine fitted and the sail toward the mark
    /// *below* the threshold (§2.11: strictly below), the whole fan again
    /// under engine: 41 more.
    #[test]
    fn a_node_offers_the_fan_the_vmg_headings_and_the_ring_behind() {
        let p = polar();
        let hop = mark(START, 90.0, 0.4);
        let count = |cfg: &RoutingConfig, tws: f64, waves: Option<&dyn WaveField>| {
            let wind = ConstantWind { from_deg: 0.0, tws_kt: tws };
            let r = solve(&IsochroneInput { waves, ..open_water(&p, &wind, cfg, hop, 600) })
                .expect("a short hop");
            assert!(r.arrival_ms <= 600_000, "the hop must close inside the first ring");
            r.propagations
        };
        let sail_only = 41 + 4 + 7;
        assert_eq!(count(&config(), 10.0, None), sail_only);

        // 6 kn of wind: the beam reach toward the mark makes 4.5 kn.
        let mut motor = config();
        motor.motor_speed_kt = Some(5.0);
        motor.motor_threshold_kt = 4.5;
        assert_eq!(count(&motor, 6.0, None), sail_only, "4.5 kn is not below a 4.5 kn threshold");
        motor.motor_threshold_kt = 4.6;
        assert_eq!(count(&motor, 6.0, None), sail_only + 41);

        // The threshold judges the sail *after* the sea has slowed it: 6 kn
        // halved by a 2 m sea at c = 0.25 is 3 kn, under a 4 kn threshold.
        let mut rough = motor.clone();
        rough.motor_threshold_kt = 4.0;
        rough.wave_penalty_coef = 0.25;
        let sea = ConstantWaves { hs_m: 2.0 };
        assert_eq!(count(&rough, 10.0, Some(&sea)), sail_only + 41);
        assert_eq!(count(&rough, 10.0, None), sail_only, "6 kn of sail is not below 4 kn");
    }

    /// The wind falls light the moment the boat leaves.
    struct FallsLightAfterDeparture;

    impl WindField for FallsLightAfterDeparture {
        fn wind(&self, _pos: [f64; 2], t: i64) -> (Bam, f64) {
            (deg_to_bam(0.0), if t <= 0 { 10.0 } else { 6.0 })
        }
    }

    /// The analytic closing reaches a step and a half at 8 kn, never less
    /// than 3 nm — and no further. Closing legs sail on the weather of the
    /// node they leave from, so a wind that falls light after departure
    /// shows where the horizon is: within it the start closes at the
    /// departure's 6.0 kn beam reach; beyond it the boat must step on into
    /// the light air (4.5 kn).
    #[test]
    fn the_closing_horizon_is_a_step_and_a_half_at_eight_knots_or_three_miles() {
        let p = polar();
        let cfg = config();
        let arrive = |nm: f64, dt_s: u32| {
            let r = solve(&open_water(&p, &FallsLightAfterDeparture, &cfg, mark(START, 90.0, nm), dt_s))
                .expect("a beam reach");
            hours(&r)
        };
        let near = |got: f64, want: f64, what: &str| {
            assert!((got - want).abs() / want < 0.01, "{what}: {got:.4} h, expected {want:.4} h");
        };
        // Five-minute steps: 1.5 × 8 kn × 5 min is 1 nm, so 3 nm rules.
        near(arrive(2.5, 300), 2.5 / 6.0, "2.5 nm at a 5-min step is inside 3 nm");
        // One ring of 5 min at 6 kn, then the rest at 4.5 kn.
        near(arrive(3.5, 300), 300.0 / 3600.0 + 3.0 / 4.5, "3.5 nm at a 5-min step is outside");
        // One-hour steps: 1.5 × 8 kn × 1 h = 12 nm.
        near(arrive(11.5, 3600), 11.5 / 6.0, "11.5 nm at a 1-h step is inside 12 nm");
        near(arrive(12.5, 3600), 1.0 + 6.5 / 4.5, "12.5 nm at a 1-h step is outside");
    }

    /// Π₅ (and LuckGrib's practice, §2.5): a tack penalty is capped at half
    /// the time step. A dead beat of 2 nm closes from the start with one
    /// tack, so it takes L / VMG plus exactly one capped penalty — 300 s at
    /// a 10-minute step, however large the configured penalty.
    #[test]
    fn a_huge_tack_penalty_is_capped_at_half_a_step() {
        let p = polar();
        let mut cfg = config();
        cfg.tack_penalty_day_s = 100_000;
        let vmg = p.vmg_at(10.0).beat_vmg_kt as f64;
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let r = solve(&open_water(&p, &wind, &cfg, mark(START, 0.0, 2.0), 600)).expect("a beat");
        let expected_s = 2.0 / vmg * 3600.0 + 300.0;
        let got_s = r.arrival_ms as f64 / 1000.0;
        assert!((got_s - expected_s).abs() < 10.0, "{got_s:.0} s, expected {expected_s:.0} s");
        assert_eq!(r.raw_tack_changes, 1);
    }

    /// No current, but a declared maximum: what the Π₂ guard reads.
    struct DeclaredMax(f64);

    impl CurrentField for DeclaredMax {
        fn current(&self, _pos: [f64; 2], _t: i64) -> (f64, f64) {
            (0.0, 0.0)
        }
        fn max_speed_ms(&self) -> f64 {
            self.0
        }
    }

    /// Π₂ = C/V trips when the current is *faster* than the boat. The test
    /// polar's best speed is 7.5 kn (beam reach, 20 kn and up); 7 kn of
    /// stream is slower, a stream of exactly 7.5 kn is not faster, and
    /// anything beyond is.
    #[test]
    fn the_current_guard_trips_only_when_the_stream_outruns_the_boat() {
        let p = polar();
        let cfg = config();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let finish = mark(START, 90.0, 2.0);
        let fallback = |current: &dyn CurrentField| {
            solve(&IsochroneInput { current: Some(current), ..open_water(&p, &wind, &cfg, finish, 600) })
                .expect("open water")
                .pruning_fallback
        };
        // A fair 7 kn stream: C/V = 0.93.
        assert!(!fallback(&ConstantCurrent { set_deg: 90.0, drift_kt: 7.0 }), "7 kn is slower than 7.5 kn");
        assert!(fallback(&ConstantCurrent { set_deg: 90.0, drift_kt: 8.0 }));
        // Exactly 7.5 kn: the m/s value whose knots are 7.5 to the bit.
        let mut ms = 7.5 / MS_TO_KNOTS_INV;
        for _ in 0..4 {
            if ms * MS_TO_KNOTS_INV == 7.5 {
                break;
            }
            ms = f64::from_bits(if ms * MS_TO_KNOTS_INV < 7.5 { ms.to_bits() + 1 } else { ms.to_bits() - 1 });
        }
        assert_eq!(ms * MS_TO_KNOTS_INV, 7.5, "fixture: no m/s value is exactly 7.5 kn");
        assert!(!fallback(&DeclaredMax(ms)), "a stream as fast as the boat is not faster");
        assert!(fallback(&DeclaredMax(f64::from_bits(ms.to_bits() + 2))));
    }

    /// A stream exactly as fast as the engine, setting south.
    struct HoldsHerInPlace;

    impl CurrentField for HoldsHerInPlace {
        fn current(&self, _pos: [f64; 2], _t: i64) -> (f64, f64) {
            (0.0, -2.0)
        }
        fn max_speed_ms(&self) -> f64 {
            2.0
        }
    }

    /// A frontier that gets no nearer gives up after 40 rings — and "no
    /// nearer" includes *exactly as near*. On the equator (where the plane
    /// and the ground agree, k = 1) a boat motoring north at 2 m/s into a
    /// 2 m/s stream setting south stays exactly where she is, to the bit;
    /// every other heading loses ground. That is a stall, not progress.
    #[test]
    fn a_frontier_held_exactly_in_place_is_a_stall() {
        let p = polar();
        let start = LatLon { lat: 0.0, lon: 11.5 };
        let finish = LatLon { lat: 0.1, lon: 11.5 };
        let mut cfg = config();
        // Find the engine speed whose hour's run is 7200 m to the bit.
        let mut m = 7200.0 / METRES_PER_NM;
        for _ in 0..4 {
            if m * METRES_PER_NM == 7200.0 {
                break;
            }
            m = f64::from_bits(if m * METRES_PER_NM < 7200.0 { m.to_bits() + 1 } else { m.to_bits() - 1 });
        }
        assert_eq!(m * METRES_PER_NM, 7200.0, "fixture: no engine speed runs exactly 7200 m");
        cfg.motor_speed_kt = Some(m);
        cfg.max_diverted_deg = 30.0;
        cfg.heading_step_deg = 10.0;
        let calm = ConstantWind { from_deg: 0.0, tws_kt: 0.0 };
        let r = solve(&IsochroneInput {
            polar: &p,
            wind: &calm,
            grid: None,
            start,
            finish,
            depart_ms: 0,
            config: &cfg,
            dt_s: 3600,
            offing_min_m: 0.0,
            current: Some(&HoldsHerInPlace),
            waves: None,
            gates: &[],
        });
        assert!(matches!(r, Err(IsoError::Unreachable)), "held in place is not progress");
    }

    /// An 8 kn stream setting west for the first hour, slack after.
    struct SetBackForAnHour;

    impl CurrentField for SetBackForAnHour {
        fn current(&self, _pos: [f64; 2], t: i64) -> (f64, f64) {
            if t < 3_600_000 { (-8.0 / MS_TO_KNOTS_INV, 0.0) } else { (0.0, 0.0) }
        }
        fn max_speed_ms(&self) -> f64 {
            8.0 / MS_TO_KNOTS_INV
        }
    }

    /// Losing ground for a few rings is not a stall: 40 rings is. Every
    /// heading loses ground for the first hour (six rings); then the stream
    /// is slack and the passage goes on. Stemming it at 6 kn is the best
    /// she can do — 2 nm lost, then 8 nm at 6 kn — so no route arrives
    /// before 1 h + 8/6 h.
    #[test]
    fn being_set_back_for_a_few_rings_is_not_a_stall() {
        let p = polar();
        let cfg = config();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let r = solve(&IsochroneInput {
            current: Some(&SetBackForAnHour),
            ..open_water(&p, &wind, &cfg, mark(START, 90.0, 6.0), 600)
        })
        .expect("the stream turns; the passage goes on");
        let best = 1.0 + 8.0 / 6.0;
        assert!(hours(&r) >= best * 0.995, "{} h beats the physics ({best} h)", hours(&r));
    }

    /// The optimum of the case above: stem the stream for the hour, then
    /// sail the 8 nm left.
    #[test]
    fn being_set_back_the_route_stems_the_stream() {
        let p = polar();
        let cfg = config();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let r = solve(&IsochroneInput {
            current: Some(&SetBackForAnHour),
            ..open_water(&p, &wind, &cfg, mark(START, 90.0, 6.0), 600)
        })
        .expect("the stream turns; the passage goes on");
        let best = 1.0 + 8.0 / 6.0;
        assert!((hours(&r) - best).abs() / best < 0.03, "{} h, expected {best} h", hours(&r));
    }

    /// Next to no wind at departure, a breeze ten minutes later.
    struct CalmThenBreeze;

    impl WindField for CalmThenBreeze {
        fn wind(&self, _pos: [f64; 2], t: i64) -> (Bam, f64) {
            (deg_to_bam(0.0), if t <= 0 { 0.03 } else { 10.0 })
        }
    }

    /// 0.03 kn of wind drives the test boat at 0.02 kn — a calm, not a
    /// crawl. Becalmed at the start (the hop is beyond the closing horizon,
    /// and the engine does not wait for wind), there is no passage.
    #[test]
    fn a_boat_becalmed_at_the_start_goes_nowhere() {
        let p = polar();
        let cfg = config();
        assert!(p.speed_kt(90.0, 0.03).unwrap_or(0.0) < 0.05);
        let r = solve(&open_water(&p, &CalmThenBreeze, &cfg, mark(START, 90.0, 6.0), 600));
        assert!(matches!(r, Err(IsoError::Unreachable)), "crept out of a calm");
    }

    /// A tack is charged as a tack in the stepped search too: with the tack
    /// penalty huge and gybes free, a beat still makes one tack — gybe
    /// pricing would let it zigzag for nothing.
    #[test]
    fn a_beat_pays_the_tack_penalty_not_the_gybe_penalty() {
        let mut cfg = config();
        cfg.tack_penalty_day_s = 100_000;
        cfg.gybe_penalty_day_s = 0;
        let r = run(ConstantWind { from_deg: 0.0, tws_kt: 10.0 }, START, mark(START, 0.0, 8.0), &cfg, 600)
            .expect("a beat");
        assert_eq!(r.raw_tack_changes, 1, "one tack and no more");
    }

    /// A polar that sails well at exactly two angles — 45° (6 kn) and 160°
    /// (8 kn) — and badly either side of them, so only the exact VMG
    /// headings make good time and a fan five degrees coarse cannot stand
    /// in for them.
    const SPIKY_POLAR: &str = "\
twa/tws\t6\t20
45\t6\t6
50\t2\t2
150\t2\t2
160\t8\t8
165\t2\t2
180\t2\t2
";

    /// With a spiky polar and no penalties, a beat or a run takes exactly
    /// the VMG time — L·cos(off-axis) / VMG — but only if *both* VMG
    /// headings are offered: the mark sits 2.7° off the wind axis, so each
    /// course needs both tacks (or gybes), and neither single line passes
    /// within the 3 nm closing horizon of it.
    #[test]
    fn both_vmg_headings_are_offered_upwind_and_down() {
        let p = Polar::parse(SPIKY_POLAR).unwrap();
        let v = p.vmg_at(10.0);
        assert_eq!((v.beat_twa_deg, v.run_twa_deg), (45.0, 160.0), "fixture polar");
        let mut cfg = config();
        cfg.tack_penalty_day_s = 0;
        cfg.gybe_penalty_day_s = 0;
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let l_nm = 12.0;
        for (bearing, vmg, what) in [
            (357.3, 6.0 * 45f64.to_radians().cos(), "beat"),
            (2.7, 6.0 * 45f64.to_radians().cos(), "beat"),
            (182.7, 8.0 * 20f64.to_radians().cos(), "run"),
            (177.3, 8.0 * 20f64.to_radians().cos(), "run"),
        ] {
            let r = solve(&open_water(&p, &wind, &cfg, mark(START, bearing, l_nm), 300)).expect(what);
            let expected = l_nm * 2.7f64.to_radians().cos() / vmg;
            assert!(
                (hours(&r) - expected).abs() / expected < 0.01,
                "{what} to {bearing}°: {:.3} h, expected {expected:.3} h",
                hours(&r)
            );
        }
    }

    /// §5: the wave factor slows the engine too. Motoring in a calm at 5 kn
    /// through a 2 m sea at c = 0.25 is 2.5 kn: 10 nm in 4 h, stepped and
    /// closed alike.
    #[test]
    fn a_sea_slows_the_engine_as_much_as_the_sails() {
        let p = polar();
        let mut cfg = config();
        cfg.motor_speed_kt = Some(5.0);
        cfg.wave_penalty_coef = 0.25;
        let calm = ConstantWind { from_deg: 0.0, tws_kt: 0.0 };
        let sea = ConstantWaves { hs_m: 2.0 };
        let r = solve(&IsochroneInput { waves: Some(&sea), ..open_water(&p, &calm, &cfg, mark(START, 90.0, 10.0), 600) })
            .expect("motoring");
        assert!((hours(&r) - 4.0).abs() / 4.0 < 0.01, "{} h, expected 4 h", hours(&r));
    }

    /// The start has no tack, so leaving it on port is no manoeuvre — but a
    /// two-leg beat from the start is one tack.
    #[test]
    fn the_first_course_from_the_start_is_not_a_tack() {
        let wind = || ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let r = run(wind(), START, mark(START, 90.0, 0.5), &config(), 600).expect("a hop on port");
        assert_eq!(r.points.last().unwrap().tack, TackState::Port);
        assert_eq!(r.raw_tack_changes, 0, "leaving the start on port counted as a tack");
        // A dead beat needs one tack, and with tacks costing time it makes
        // no more than that one.
        let r = run(wind(), START, mark(START, 0.0, 2.0), &config(), 600).expect("a short beat");
        assert_eq!(r.raw_tack_changes, 1);
    }

    /// Wind that is fresh only for a few minutes: strong from 0.1 h to
    /// 0.15 h, light either side.
    struct BriefFreshening;

    impl WindField for BriefFreshening {
        fn wind(&self, _pos: [f64; 2], t: i64) -> (Bam, f64) {
            (deg_to_bam(0.0), if (360_000..540_000).contains(&t) { 20.0 } else { 6.0 })
        }
    }

    /// best_departure keeps the departure with the earliest *arrival* —
    /// whichever order the departures come in, and even when leaving later
    /// is what arrives first.
    #[test]
    fn the_best_departure_is_the_earliest_arrival_not_the_first_that_works() {
        let p = polar();
        let cfg = config();
        let input = open_water(&p, &BriefFreshening, &cfg, mark(START, 90.0, 2.5), 300);
        let departures = [0i64, 360_000, 3_600_000];
        let arrivals: Vec<i64> = departures
            .iter()
            .map(|&d| solve(&IsochroneInput { depart_ms: d, ..input }).expect("a beam reach").arrival_ms)
            .collect();
        let (best_i, _) = arrivals.iter().enumerate().min_by_key(|(_, a)| **a).unwrap();
        assert_eq!(best_i, 1, "fixture: leaving at 0.1 h must arrive first ({arrivals:?})");
        let (depart, route) = best_departure(&input, &departures).expect("all three sail");
        assert_eq!(depart, departures[best_i]);
        assert_eq!(route.arrival_ms, arrivals[best_i]);
        // The order offered does not matter.
        let reversed = [3_600_000i64, 360_000, 0];
        assert_eq!(best_departure(&input, &reversed).unwrap().0, 360_000);
    }


    // ---------------------------------------------------------------------
    // The analytic closing legs, called directly.
    // ---------------------------------------------------------------------

    /// The closing from `pos` at `time_ms`, the wind given in degrees and
    /// the tack and gybe penalties as a pair.
    #[allow(clippy::too_many_arguments)]
    fn close(
        input: &IsochroneInput<'_>,
        pos: [f64; 2],
        time_ms: i64,
        tack_now: Option<TackState>,
        finish_m: [f64; 2],
        k: f64,
        (wind_from_deg, tws): (f64, f64),
        current_ms: (f64, f64),
        wave_f: f64,
        (tack_pen_ms, gybe_pen_ms): (i64, i64),
    ) -> Option<Vec<ClosingLeg>> {
        closing_legs(
            input,
            pos,
            time_ms,
            tack_now,
            finish_m,
            k,
            deg_to_bam(wind_from_deg),
            tws,
            current_ms,
            wave_f,
            tack_pen_ms,
            gybe_pen_ms,
        )
    }

    /// The engine speed, in knots, whose plane speed at k = 1 is exactly
    /// `ms` metres a second — found to the bit, so a test can put the boat
    /// and the stream at exactly the same speed.
    fn knots_for_exactly(ms: f64) -> f64 {
        let mut kt = ms * (3600.0 / METRES_PER_NM);
        for _ in 0..8 {
            let back = kt / (3600.0 / METRES_PER_NM) * 1.0;
            if back == ms {
                return kt;
            }
            kt = f64::from_bits(if back < ms { kt.to_bits() + 1 } else { kt.to_bits() - 1 });
        }
        panic!("fixture: no knot value gives exactly {ms} m/s");
    }

    /// Zermelo's degenerate case, the boat exactly as fast as the stream
    /// (C² = V², so the quadratic is linear): with the stream fair she makes
    /// good twice her speed, T = D / 2C; with it square across or foul she
    /// can make nothing good at all.
    #[test]
    fn motoring_at_exactly_the_streams_speed() {
        let p = polar();
        let mut cfg = config();
        let v = 2.0;
        cfg.motor_speed_kt = Some(knots_for_exactly(v));
        let calm = ConstantWind { from_deg: 0.0, tws_kt: 0.0 };
        let input = open_water(&p, &calm, &cfg, START, 600);
        let (pos, finish) = ([0.0, 0.0], [0.0, 10_000.0]);
        let t0 = 5_000_000;
        let go = |c: (f64, f64)| close(&input, pos, t0, None, finish, 1.0, (0.0, 0.0), c, 1.0, (0, 0));
        let legs = go((0.0, v)).expect("a fair stream doubles her speed");
        assert_eq!(legs.len(), 1);
        assert!(legs[0].motoring);
        let want = t0 + (10_000.0 / (2.0 * v) * 1000.0) as i64;
        assert!((legs[0].end_time_ms - want).abs() <= 2, "{} vs {want}", legs[0].end_time_ms);
        assert!(go((v, 0.0)).is_none(), "square across, all her speed holds her against it");
        assert!(go((-v, 0.0)).is_none());
        assert!(go((0.0, -v)).is_none(), "foul, she stands still");
    }

    /// Zermelo when the stream outruns the boat: reachable only where
    /// |d − C·t| = V·t has a root, and the passage takes its smallest. The
    /// figures are exact in binary, so the tangent case — exactly one
    /// heading reaches the mark — is hit exactly: V = 2, C = (2, 1),
    /// D = 1024 north, T = 1024 s heading due west.
    #[test]
    fn a_stream_faster_than_the_boat_is_crossed_at_the_first_moment_possible() {
        let p = polar();
        let mut cfg = config();
        cfg.motor_speed_kt = Some(knots_for_exactly(2.0));
        let calm = ConstantWind { from_deg: 0.0, tws_kt: 0.0 };
        let input = open_water(&p, &calm, &cfg, START, 600);
        let pos = [0.0, 0.0];
        let go = |finish: [f64; 2], c: (f64, f64)| {
            close(&input, pos, 0, None, finish, 1.0, (0.0, 0.0), c, 1.0, (0, 0))
                .map(|legs| (legs[0].end_time_ms as f64 / 1000.0, legs[0].heading))
        };
        // Tangent.
        let (t, h) = go([0.0, 1024.0], (2.0, 1.0)).expect("one heading just reaches the mark");
        assert_eq!(t, 1024.0);
        assert_eq!(h, deg_to_bam(270.0));
        // Where the mark is reachable, the time returned must satisfy
        // |d − C·t| = V·t and must be the first such: earlier, the mark is
        // still out of reach.
        let reach = |d: [f64; 2], c: (f64, f64), t: f64| {
            (d[0] - c.0 * t).hypot(d[1] - c.1 * t) - 2.0 * t
        };
        for (d, c) in [
            ([0.0, 1024.0], (1.75, 1.0)),
            ([0.0, 2.0], (1.9921875, 1.0)),
            ([300.0, 900.0], (-1.0, 0.5)),
        ] {
            let (t, _) = go(d, c).expect("reachable");
            assert!(reach(d, c, t).abs() <= 0.002 * 2.0 * t + 0.002, "d {d:?} c {c:?}: t {t}");
            for f in [0.25, 0.5, 0.9] {
                assert!(reach(d, c, f * t) > 0.0, "d {d:?} c {c:?}: reachable already at {} s", f * t);
            }
        }
    }

    /// Straight at the mark under sail across a stream: Zermelo with the
    /// polar's speed. Beam-on to a 10 kn northerly the polar gives 6.0 kn
    /// all the way from 90° to 120°, so with 1.5 kn setting north across a
    /// leg due east the boat heads asin(1.5/6) south of east and makes good
    /// √(6² − 1.5²) kn. Whatever the speeds, the leg must be the one she
    /// sails: her water track plus the drift lands on the mark.
    #[test]
    fn a_straight_leg_across_a_stream_heads_upstream_and_lands_on_the_mark() {
        let p = polar();
        let cfg = config();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let input = open_water(&p, &wind, &cfg, START, 600);
        let pos = merc(START);
        let k = k_of(pos);
        let t0 = 1_000_000;
        let check = |finish: [f64; 2], cur_kt: (f64, f64)| {
            let c = (cur_kt.0 / MS_TO_KNOTS_INV, cur_kt.1 / MS_TO_KNOTS_INV);
            let legs = close(&input, pos, t0, None, finish, k, (0.0, 10.0), c, 1.0, (0, 0))
                .expect("a sailable straight leg");
            assert_eq!(legs.len(), 1);
            let leg = &legs[0];
            let t = (leg.end_time_ms - t0) as f64 / 1000.0;
            let d = [finish[0] - pos[0], finish[1] - pos[1]];
            let water = [d[0] - c.0 * k * t, d[1] - c.1 * k * t];
            let run = water[0].hypot(water[1]);
            let v = kt_pl(leg.stw_kt, k) * t;
            assert!((run - v).abs() / v < 0.005, "water track {run:.1} m, sailed {v:.1} m");
            let steer = water[0].atan2(water[1]).to_degrees().rem_euclid(360.0);
            let head = super::super::angles::bam_to_deg(leg.heading);
            assert!(signed_diff_deg(deg_to_bam(head), deg_to_bam(steer)).abs() < 0.2, "heading {head} vs {steer}");
            let twa = signed_diff_deg(leg.heading, deg_to_bam(0.0)).abs();
            assert!((leg.stw_kt - p.speed_kt(twa, 10.0).unwrap()).abs() < 1e-9);
            (t, head)
        };
        let east = [pos[0] + 3.0 * METRES_PER_NM * k, pos[1]];
        let (t, head) = check(east, (0.0, 1.5));
        let want = 3.0 / (36.0f64 - 2.25).sqrt() * 3600.0;
        assert!((t - want).abs() / want < 0.005, "{t:.0} s vs {want:.0} s");
        assert!((head - (90.0 + (1.5f64 / 6.0).asin().to_degrees())).abs() < 0.2, "heading {head}");
        // A close reach, where the speed changes with the heading, across a
        // strong stream, so the heading has to be found by iteration.
        let ne = [pos[0] + 3.0 * METRES_PER_NM * k * 0.94, pos[1] + 3.0 * METRES_PER_NM * k * 0.34];
        check(ne, (-1.0, 2.5));
        check(ne, (1.5, -1.0));
        // Close-hauled, where the polar is steepest (5.0 kn at 45°, 5.8 at
        // 60°), with the stream pushing her up toward the wind: each round
        // of the iteration changes the speed, so it settles only gradually —
        // within its 2 %, but not to the last microsecond.
        for (brg, cur) in [(58.0f64, (0.8 * 148f64.to_radians().sin(), 0.8 * 148f64.to_radians().cos())), (60.0, (1.0 * 150f64.to_radians().sin(), 1.0 * 150f64.to_radians().cos()))] {
            let r = brg.to_radians();
            let far = [pos[0] + 10.0 * METRES_PER_NM * k * r.sin(), pos[1] + 10.0 * METRES_PER_NM * k * r.cos()];
            let (_, head) = check(far, cur);
            assert!(head > 45.0 && head < brg, "heading {head} for a mark at {brg}°");
        }
        // A leg of 40 m, over in seconds.
        let hop = [pos[0] + 40.0 * k, pos[1]];
        let (t, _) = check(hop, (0.0, 0.0));
        assert!((t - 40.0 / (6.0 * METRES_PER_NM / 3600.0)).abs() < 0.01, "{t} s");
    }

    /// §2.11: the engine comes on when sailing is slower, not when it is
    /// merely as quick. Motor at the sail's own 6.0 kn: she sails. Motor at
    /// 8 kn: she motors.
    #[test]
    fn the_closing_motors_only_when_the_engine_is_faster() {
        let p = polar();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let pos = merc(START);
        let k = k_of(pos);
        let finish = [pos[0] + 2.0 * METRES_PER_NM * k, pos[1]];
        let with_motor = |kt: f64| {
            let mut cfg = config();
            cfg.motor_speed_kt = Some(kt);
            let input = open_water(&p, &wind, &cfg, START, 600);
            close(&input, pos, 0, None, finish, k, (0.0, 10.0), (0.0, 0.0), 1.0, (0, 0)).unwrap()
        };
        let legs = with_motor(6.0);
        assert!(!legs[0].motoring, "motored at no gain");
        assert!((legs[0].end_time_ms as f64 / 3_600_000.0 - 2.0 / 6.0).abs() < 1e-4);
        let legs = with_motor(8.0);
        assert!(legs[0].motoring, "sailed when the engine was faster");
        assert!((legs[0].end_time_ms as f64 / 3_600_000.0 - 2.0 / 8.0).abs() < 1e-4);
    }

    /// The sea slows the engine on the closing leg as on the stepped ones:
    /// 5 kn halved is 2.5 kn, 2 nm in 0.8 h.
    #[test]
    fn the_closing_motor_leg_is_slowed_by_the_sea() {
        let p = polar();
        let mut cfg = config();
        cfg.motor_speed_kt = Some(5.0);
        let calm = ConstantWind { from_deg: 0.0, tws_kt: 0.0 };
        let input = open_water(&p, &calm, &cfg, START, 600);
        let pos = merc(START);
        let k = k_of(pos);
        let finish = [pos[0] + 2.0 * METRES_PER_NM * k, pos[1]];
        let legs = close(&input, pos, 0, None, finish, k, (0.0, 0.0), (0.0, 0.0), 0.5, (0, 0)).unwrap();
        assert!(legs[0].motoring);
        assert!((legs[0].end_time_ms as f64 / 3_600_000.0 - 0.8).abs() < 1e-4, "{}", legs[0].end_time_ms);
    }

    /// A calm, and a gale from a given moment on.
    struct GaleFrom(i64);

    impl WindField for GaleFrom {
        fn wind(&self, _pos: [f64; 2], t: i64) -> (Bam, f64) {
            (deg_to_bam(0.0), if t >= self.0 { 45.0 } else { 0.0 })
        }
    }

    /// The motor leg is judged at the times the boat is on it: a gale due
    /// halfway along stops it, a gale due an hour after she arrives does
    /// not. 2 nm at 5 kn from t0 = 1000 s is 0.4 h.
    #[test]
    fn a_motor_leg_into_a_gale_due_on_the_way_is_refused() {
        let p = polar();
        let mut cfg = config();
        cfg.motor_speed_kt = Some(5.0);
        assert!(cfg.max_tws_kt < 45.0);
        let pos = merc(START);
        let k = k_of(pos);
        let finish = [pos[0] + 2.0 * METRES_PER_NM * k, pos[1]];
        let t0 = 1_000_000;
        let leg_ms = 1_440_000;
        let go = |gale_at: i64| {
            let wind = GaleFrom(gale_at);
            let input = open_water(&p, &wind, &cfg, START, 600);
            close(&input, pos, t0, None, finish, k, (0.0, 0.0), (0.0, 0.0), 1.0, (0, 0))
        };
        assert!(go(t0 + leg_ms / 2).is_none(), "motored into the gale");
        let legs = go(t0 + leg_ms + 3_600_000).expect("the gale comes after she is in");
        assert!((legs[0].end_time_ms - (t0 + leg_ms)).abs() <= 2);
    }

    /// The two-leg wedge when the mark is in the no-go cone: L·cos / VMG,
    /// stretched by the sea's factor, upwind and down.
    #[test]
    fn the_wedge_takes_vmg_time_and_the_sea_stretches_it() {
        let p = polar();
        let cfg = config();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let input = open_water(&p, &wind, &cfg, START, 600);
        let pos = merc(START);
        let k = k_of(pos);
        let nm = METRES_PER_NM * k;
        let v = p.vmg_at(10.0);
        for (finish, vmg, what) in [
            ([pos[0], pos[1] + 2.0 * nm], v.beat_vmg_kt as f64, "beat"),
            ([pos[0], pos[1] - 2.0 * nm], v.run_vmg_kt as f64, "run"),
        ] {
            for wave_f in [1.0, 0.5] {
                let legs = close(&input, pos, 0, None, finish, k, (0.0, 10.0), (0.0, 0.0), wave_f, (0, 0))
                    .expect(what);
                assert_eq!(legs.len(), 2, "{what}: a wedge is two legs");
                let got = legs[1].end_time_ms as f64 / 3_600_000.0;
                let want = 2.0 / (vmg * wave_f);
                assert!((got - want).abs() / want < 0.002, "{what} at ×{wave_f}: {got:.4} h vs {want:.4} h");
                assert_ne!(legs[0].tack, legs[1].tack, "{what}: the wedge changes tack");
            }
        }
    }

    /// From a node already on starboard, a dead beat must tack once (and a
    /// dead run gybe once): the closing charges that manoeuvre at its own
    /// price — the tack penalty for the beat, the gybe penalty for the run.
    #[test]
    fn the_closing_charges_a_tack_as_a_tack_and_a_gybe_as_a_gybe() {
        let p = polar();
        let cfg = config();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let input = open_water(&p, &wind, &cfg, START, 600);
        let pos = merc(START);
        let k = k_of(pos);
        let nm = METRES_PER_NM * k;
        let v = p.vmg_at(10.0);
        let arrive = |finish: [f64; 2], pens: (i64, i64)| {
            let legs = close(&input, pos, 0, Some(TackState::Starboard), finish, k, (0.0, 10.0), (0.0, 0.0), 1.0, pens)
                .unwrap();
            legs.last().unwrap().end_time_ms as f64 / 1000.0
        };
        let up = [pos[0], pos[1] + 2.0 * nm];
        let beat_s = 2.0 / v.beat_vmg_kt as f64 * 3600.0;
        assert!((arrive(up, (100_000, 0)) - (beat_s + 100.0)).abs() < 0.5);
        assert!((arrive(up, (0, 100_000)) - beat_s).abs() < 0.5);
        let down = [pos[0], pos[1] - 2.0 * nm];
        let run_s = 2.0 / v.run_vmg_kt as f64 * 3600.0;
        // A gybe costing more than running dead downwind saves is not made.
        let dead_run_s = 2.0 / p.speed_kt(180.0, 10.0).unwrap() * 3600.0;
        assert!((arrive(down, (0, 100_000)) - dead_run_s.min(run_s + 100.0)).abs() < 0.5);
        assert!((arrive(down, (100_000, 0)) - run_s).abs() < 0.5);
    }

    /// A node within a metre of the mark has arrived: no legs to sail.
    #[test]
    fn a_node_on_the_mark_needs_no_closing_legs() {
        let p = polar();
        let cfg = config();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let input = open_water(&p, &wind, &cfg, START, 600);
        let pos = merc(START);
        let legs = close(&input, pos, 0, None, [pos[0] + 0.3, pos[1] + 0.4], k_of(pos), (0.0, 10.0), (0.0, 0.0), 1.0, (0, 0));
        assert!(legs.expect("arrived").is_empty());
    }

    /// The wedge's legs are sailed in the water: each leg is her heading at
    /// the polar's VMG speed plus the drift, and the two together land on
    /// the mark. A dead beat across a 1 kn stream setting east.
    #[test]
    fn the_wedge_across_a_stream_is_sailed_in_moving_water() {
        let p = polar();
        let cfg = config();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let input = open_water(&p, &wind, &cfg, START, 600);
        let pos = merc(START);
        let k = k_of(pos);
        let finish = [pos[0], pos[1] + 3.0 * METRES_PER_NM * k];
        for c_kt in [(1.0, 0.0), (-0.7, 0.4)] {
            let c = (c_kt.0 / MS_TO_KNOTS_INV, c_kt.1 / MS_TO_KNOTS_INV);
            let legs = close(&input, pos, 0, None, finish, k, (0.0, 10.0), c, 1.0, (0, 0)).expect("a beat");
            assert_eq!(legs.len(), 2);
            let mut at = pos;
            let mut t_prev = 0;
            for leg in &legs {
                let t = (leg.end_time_ms - t_prev) as f64 / 1000.0;
                let h = super::super::angles::bam_to_rad(leg.heading);
                let v = kt_pl(leg.stw_kt, k);
                at = [at[0] + (v * h.sin() + c.0 * k) * t, at[1] + (v * h.cos() + c.1 * k) * t];
                assert!(
                    (at[0] - leg.end_pos[0]).hypot(at[1] - leg.end_pos[1]) < 2.0,
                    "stream {c_kt:?}: leg ends {:?}, sailed to {at:?}",
                    leg.end_pos
                );
                t_prev = leg.end_time_ms;
            }
            assert!((at[0] - finish[0]).hypot(at[1] - finish[1]) < 2.0, "missed the mark");
        }
    }

    /// A boat that cannot sail past the beam: 6 kn at 90°, nothing beyond.
    /// Her best "run" is the beam reach itself, a cone of exactly 90°.
    const BEAM_POLAR: &str = "\
twa/tws\t6\t20
45\t5\t5
90\t6\t6
91\t0\t0
180\t0\t0
";

    /// The no-go cone downwind of a boat that cannot bear away past the
    /// beam: a mark at 120° off the wind is in it. A stream setting south
    /// at C carries her there while she reaches east and west across it —
    /// the mark is 2 nm off, 1 nm of it to the south, so the passage takes
    /// exactly 1 nm / C, provided she can make the 1.73 nm of easting in
    /// that time (C ≤ 3.46 kn at 6 kn). Faster, or in still water, she
    /// cannot get there at all.
    #[test]
    fn a_mark_abaft_the_beam_is_reached_on_the_stream_or_not_at_all() {
        let p = Polar::parse(BEAM_POLAR).unwrap();
        assert_eq!(p.vmg_at(10.0).run_twa_deg, 90.0, "fixture polar");
        let cfg = config();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let input = open_water(&p, &wind, &cfg, START, 600);
        let pos = merc(START);
        let k = k_of(pos);
        let nm = METRES_PER_NM * k;
        let mark_at = |brg: f64| {
            let r = brg.to_radians();
            [pos[0] + 2.0 * nm * r.sin(), pos[1] + 2.0 * nm * r.cos()]
        };
        let go = |brg: f64, south_kt: f64| {
            let c = (0.0, -south_kt / MS_TO_KNOTS_INV);
            close(&input, pos, 0, None, mark_at(brg), k, (0.0, 10.0), c, 1.0, (0, 0))
        };
        for brg in [120.0, 240.0] {
            let legs = go(brg, 2.0).expect("the stream carries her down to it");
            let h = legs.last().unwrap().end_time_ms as f64 / 3_600_000.0;
            assert!((h - 0.5).abs() < 0.001, "{brg}°: {h} h, expected 0.5 h");
            assert!(go(brg, 4.0).is_none(), "{brg}°: a 4 kn stream sweeps her past it");
            assert!(go(brg, 0.0).is_none(), "{brg}°: in still water she cannot sail there");
        }
    }

    /// Calm air near the mark and a gale everywhere east of the rhumb line,
    /// before t0, from 10 h after it, and — optionally — round one point
    /// for the minute before a given time.
    struct WedgeWeather {
        x_east: f64,
        t0: i64,
        spot: Option<([f64; 2], f64, i64)>,
    }

    impl WindField for WedgeWeather {
        fn wind(&self, pos: [f64; 2], t: i64) -> (Bam, f64) {
            let mut gale = pos[0] > self.x_east
                || (self.t0 - 60_000..self.t0).contains(&t)
                || t >= self.t0 + 36_000_000;
            if let Some((c, r, until)) = self.spot {
                if (pos[0] - c[0]).hypot(pos[1] - c[1]) < r && (until - 60_000..until).contains(&t) {
                    gale = true;
                }
            }
            (deg_to_bam(0.0), if gale { 45.0 } else { 10.0 })
        }
    }

    /// A wedge's legs are judged from when each begins — after its tack —
    /// to when it ends. Arriving on port with the port-first wedge walled
    /// off by a gale to the east, she must tack onto starboard, sail, and
    /// tack back: two penalties of a minute. Weather before she reaches the
    /// node, or at the tack point before she reaches it, or long after she
    /// is in, is none of the passage's business.
    #[test]
    fn a_wedge_is_judged_over_the_time_each_leg_is_sailed() {
        let p = polar();
        let cfg = config();
        let pos = merc(START);
        let k = k_of(pos);
        let finish = [pos[0], pos[1] + 3.0 * METRES_PER_NM * k];
        let t0 = 1_000_000;
        let pens = (60_000, 60_000);
        let go = |spot| {
            let wind = WedgeWeather { x_east: pos[0] + 100.0, t0, spot };
            let input = open_water(&p, &wind, &cfg, START, 600);
            close(&input, pos, t0, Some(TackState::Port), finish, k, (0.0, 10.0), (0.0, 0.0), 1.0, pens)
        };
        let legs = go(None).expect("the starboard-first wedge is open");
        assert_eq!(legs.len(), 2);
        assert_eq!(legs[0].tack, TackState::Starboard, "the port-first wedge sails into the gale");
        let vmg = p.vmg_at(10.0).beat_vmg_kt as f64;
        let want = t0 + 120_000 + (3.0 / vmg * 3_600_000.0) as i64;
        assert!((legs[1].end_time_ms - want).abs() < 50, "{} vs {want}", legs[1].end_time_ms);
        // A squall at the tack point, gone the moment she gets there.
        let (mid, mid_t) = (legs[0].end_pos, legs[0].end_time_ms);
        let again = go(Some((mid, 0.5 * METRES_PER_NM * k, mid_t))).expect("the squall is over");
        assert_eq!(again[1].end_time_ms, legs[1].end_time_ms);
    }


    // ---------------------------------------------------------------------
    // Post-processing: extract and its simplification.
    // ---------------------------------------------------------------------

    /// Ground nautical miles east and north of START, as Mercator plane.
    fn nm_off(east: f64, north: f64) -> [f64; 2] {
        let s = merc(START);
        let nm = METRES_PER_NM * k_of(s);
        [s[0] + east * nm, s[1] + north * nm]
    }

    /// A closing leg ending at `end`, on `tack`, at `end_time_ms`.
    fn leg_to(end: [f64; 2], tack: TackState, end_time_ms: i64) -> ClosingLeg {
        ClosingLeg {
            end_pos: end,
            heading: deg_to_bam(90.0),
            twa_deg: if tack == TackState::Port { 90.0 } else { -90.0 },
            tws_kt: 10.0,
            stw_kt: 6.0,
            tack,
            end_time_ms,
            motoring: false,
        }
    }

    /// Run extract over a hand-built chain, closed by one leg on to `end`.
    fn extract_chain(input: &IsochroneInput<'_>, nodes: &[Node], end: [f64; 2]) -> Isoroute {
        let last = nodes.last().unwrap();
        let t_end = last.time_ms + 600_000;
        extract(input, nodes, nodes.len() as u32 - 1, t_end, vec![leg_to(end, last.tack, t_end)], 0)
    }

    /// Does the plan hold a vertex at `p` (Mercator), to a metre?
    fn has_vertex(r: &Isoroute, p: [f64; 2]) -> bool {
        r.points.iter().any(|q| {
            let m = merc(q.pos);
            (m[0] - p[0]).hypot(m[1] - p[1]) < 1.0
        })
    }

    /// The simplification tolerance is in *ground* miles: at 56.5° N, where
    /// a plane metre is 0.55 ground metres, a corner 0.075 nm off the chord
    /// is inside the 0.1 nm default and goes, one 0.13 nm off stays.
    #[test]
    fn the_simplify_tolerance_is_measured_on_the_ground() {
        let p = polar();
        let cfg = config();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let input = open_water(&p, &wind, &cfg, START, 600);
        for (off, kept) in [(0.075, false), (0.13, true)] {
            let corner = nm_off(1.5, off);
            let nodes = chain_of(&[nm_off(0.0, 0.0), corner, nm_off(3.0, 0.0)], TackState::Starboard);
            let r = extract_chain(&input, &nodes, nm_off(4.5, 0.0));
            assert_eq!(has_vertex(&r, corner), kept, "a corner {off} nm off the chord");
            // Start and mark are always there.
            assert!(has_vertex(&r, nm_off(0.0, 0.0)) && has_vertex(&r, nm_off(4.5, 0.0)));
        }
    }

    /// §7.8: tack changes are real manoeuvres and always survive; between
    /// them Douglas–Peucker eats the zigzag. A sawtooth of short tacks well
    /// inside the tolerance keeps every tack in the plan; the same wiggle
    /// on one tack is eaten whole.
    #[test]
    fn simplifying_keeps_every_tack_and_eats_the_wiggle_between() {
        let p = polar();
        let cfg = config();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let input = open_water(&p, &wind, &cfg, START, 600);
        let zig: Vec<[f64; 2]> = (0..7)
            .map(|i| nm_off(0.3 * i as f64, if i % 2 == 1 { 0.02 } else { 0.0 }))
            .collect();
        let end = nm_off(2.0, 0.0);

        let mut tacking = chain_of(&zig, TackState::Starboard);
        for (i, n) in tacking.iter_mut().enumerate().skip(1) {
            n.tack = if i % 2 == 1 { TackState::Port } else { TackState::Starboard };
        }
        let r = extract_chain(&input, &tacking, end);
        // The start's own tack is no manoeuvre: n1…n6 alternate, five tacks.
        assert_eq!(r.raw_tack_changes, 5);
        let shown = r.points[1..].windows(2).filter(|w| w[0].tack != w[1].tack).count();
        assert_eq!(shown, r.raw_tack_changes, "a tack was simplified away");

        let one_tack = chain_of(&zig, TackState::Starboard);
        let r = extract_chain(&input, &one_tack, end);
        assert_eq!(r.raw_tack_changes, 0);
        assert_eq!(r.points.len(), 2, "the wiggle on one tack survived: {} points", r.points.len());
    }

    /// The junction pass merges a vertex only where the course runs
    /// straight on: a real corner on one tack stays.
    #[test]
    fn a_corner_on_one_tack_is_not_merged_away() {
        let p = polar();
        let cfg = config();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let input = open_water(&p, &wind, &cfg, START, 600);
        let corner = nm_off(2.0, 1.0);
        let nodes = chain_of(&[nm_off(0.0, 0.0), corner], TackState::Starboard);
        let r = extract_chain(&input, &nodes, nm_off(4.0, 0.0));
        assert!(has_vertex(&r, corner), "the corner was merged");
        assert_eq!(r.points.len(), 3);
    }

    /// A square island, Mercator corners, stamped into a fresh grid a few
    /// miles round START.
    fn island_grid(lo: [f64; 2], hi: [f64; 2]) -> Grid {
        let (a, b) = (nm_off(-1.0, -2.5), nm_off(7.0, 2.5));
        let mut grid = Grid::new(a, b, 40.0);
        let (c1, c2, c3, c4) = (lo, [hi[0], lo[1]], hi, [lo[0], hi[1]]);
        grid.stamp_triangle([c1, c2, c3], true);
        grid.stamp_triangle([c1, c3, c4], true);
        grid.finalize();
        grid
    }

    /// Ground metres from `p` to the box `lo..hi` (Mercator).
    fn ground_to_box(p: [f64; 2], lo: [f64; 2], hi: [f64; 2]) -> f64 {
        let dx = (lo[0] - p[0]).max(p[0] - hi[0]).max(0.0);
        let dy = (lo[1] - p[1]).max(p[1] - hi[1]).max(0.0);
        dx.hypot(dy) / k_of(p)
    }

    /// The closest any leg of the plan comes to the box, ground metres.
    fn closest_pass(r: &Isoroute, lo: [f64; 2], hi: [f64; 2]) -> f64 {
        let mut best = f64::INFINITY;
        for w in r.points.windows(2) {
            let (a, b) = (merc(w[0].pos), merc(w[1].pos));
            for i in 0..=400 {
                let t = i as f64 / 400.0;
                best = best.min(ground_to_box([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t], lo, hi));
            }
        }
        best
    }

    /// The offing is ground metres: a leg 150 m off an island is clear at a
    /// 100 m offing and not at a 200 m one — at 56.5° N, where those are
    /// 181 and 362 plane metres against the 271 the leg actually keeps.
    #[test]
    fn a_leg_is_clear_only_at_the_offing_it_keeps_on_the_ground() {
        let (lo, hi) = (nm_off(2.0, 0.0), nm_off(3.0, 1.0));
        let grid = island_grid(lo, hi);
        let p = polar();
        let cfg = config();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let off = -150.0 / METRES_PER_NM;
        let (a, b) = (nm_off(0.0, off), nm_off(5.0, off));
        assert!((ground_to_box(nm_off(2.5, off), lo, hi) - 150.0).abs() < 1.0, "fixture");
        let at = |offing_m: f64| {
            let input = IsochroneInput { grid: Some(&grid), offing_min_m: offing_m, ..open_water(&p, &wind, &cfg, START, 600) };
            leg_clear(&input, a, b)
        };
        assert!(at(100.0), "150 m off is clear at a 100 m offing");
        assert!(!at(200.0), "150 m off is not clear at a 200 m offing");
        // And a leg through the island is never clear.
        let input = IsochroneInput { grid: Some(&grid), offing_min_m: 0.0, ..open_water(&p, &wind, &cfg, START, 600) };
        assert!(!leg_clear(&input, nm_off(0.0, 0.5), nm_off(5.0, 0.5)));
    }

    /// Simplifying moves legs sideways, and a leg is only known safe where
    /// the chart has checked it: with a tolerance wide enough to cut the
    /// corner of an island, the dropped corners come back until no leg
    /// crosses it.
    #[test]
    fn simplifying_never_cuts_a_corner_across_land() {
        let (lo, hi) = (nm_off(2.0, -1.0), nm_off(3.0, 1.0));
        let grid = island_grid(lo, hi);
        let p = polar();
        let mut cfg = config();
        cfg.simplify_tolerance_nm = 5.0;
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let input = IsochroneInput { grid: Some(&grid), ..open_water(&p, &wind, &cfg, START, 600) };
        let round = [nm_off(0.0, 0.0), nm_off(1.5, 1.2), nm_off(3.5, 1.3), nm_off(5.0, 0.0)];
        let nodes = chain_of(&round, TackState::Starboard);
        let r = extract_chain(&input, &nodes, nm_off(6.0, 0.0));
        assert!(closest_pass(&r, lo, hi) > 0.0, "a simplified leg crosses the island");
        for v in &round {
            assert!(has_vertex(&r, *v), "the way round lost a vertex");
        }
        // Where the raw chain is all there is — one step, even one the chart
        // would refuse — it comes back as it went in.
        let blind = chain_of(&[nm_off(0.0, 0.0), nm_off(5.0, 0.0)], TackState::Starboard);
        let r = extract_chain(&input, &blind, nm_off(6.0, 0.0));
        assert!(has_vertex(&r, nm_off(0.0, 0.0)) && has_vertex(&r, nm_off(5.0, 0.0)));
    }

    /// The offing holds along every leg the search makes — stepped and
    /// analytic — in ground metres: islets 150 m off the rhumb line, one
    /// early in the passage and one inside the final approach, are passed
    /// at 200 m (less a cell of chart).
    #[test]
    fn the_route_keeps_its_offing_from_the_chart_on_every_leg() {
        let p = polar();
        let cfg = config();
        let off = 150.0 / METRES_PER_NM;
        let (lo1, hi1) = (nm_off(2.4, off), nm_off(2.6, off + 0.2));
        let (lo2, hi2) = (nm_off(6.4, off), nm_off(6.6, off + 0.2));
        let (a, b) = (nm_off(-1.0, -2.0), nm_off(9.0, 2.0));
        let mut grid = Grid::new(a, b, 40.0);
        for (lo, hi) in [(lo1, hi1), (lo2, hi2)] {
            let (c1, c2, c3, c4) = (lo, [hi[0], lo[1]], hi, [lo[0], hi[1]]);
            grid.stamp_triangle([c1, c2, c3], true);
            grid.stamp_triangle([c1, c3, c4], true);
        }
        grid.finalize();
        let finish_m = nm_off(8.0, 0.0);
        let (lat, lon) = Projection::to_wgs84(finish_m[0], finish_m[1]);
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let r = solve(&IsochroneInput {
            grid: Some(&grid),
            offing_min_m: 200.0,
            ..open_water(&p, &wind, &cfg, LatLon::new(lat, lon), 600)
        })
        .expect("room to pass outside the islets");
        for (lo, hi) in [(lo1, hi1), (lo2, hi2)] {
            let d = closest_pass(&r, lo, hi);
            assert!(d > 175.0, "passed an islet at {d:.0} m");
        }
    }

    // ---------------------------------------------------------------------
    // Properties.
    // ---------------------------------------------------------------------

    use proptest::prelude::*;

    /// The constant-wind optimum. With no penalties the boat can mix any
    /// headings in any proportion, so what she can make good toward the
    /// mark is the furthest point of the convex hull of her polar's
    /// velocities that lies on the bearing — found by brute force over
    /// every pair of headings either side of it (and any heading straight
    /// at it). The polar is read a whole degree of TWA at a time, so the
    /// hull's corners lie at the ends of those degrees: those, both sides
    /// of the wind, are the headings tried.
    fn best_made_good_kt(p: &Polar, wind_from_deg: f64, tws: f64, bearing_deg: f64) -> f64 {
        // Velocities in a frame with the bearing up the y axis.
        let mut v: Vec<(f64, f64)> = Vec::new();
        for n in 0..=180 {
            for d in [-0.4999, 0.0, 0.4999] {
                let twa = (n as f64 + d).clamp(0.0, 180.0);
                let Some(s) = p.speed_kt(twa, tws) else { continue };
                for side in [1.0, -1.0] {
                    let rel = (wind_from_deg + side * twa - bearing_deg).to_radians();
                    v.push((s * rel.sin(), s * rel.cos()));
                }
            }
        }
        let mut best = 0.0f64;
        for &(xi, yi) in &v {
            if xi.abs() < 1e-12 {
                best = best.max(yi);
            }
            if xi <= 0.0 {
                continue;
            }
            for &(xj, yj) in &v {
                if xj < 0.0 {
                    best = best.max((yi * -xj + yj * xi) / (xi - xj));
                }
            }
        }
        best
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(12))]

        /// In a constant wind on open water the route starts at the start,
        /// ends on the mark at the arrival time, runs forward in time, and
        /// takes the analytic optimum, L over the best speed made good on
        /// the bearing: never less (the polar forbids it), and no more than
        /// the heading fan's discretisation.
        #[test]
        fn constant_wind_routes_take_the_analytic_optimum(
            bearing in 0.0f64..360.0,
            l_nm in 2.0f64..10.0,
            tws in 6.0f64..20.0,
            wind_from in 0.0f64..360.0,
        ) {
            let p = polar();
            let mut cfg = config();
            cfg.tack_penalty_day_s = 0;
            cfg.gybe_penalty_day_s = 0;
            let finish = mark(START, bearing, l_nm);
            let wind = ConstantWind { from_deg: wind_from, tws_kt: tws };
            let r = solve(&open_water(&p, &wind, &cfg, finish, 600)).expect("open water always sails");
            let best_h = l_nm / best_made_good_kt(&p, wind_from, tws, bearing);
            let got = hours(&r);
            // The engine measures the sea on the spherical Mercator, the
            // mark is placed on the ellipsoid: up to a quarter of a percent
            // apart in these latitudes, hence the half-percent of slack.
            prop_assert!(got >= best_h * 0.995, "{got:.4} h beats the polar's {best_h:.4} h");
            prop_assert!(got <= best_h * 1.02, "{got:.4} h against an optimum of {best_h:.4} h");
            let first = &r.points[0];
            let last = r.points.last().unwrap();
            prop_assert!(crate::geo::distance_m(first.pos, START) < 1.0);
            prop_assert!(crate::geo::distance_m(last.pos, finish) < 1.0);
            prop_assert_eq!(last.time_ms, r.arrival_ms);
            prop_assert!(r.points.windows(2).all(|w| w[0].time_ms <= w[1].time_ms));
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(8))]

        /// Wherever the plan is inside a tidal gate's circle, the gate is
        /// open: every point of every leg (the boat moving steadily along
        /// it) that lies well inside the circle is passed inside the window.
        /// A route need not exist; if one does, it obeys the gate.
        #[test]
        fn a_route_is_inside_a_gate_only_while_it_is_open(
            along in 0.3f64..0.7,
            across in -1.0f64..1.0,
            radius_nm in 0.5f64..2.0,
            open_h in 0.0f64..2.0,
            len_h in 0.3f64..2.0,
        ) {
            let p = polar();
            let cfg = config();
            let l_nm = 10.0;
            let finish = mark(START, 90.0, l_nm);
            let centre = mark(mark(START, 90.0, l_nm * along), 0.0, across);
            let h = 3_600_000.0;
            let window = ((open_h * h) as i64, ((open_h + len_h) * h) as i64);
            let gates = [TidalGate { pos: centre, radius_nm, windows: vec![window] }];
            let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
            let input = IsochroneInput { gates: &gates, ..open_water(&p, &wind, &cfg, finish, 600) };
            if let Ok(r) = solve(&input) {
                let slack_ms = 120_000;
                for w in r.points.windows(2) {
                    let (a, b) = (merc(w[0].pos), merc(w[1].pos));
                    for i in 0..=200 {
                        let f = i as f64 / 200.0;
                        let m = [a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f];
                        let (lat, lon) = Projection::to_wgs84(m[0], m[1]);
                        let d_nm = crate::geo::distance_m(LatLon::new(lat, lon), centre) / METRES_PER_NM;
                        if d_nm < radius_nm - 0.15 {
                            let t = w[0].time_ms + ((w[1].time_ms - w[0].time_ms) as f64 * f) as i64;
                            prop_assert!(
                                t >= window.0 - slack_ms && t <= window.1 + slack_ms,
                                "in the gate at {} min, open {}–{} min",
                                t / 60_000, window.0 / 60_000, window.1 / 60_000
                            );
                        }
                    }
                }
            }
        }
    }


    /// The best speed made good on a bearing in a uniform stream: Zermelo
    /// in uniform water is the same hull, shifted by the drift (knots, east
    /// and north).
    fn best_made_good_in_stream_kt(
        p: &Polar,
        wind_from_deg: f64,
        tws: f64,
        bearing_deg: f64,
        drift_en_kt: (f64, f64),
    ) -> f64 {
        let b = bearing_deg.to_radians();
        // The drift in the bearing's frame: x to starboard of it, y along.
        let (cx, cy) = (
            drift_en_kt.0 * b.cos() - drift_en_kt.1 * b.sin(),
            drift_en_kt.0 * b.sin() + drift_en_kt.1 * b.cos(),
        );
        let mut v: Vec<(f64, f64)> = Vec::new();
        for n in 0..=180 {
            for d in [-0.4999, 0.0, 0.4999] {
                let twa = (n as f64 + d).clamp(0.0, 180.0);
                let Some(s) = p.speed_kt(twa, tws) else { continue };
                for side in [1.0, -1.0] {
                    let rel = (wind_from_deg + side * twa - bearing_deg).to_radians();
                    v.push((s * rel.sin() + cx, s * rel.cos() + cy));
                }
            }
        }
        let mut best = 0.0f64;
        for &(xi, yi) in &v {
            if xi <= 0.0 {
                continue;
            }
            for &(xj, yj) in &v {
                if xj < 0.0 {
                    best = best.max((yi * -xj + yj * xi) / (xi - xj));
                }
            }
        }
        best
    }

    /// Sailing across a stream, the course to steer lies well off the
    /// bearing of the mark: a westerly, a mark 12 nm north, 3 kn setting
    /// east, and the boat must hold some 30° to windward of the rhumb line
    /// to stem it — a heading only the full fan offers, neither VMG heading
    /// nor the ring behind. Zermelo's optimum for the polar in uniform
    /// water is the bound either way.
    #[test]
    fn a_cross_stream_is_stemmed_with_the_full_fan() {
        let p = polar();
        let cfg = config();
        let wind = ConstantWind { from_deg: 270.0, tws_kt: 10.0 };
        let stream = ConstantCurrent { set_deg: 90.0, drift_kt: 3.0 };
        let l_nm = 12.0;
        let r = solve(&IsochroneInput {
            current: Some(&stream),
            ..open_water(&p, &wind, &cfg, mark(START, 0.0, l_nm), 600)
        })
        .expect("a 6 kn boat stems a 3 kn stream");
        let best_h = l_nm / best_made_good_in_stream_kt(&p, 270.0, 10.0, 0.0, (3.0, 0.0));
        let got = hours(&r);
        assert!(got >= best_h * 0.995, "{got:.3} h beats Zermelo's {best_h:.3} h");
        assert!(got <= best_h * 1.015, "{got:.3} h against Zermelo's {best_h:.3} h");
    }

    /// The mark lies exactly on one tack's course made good, the stream
    /// included: the wedge's other leg has no length at all — x or y is
    /// exactly zero — and the passage is that one leg, 1024 s. The figures
    /// are built from the same expressions the wedge uses, so zero is zero
    /// to the bit; the expectation is the geometry.
    #[test]
    fn a_mark_on_one_tacks_course_made_good_is_one_legs_sail() {
        let p = polar();
        let cfg = config();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let input = open_water(&p, &wind, &cfg, START, 600);
        let stw = p.vmg_at(10.0).beat_stw_kt as f64;
        let v_pl = stw / (3600.0 / METRES_PER_NM) * 1.0;
        for (twa, c) in [(-45.0, (1.0, 0.0)), (45.0, (-1.0, 0.0))] {
            let r = bam_to_rad(deg_to_bam(0.0).wrapping_add(deg_to_bam(twa)));
            let g = [v_pl * r.sin() + c.0 * 1.0, v_pl * r.cos() + c.1 * 1.0];
            let finish = [1024.0 * g[0], 1024.0 * g[1]];
            // Inside the no-go cone, so no straight leg: only the wedge.
            let brg = finish[0].atan2(finish[1]).to_degrees();
            assert!(brg.abs() < 45.0, "fixture: bearing {brg}");
            let legs = close(&input, [0.0, 0.0], 0, None, finish, 1.0, (0.0, 10.0), c, 1.0, (0, 0))
                .unwrap_or_else(|| panic!("the mark on the {twa}° course made good was not reached"));
            let t = legs.last().unwrap().end_time_ms;
            assert!((t - 1_024_000).abs() <= 2, "{twa}°: {t} ms");
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Whatever the analytic closing proposes, it is a plan the boat can
        /// sail: replaying each leg — her heading at her speed through the
        /// water, plus the stream, for the leg's time less any tack or gybe
        /// it opens with — lands on the leg's end and the last one on the
        /// mark, in order. Both polars, any wind, stream, sea and engine.
        #[test]
        fn every_closing_plan_lands_on_the_mark(
            spiky in any::<bool>(),
            brg in 0.0f64..360.0,
            d_nm in 0.05f64..8.0,
            wind_from in 0.0f64..360.0,
            tws in 6.0f64..25.0,
            set in 0.0f64..360.0,
            drift_kt in 0.0f64..3.0,
            wave_f in 0.5f64..1.0,
            tack in 0usize..3,
            pens in (0i64..300_000, 0i64..300_000),
            motor in proptest::option::of(3.0f64..8.0),
        ) {
            let p = if spiky { Polar::parse(SPIKY_POLAR).unwrap() } else { polar() };
            let mut cfg = config();
            cfg.motor_speed_kt = motor;
            let wind = ConstantWind { from_deg: wind_from, tws_kt: tws };
            let input = open_water(&p, &wind, &cfg, START, 600);
            let pos = merc(START);
            let k = k_of(pos);
            let b = brg.to_radians();
            let finish = [pos[0] + d_nm * METRES_PER_NM * k * b.sin(), pos[1] + d_nm * METRES_PER_NM * k * b.cos()];
            let c = (drift_kt / MS_TO_KNOTS_INV * set.to_radians().sin(), drift_kt / MS_TO_KNOTS_INV * set.to_radians().cos());
            // The closing does not drift the boat while she tacks; keep the
            // two apart (that is a question of its own, below).
            let pens = if drift_kt > 0.0 { (0, 0) } else { pens };
            let tack_now = [None, Some(TackState::Port), Some(TackState::Starboard)][tack];
            let t0 = 7_000_000;
            if let Some(legs) = close(&input, pos, t0, tack_now, finish, k, (wind_from, tws), c, wave_f, pens) {
                prop_assert!(!legs.is_empty());
                let mut at = pos;
                let mut t_prev = t0;
                let mut from = tack_now;
                let mut through_water = 0.0;
                for leg in &legs {
                    let pen = if !leg.motoring && from.is_some_and(|f| f != leg.tack) {
                        if leg.twa_deg.abs() < 90.0 { pens.0 } else { pens.1 }
                    } else {
                        0
                    };
                    let t = (leg.end_time_ms - t_prev - pen) as f64 / 1000.0;
                    prop_assert!(t >= 0.0, "a leg of negative time");
                    let h = bam_to_rad(leg.heading);
                    let v = kt_pl(leg.stw_kt, k);
                    at = [at[0] + (v * h.sin() + c.0 * k) * t, at[1] + (v * h.cos() + c.1 * k) * t];
                    through_water += v * t;
                    // A sailed leg's speed is the polar's (or the VMG
                    // table's) at its TWA, through the sea.
                    if !leg.motoring {
                        let polar_v = p.speed_kt(leg.twa_deg.abs(), tws).unwrap_or(0.0) * wave_f;
                        prop_assert!((leg.stw_kt - polar_v).abs() <= 1e-9 * polar_v, "stw {} vs polar {}", leg.stw_kt, polar_v);
                    }
                    t_prev = leg.end_time_ms;
                    from = if leg.motoring { from } else { Some(leg.tack) };
                }
                let miss = (at[0] - finish[0]).hypot(at[1] - finish[1]);
                let d = d_nm * METRES_PER_NM * k;
                // Headings are BAM, 65536 to the turn: half a step is
                // 4.8e-5 rad of aim, which over a long crawl against a
                // stream adds up — so the bound grows with the water sailed.
                prop_assert!(
                    miss <= 0.03 * d + 1e-4 * through_water + 2.0,
                    "replayed plan misses the mark by {miss:.0} m of {d:.0} ({through_water:.0} m through the water)"
                );
            }
        }
    }

    /// The stepped search drifts the boat through a tack ("a tack in a 2 kt
    /// stream still drifts"); the analytic closing should too. A dead beat
    /// of 2 nm from a node on starboard, one 240 s tack, a 2 kn stream
    /// setting east: replayed with the drift running through the tack, the
    /// plan should still land on the mark.
    #[test]
    fn the_closing_drifts_through_its_tacks() {
        let p = polar();
        let cfg = config();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let input = open_water(&p, &wind, &cfg, START, 600);
        let pos = merc(START);
        let k = k_of(pos);
        let finish = [pos[0], pos[1] + 2.0 * METRES_PER_NM * k];
        let c = (2.0 / MS_TO_KNOTS_INV, 0.0);
        let legs = close(&input, pos, 0, Some(TackState::Starboard), finish, k, (0.0, 10.0), c, 1.0, (240_000, 240_000))
            .expect("a beat");
        let mut at = pos;
        let mut t_prev = 0;
        let mut from = Some(TackState::Starboard);
        for leg in &legs {
            let pen = if from.is_some_and(|f| f != leg.tack) { 240_000 } else { 0 };
            let sail = (leg.end_time_ms - t_prev - pen) as f64 / 1000.0;
            let all = (leg.end_time_ms - t_prev) as f64 / 1000.0;
            let h = bam_to_rad(leg.heading);
            let v = kt_pl(leg.stw_kt, k);
            at = [at[0] + v * h.sin() * sail + c.0 * k * all, at[1] + v * h.cos() * sail + c.1 * k * all];
            t_prev = leg.end_time_ms;
            from = Some(leg.tack);
        }
        let miss = (at[0] - finish[0]).hypot(at[1] - finish[1]) / k;
        assert!(miss < 20.0, "the plan, drifted through its tack, misses by {miss:.0} m");
    }


    /// The closing legs keep the offing in ground metres too: a straight
    /// reach passing 150 m off an islet is no leg at a 200 m offing (362
    /// plane metres here, against the 271 it keeps), and is one at 100 m.
    #[test]
    fn a_closing_leg_keeps_the_offing_on_the_ground() {
        let (lo, hi) = (nm_off(2.0, 0.0), nm_off(3.0, 1.0));
        let grid = island_grid(lo, hi);
        let p = polar();
        let cfg = config();
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let off = -150.0 / METRES_PER_NM;
        let (a, b) = (nm_off(0.0, off), nm_off(5.0, off));
        let k = k_of(a);
        let at = |offing_m: f64| {
            let input = IsochroneInput { grid: Some(&grid), offing_min_m: offing_m, ..open_water(&p, &wind, &cfg, START, 600) };
            close(&input, a, 0, None, b, k, (0.0, 10.0), (0.0, 0.0), 1.0, (0, 0))
        };
        assert!(at(100.0).is_some(), "150 m off is clear at a 100 m offing");
        assert!(at(200.0).is_none(), "the reach was allowed 150 m off at a 200 m offing");
    }



    /// Replay a closing plan as the boat would sail it — each leg her
    /// heading at her speed through the water plus the stream, for the
    /// leg's time less any tack or gybe it opens with — and return how far
    /// from `finish` she ends, and how far she sailed through the water.
    #[allow(clippy::too_many_arguments)]
    fn replay_miss(
        legs: &[ClosingLeg],
        pos: [f64; 2],
        finish: [f64; 2],
        t0: i64,
        tack_now: Option<TackState>,
        c: (f64, f64),
        k: f64,
        pens: (i64, i64),
    ) -> (f64, f64) {
        let mut at = pos;
        let mut t_prev = t0;
        let mut from = tack_now;
        let mut through_water = 0.0;
        for leg in legs {
            let pen = if !leg.motoring && from.is_some_and(|f| f != leg.tack) {
                if leg.twa_deg.abs() < 90.0 { pens.0 } else { pens.1 }
            } else {
                0
            };
            let t = (leg.end_time_ms - t_prev - pen) as f64 / 1000.0;
            let h = bam_to_rad(leg.heading);
            let v = kt_pl(leg.stw_kt, k);
            at = [at[0] + (v * h.sin() + c.0 * k) * t, at[1] + (v * h.cos() + c.1 * k) * t];
            through_water += v * t;
            t_prev = leg.end_time_ms;
            from = if leg.motoring { from } else { Some(leg.tack) };
        }
        ((at[0] - finish[0]).hypot(at[1] - finish[1]), through_water)
    }

    /// The straight leg's heading is found by iteration, and the polar is
    /// read a whole degree at a time, so near a degree's edge the speed can
    /// flip between two readings a percent apart. Such a leg is still a
    /// leg — it is accepted within 2 % — but one swinging between readings
    /// far apart is not, and must not be offered. Close-hauled across a
    /// stream every one of these reaches is sailable; on the spiky polar
    /// (a third of its speed lost in five degrees) any straight leg offered
    /// must land on the mark.
    #[test]
    fn a_straight_leg_is_offered_once_its_heading_settles_and_only_then() {
        let wind = ConstantWind { from_deg: 0.0, tws_kt: 10.0 };
        let cfg = config();
        let pos = merc(START);
        let k = k_of(pos);
        let at_brg = |brg: f64, nm: f64| {
            let r = brg.to_radians();
            [pos[0] + nm * METRES_PER_NM * k * r.sin(), pos[1] + nm * METRES_PER_NM * k * r.cos()]
        };
        let ms = |kt: f64, set: f64| {
            let r = set.to_radians();
            (kt / MS_TO_KNOTS_INV * r.sin(), kt / MS_TO_KNOTS_INV * r.cos())
        };

        let p = polar();
        let input = open_water(&p, &wind, &cfg, START, 600);
        let finish = at_brg(58.0, 10.0);
        // From 0.3 kn to 1.12 kn the course to steer runs from 55° down to
        // 45°, the edge of the no-go; at 1.2 kn it would be inside it.
        assert!(close(&input, pos, 0, None, finish, k, (0.0, 10.0), ms(1.2, 148.0), 1.0, (0, 0)).is_none());
        for i in 30..=112 {
            let c = ms(i as f64 / 100.0, 148.0);
            let legs = close(&input, pos, 0, None, finish, k, (0.0, 10.0), c, 1.0, (0, 0))
                .unwrap_or_else(|| panic!("no straight leg across {} kn", i as f64 / 100.0));
            assert_eq!(legs.len(), 1);
            let (miss, _) = replay_miss(&legs, pos, finish, 0, None, c, k, (0, 0));
            assert!(miss < 0.03 * 10.0 * METRES_PER_NM * k, "{} kn: misses by {miss:.0} m", i as f64 / 100.0);
        }
        // A broad reach (125°) with the stream square across it: here more
        // speed means less to correct and a course nearer the wind, where
        // this polar is *faster* still… the other way round from above, so
        // near a degree's edge the iteration ends swinging between two
        // readings 1 % apart. Each of these is still a plain reach to sail.
        let finish = at_brg(125.0, 10.0);
        for i in 0..=40 {
            let kt = 0.355 + i as f64 * 0.0005;
            let c = ms(kt, 215.0);
            let legs = close(&input, pos, 0, None, finish, k, (0.0, 10.0), c, 1.0, (0, 0))
                .unwrap_or_else(|| panic!("no straight leg on a reach across {kt} kn"));
            let (miss, _) = replay_miss(&legs, pos, finish, 0, None, c, k, (0, 0));
            assert!(miss < 0.03 * 10.0 * METRES_PER_NM * k, "{kt} kn: misses by {miss:.0} m");
        }

        let spiky = Polar::parse(SPIKY_POLAR).unwrap();
        let input = open_water(&spiky, &wind, &cfg, START, 600);
        for b10 in 1400..=1750 {
            let brg = b10 as f64 / 10.0;
            let finish = at_brg(brg, 3.0);
            let d = 3.0 * METRES_PER_NM * k;
            for side in [90.0, -90.0] {
                for q in 1..=8 {
                    let c = ms(q as f64 * 0.25, brg + side);
                    if let Some(legs) = close(&input, pos, 0, None, finish, k, (0.0, 10.0), c, 1.0, (0, 0)) {
                        let (miss, water) = replay_miss(&legs, pos, finish, 0, None, c, k, (0, 0));
                        assert!(
                            miss <= 0.03 * d + 1e-4 * water + 2.0,
                            "{brg}°, {} kn on the {side}: a leg that misses by {miss:.0} m",
                            q as f64 * 0.25
                        );
                    }
                }
            }
        }
    }


}
