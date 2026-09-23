//! The navigability raster and the search over it.
//!
//! §6.7's three spatial structures collapse to two here: the boolean raster
//! and the distance-to-hazard field, both on one grid in global Mercator
//! metres. The spec's third structure — an R-tree for segment-vs-obstacle
//! tests — is deliberately not built: the raster *is* the quilt (finer charts
//! overwrite coarser inside their own coverage), and a segment test against a
//! separate feature index would disagree with it exactly where quilting
//! matters. Segments are tested against the raster by supercover traversal,
//! which cannot miss a cell the rasterizer marked. Rasterization itself is
//! conservative: triangle interiors by centre-in-triangle, plus every
//! triangle *edge* stamped as a segment, so a sliver thinner than a cell
//! still blocks the cells it passes through.
//!
//! Distances are chamfer 3-4 — the spec says start there and MEASURE; the
//! build logs its cost so the Felzenszwalb upgrade is a decision, not a hunch.

/// One routing grid.
pub struct Grid {
    /// Mercator metres of the south-west corner of cell (0,0).
    pub origin: [f64; 2],
    /// Metres per cell.
    pub res: f64,
    pub w: usize,
    pub h: usize,
    /// 1 = hard obstacle.
    blocked: Vec<u8>,
    /// Additive soft-cost units (0..=255) per cell.
    soft: Vec<u8>,
    /// Metres to the nearest blocked cell, after [`Grid::finalize`].
    dist: Vec<f32>,
    /// Some chart's coverage reaches this cell. Outside every chart the
    /// grid is closed, and saying *why* an end cannot be reached ("outside
    /// your charts") needs to tell that apart from land.
    covered: Vec<bool>,
}

/// Cap on total cells: past this the grid coarsens itself rather than eating
/// gigabytes. 12M cells is about 200 MB with the search's own arrays —
/// within a Raspberry Pi 5's means, and seconds to fill.
const MAX_CELLS: usize = 12_000_000;

impl Grid {
    /// A grid covering `min..max` (Mercator metres) at `res`, coarsened if the
    /// area demands it.
    pub fn new(min: [f64; 2], max: [f64; 2], res_m: f64) -> Self {
        let span = [max[0] - min[0], max[1] - min[1]];
        let mut res = res_m.max(1.0);
        loop {
            let w = (span[0] / res).ceil() as usize + 1;
            let h = (span[1] / res).ceil() as usize + 1;
            if w * h <= MAX_CELLS {
                if res != res_m {
                    log::warn!(
                        "routing grid coarsened {res_m:.0} m → {res:.0} m to fit {w}×{h}"
                    );
                }
                return Self {
                    origin: min,
                    res,
                    w,
                    h,
                    blocked: vec![0; w * h],
                    soft: vec![0; w * h],
                    dist: Vec::new(),
                    covered: vec![false; w * h],
                };
            }
            res *= 1.5;
        }
    }

    /// Mark every cell as an obstacle: the start state of a grid on which
    /// only charted coverage is water.
    pub fn close_all(&mut self) {
        self.blocked.fill(1);
    }

    #[inline]
    fn idx(&self, x: usize, y: usize) -> usize {
        y * self.w + x
    }

    /// The cell containing a Mercator position, if it is on the grid.
    pub fn cell_of(&self, p: [f64; 2]) -> Option<(usize, usize)> {
        let x = ((p[0] - self.origin[0]) / self.res).floor();
        let y = ((p[1] - self.origin[1]) / self.res).floor();
        (x >= 0.0 && y >= 0.0 && (x as usize) < self.w && (y as usize) < self.h)
            .then(|| (x as usize, y as usize))
    }

    /// The Mercator centre of a cell.
    pub fn centre(&self, x: usize, y: usize) -> [f64; 2] {
        [
            self.origin[0] + (x as f64 + 0.5) * self.res,
            self.origin[1] + (y as f64 + 0.5) * self.res,
        ]
    }

    pub fn is_blocked(&self, x: usize, y: usize) -> bool {
        self.blocked[self.idx(x, y)] != 0
    }

    /// Metres to the nearest hard obstacle. Valid after [`Grid::finalize`].
    pub fn hazard_distance(&self, x: usize, y: usize) -> f64 {
        self.dist[self.idx(x, y)] as f64
    }

    pub fn soft_cost(&self, x: usize, y: usize) -> f64 {
        self.soft[self.idx(x, y)] as f64 / 255.0
    }

    fn mark(&mut self, x: i64, y: i64, hard: bool) {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h {
            return;
        }
        let i = self.idx(x as usize, y as usize);
        if hard {
            self.blocked[i] = 1;
        } else {
            self.soft[i] = self.soft[i].saturating_add(128);
        }
    }

    /// Clear a cell back to open water — the quilting eraser a finer chart
    /// runs over its own coverage before stamping its own truth.
    fn clear_cell(&mut self, x: i64, y: i64) {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h {
            return;
        }
        let i = self.idx(x as usize, y as usize);
        self.blocked[i] = 0;
        self.soft[i] = 0;
        self.covered[i] = true;
    }

    /// Draw the grid as a PNG, for seeing why a route goes where it goes:
    /// grey uncharted, black unsafe, orange soft cost, blue deepening with
    /// distance from hazards, and `path` (Mercator) in red. Downsampled to
    /// at most `max_px` on the long side.
    pub fn debug_png(&self, path: &[[f64; 2]], out: &std::path::Path, max_px: usize) {
        let step = (self.w.max(self.h) / max_px.max(1)).max(1);
        let (iw, ih) = (self.w / step, self.h / step);
        let mut img = image::RgbImage::new(iw as u32, ih as u32);
        for py in 0..ih {
            for px in 0..iw {
                let (x, y) = (px * step, self.h - 1 - py * step);
                let i = self.idx(x, y);
                let c = if !self.covered[i] && self.blocked[i] != 0 {
                    [110, 110, 110]
                } else if self.blocked[i] != 0 {
                    [20, 20, 20]
                } else if self.soft[i] != 0 {
                    [240, 160, 60]
                } else {
                    let d = self.dist.get(i).copied().unwrap_or(0.0) as f64;
                    let f = (d / 2_000.0).min(1.0);
                    [(230.0 - 150.0 * f) as u8, (240.0 - 110.0 * f) as u8, 255]
                };
                img.put_pixel(px as u32, py as u32, image::Rgb(c));
            }
        }
        for w in path.windows(2) {
            let n = 400;
            for i in 0..=n {
                let t = i as f64 / n as f64;
                let p = [w[0][0] + (w[1][0] - w[0][0]) * t, w[0][1] + (w[1][1] - w[0][1]) * t];
                if let Some((x, y)) = self.cell_of(p) {
                    let (px, py) = (x / step, (self.h - 1 - y) / step);
                    if px < iw && py < ih {
                        img.put_pixel(px as u32, py as u32, image::Rgb([230, 30, 30]));
                    }
                }
            }
        }
        if let Err(e) = img.save(out) {
            log::warn!("could not write {}: {e}", out.display());
        }
    }

    /// Does any chart's coverage reach this cell?
    pub fn is_covered(&self, x: usize, y: usize) -> bool {
        self.covered[self.idx(x, y)]
    }

    /// Every cell reachable from `from` through water that is not blocked,
    /// 8-connected without cutting blocked corners — the same moves the
    /// search makes, with no offing at all. A marina basin shut in by fixed
    /// bridges is its own component, and an end snapped into it can never
    /// be reached from outside.
    pub fn component(&self, from: (usize, usize)) -> Vec<bool> {
        let mut seen = vec![false; self.w * self.h];
        if self.is_blocked(from.0, from.1) {
            return seen;
        }
        let mut stack = vec![from];
        seen[self.idx(from.0, from.1)] = true;
        while let Some((x, y)) = stack.pop() {
            for (dx, dy) in [(1i64, 0i64), (-1, 0), (0, 1), (0, -1), (1, 1), (1, -1), (-1, 1), (-1, -1)] {
                let (nx, ny) = (x as i64 + dx, y as i64 + dy);
                if nx < 0 || ny < 0 || nx as usize >= self.w || ny as usize >= self.h {
                    continue;
                }
                let (nx, ny) = (nx as usize, ny as usize);
                let i = self.idx(nx, ny);
                if seen[i] || self.is_blocked(nx, ny) {
                    continue;
                }
                if dx != 0 && dy != 0
                    && (self.is_blocked(nx, y) || self.is_blocked(x, ny))
                {
                    continue;
                }
                seen[i] = true;
                stack.push((nx, ny));
            }
        }
        seen
    }

    /// Stamp a filled triangle (Mercator metres): interior by cell centre,
    /// plus all three edges, so slivers cannot slip between centres.
    pub fn stamp_triangle(&mut self, t: [[f64; 2]; 3], hard: bool) {
        self.triangle_cells(t, |g, x, y| g.mark(x, y, hard));
        self.stamp_segment(t[0], t[1], hard);
        self.stamp_segment(t[1], t[2], hard);
        self.stamp_segment(t[2], t[0], hard);
    }

    /// Erase a filled triangle, edges included.
    pub fn clear_triangle(&mut self, t: [[f64; 2]; 3]) {
        self.triangle_cells(t, |g, x, y| g.clear_cell(x, y));
        for (a, b) in [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])] {
            self.walk_segment(a, b, |g, x, y| g.clear_cell(x, y));
        }
    }

    fn triangle_cells(&mut self, t: [[f64; 2]; 3], mut f: impl FnMut(&mut Self, i64, i64)) {
        let min_x = t.iter().map(|p| p[0]).fold(f64::MAX, f64::min);
        let max_x = t.iter().map(|p| p[0]).fold(f64::MIN, f64::max);
        let min_y = t.iter().map(|p| p[1]).fold(f64::MAX, f64::min);
        let max_y = t.iter().map(|p| p[1]).fold(f64::MIN, f64::max);
        // Clamped to the grid: an overview chart's land triangle can be
        // hundreds of kilometres across, and walking its whole bounding box
        // cell by cell — almost all of it off the grid — took longer than
        // everything else in the plan put together.
        let x0 = (((min_x - self.origin[0]) / self.res).floor() as i64).max(0);
        let x1 = (((max_x - self.origin[0]) / self.res).ceil() as i64).min(self.w as i64 - 1);
        let y0 = (((min_y - self.origin[1]) / self.res).floor() as i64).max(0);
        let y1 = (((max_y - self.origin[1]) / self.res).ceil() as i64).min(self.h as i64 - 1);
        if x0 > x1 || y0 > y1 {
            return;
        }
        for y in y0..=y1 {
            for x in x0..=x1 {
                let c = [
                    self.origin[0] + (x as f64 + 0.5) * self.res,
                    self.origin[1] + (y as f64 + 0.5) * self.res,
                ];
                if point_in_triangle(c, t) {
                    f(self, x, y);
                }
            }
        }
    }

    /// Stamp a line segment: every cell the segment passes through.
    pub fn stamp_segment(&mut self, a: [f64; 2], b: [f64; 2], hard: bool) {
        self.walk_segment(a, b, |g, x, y| g.mark(x, y, hard));
    }

    pub fn stamp_point(&mut self, p: [f64; 2], hard: bool) {
        let x = ((p[0] - self.origin[0]) / self.res).floor() as i64;
        let y = ((p[1] - self.origin[1]) / self.res).floor() as i64;
        self.mark(x, y, hard);
    }

    /// Supercover walk: visit every cell a segment touches, no diagonal gaps.
    fn walk_segment(
        &mut self,
        a: [f64; 2],
        b: [f64; 2],
        mut f: impl FnMut(&mut Self, i64, i64),
    ) {
        // Only the part on the grid (plus a cell) is walked: an overview
        // chart's coastline edge can be a hundred kilometres long.
        let lo = [self.origin[0] - self.res, self.origin[1] - self.res];
        let hi = [
            self.origin[0] + (self.w as f64 + 1.0) * self.res,
            self.origin[1] + (self.h as f64 + 1.0) * self.res,
        ];
        let Some((a, b)) = clip_segment(a, b, lo, hi) else {
            return;
        };
        let steps = ((b[0] - a[0]).hypot(b[1] - a[1]) / (self.res * 0.45)).ceil() as usize;
        let steps = steps.max(1);
        let mut last: Option<(i64, i64)> = None;
        for i in 0..=steps {
            let t = i as f64 / steps as f64;
            let p = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
            let x = ((p[0] - self.origin[0]) / self.res).floor() as i64;
            let y = ((p[1] - self.origin[1]) / self.res).floor() as i64;
            if last == Some((x, y)) {
                continue;
            }
            // Half-step sampling can still hop a corner; fill the elbow so
            // the cover is a true supercover.
            if let Some((lx, ly)) = last {
                if (x - lx).abs() == 1 && (y - ly).abs() == 1 {
                    f(self, lx, y);
                }
            }
            f(self, x, y);
            last = Some((x, y));
        }
    }

    /// Build the distance-to-hazard field. Chamfer 3-4: two passes, ~2 %
    /// error, which against a 370 m offing is metres.
    pub fn finalize(&mut self) {
        let start = std::time::Instant::now();
        let orth = self.res as f32;
        let diag = self.res as f32 * std::f32::consts::SQRT_2;
        let big = f32::MAX / 4.0;
        self.dist = self
            .blocked
            .iter()
            .map(|&b| if b != 0 { 0.0 } else { big })
            .collect();

        let (w, h) = (self.w, self.h);
        // Forward: SW → NE.
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let mut d = self.dist[i];
                if x > 0 {
                    d = d.min(self.dist[i - 1] + orth);
                }
                if y > 0 {
                    d = d.min(self.dist[i - w] + orth);
                    if x > 0 {
                        d = d.min(self.dist[i - w - 1] + diag);
                    }
                    if x + 1 < w {
                        d = d.min(self.dist[i - w + 1] + diag);
                    }
                }
                self.dist[i] = d;
            }
        }
        // Backward: NE → SW.
        for y in (0..h).rev() {
            for x in (0..w).rev() {
                let i = y * w + x;
                let mut d = self.dist[i];
                if x + 1 < w {
                    d = d.min(self.dist[i + 1] + orth);
                }
                if y + 1 < h {
                    d = d.min(self.dist[i + w] + orth);
                    if x + 1 < w {
                        d = d.min(self.dist[i + w + 1] + diag);
                    }
                    if x > 0 {
                        d = d.min(self.dist[i + w - 1] + diag);
                    }
                }
                self.dist[i] = d;
            }
        }
        // The spec says MEASURE this, so it is measured, every build.
        log::info!(
            "routing distance field: {}×{} cells in {} ms",
            w,
            h,
            start.elapsed().as_millis()
        );
    }

    /// Is this cell somewhere a boat may be?
    pub fn passable(&self, x: usize, y: usize, offing_min_m: f64) -> bool {
        !self.is_blocked(x, y) && self.hazard_distance(x, y) >= offing_min_m
    }

    /// Is this cell somewhere a boat may be, under a search's rules — the
    /// hard offing, faded near the endpoints?
    pub fn passable_by(&self, x: usize, y: usize, params: &SearchParams) -> bool {
        !self.is_blocked(x, y)
            && self.hazard_distance(x, y) >= params.floor_at(self.centre(x, y))
    }

    /// [`segment_clear`](Self::segment_clear) under a search's rules.
    pub fn segment_clear_by(&self, a: [f64; 2], b: [f64; 2], params: &SearchParams) -> bool {
        self.segment_clear_with(a, b, |g, x, y| g.passable_by(x, y, params))
    }

    /// Does the segment pass through a soft area — a caution area, a
    /// restricted area short of prohibition, an exercise area? The route may,
    /// but the skipper should be told.
    pub fn segment_touches_soft(&self, a: [f64; 2], b: [f64; 2]) -> bool {
        !self.segment_clear_with(a, b, |g, x, y| g.soft[g.idx(x, y)] == 0)
    }

    /// Is the straight segment between two Mercator points clear, offing
    /// included? The same supercover the rasterizer used, so the answer
    /// cannot disagree with the stamping.
    pub fn segment_clear(&self, a: [f64; 2], b: [f64; 2], offing_min_m: f64) -> bool {
        self.segment_clear_with(a, b, |g, x, y| g.passable(x, y, offing_min_m))
    }

    fn segment_clear_with(
        &self,
        a: [f64; 2],
        b: [f64; 2],
        ok: impl Fn(&Self, usize, usize) -> bool,
    ) -> bool {
        let mut clear = true;
        // Walk on a clone-free path: reimplement the walk read-only.
        let steps = ((b[0] - a[0]).hypot(b[1] - a[1]) / (self.res * 0.45)).ceil() as usize;
        let steps = steps.max(1);
        let mut last: Option<(i64, i64)> = None;
        for i in 0..=steps {
            let t = i as f64 / steps as f64;
            let p = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
            let x = ((p[0] - self.origin[0]) / self.res).floor() as i64;
            let y = ((p[1] - self.origin[1]) / self.res).floor() as i64;
            if last == Some((x, y)) {
                continue;
            }
            let mut check = |cx: i64, cy: i64| {
                if cx < 0 || cy < 0 || cx as usize >= self.w || cy as usize >= self.h {
                    clear = false;
                    return;
                }
                if !ok(self, cx as usize, cy as usize) {
                    clear = false;
                }
            };
            if let Some((lx, ly)) = last {
                if (x - lx).abs() == 1 && (y - ly).abs() == 1 {
                    check(lx, y);
                }
            }
            check(x, y);
            if !clear {
                return false;
            }
            last = Some((x, y));
        }
        true
    }
}

/// Liang–Barsky: the part of segment `a`–`b` inside the box, if any.
fn clip_segment(
    a: [f64; 2],
    b: [f64; 2],
    lo: [f64; 2],
    hi: [f64; 2],
) -> Option<([f64; 2], [f64; 2])> {
    let d = [b[0] - a[0], b[1] - a[1]];
    let (mut t0, mut t1) = (0.0f64, 1.0f64);
    for axis in 0..2 {
        for (p, q) in [(-d[axis], a[axis] - lo[axis]), (d[axis], hi[axis] - a[axis])] {
            if p == 0.0 {
                if q < 0.0 {
                    return None;
                }
            } else {
                let r = q / p;
                if p < 0.0 {
                    t0 = t0.max(r);
                } else {
                    t1 = t1.min(r);
                }
            }
        }
    }
    (t0 <= t1).then(|| {
        (
            [a[0] + d[0] * t0, a[1] + d[1] * t0],
            [a[0] + d[0] * t1, a[1] + d[1] * t1],
        )
    })
}

fn point_in_triangle(p: [f64; 2], t: [[f64; 2]; 3]) -> bool {
    let sign = |a: [f64; 2], b: [f64; 2], c: [f64; 2]| {
        (a[0] - c[0]) * (b[1] - c[1]) - (b[0] - c[0]) * (a[1] - c[1])
    };
    let d1 = sign(p, t[0], t[1]);
    let d2 = sign(p, t[1], t[2]);
    let d3 = sign(p, t[2], t[0]);
    let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(has_neg && has_pos)
}

/// Why a search failed.
#[derive(Debug, PartialEq)]
pub enum SearchError {
    /// No passable path exists between the endpoints at this offing.
    Unreachable,
    /// An endpoint was off the grid entirely.
    OffGrid,
    /// The user cancelled the plan.
    Cancelled,
}

/// How the search weighs comfort against distance — and where it may go.
///
/// Three distances from unsafe water, all Mercator-plane metres:
/// - `offing_hard_m`: never closer. Small — a cell or so — so a dredged
///   channel or a sound a few hundred metres wide stays open;
/// - `offing_min_m`: the offing a skipper wants. Closer costs steeply (up
///   to four times the distance), so the route keeps it wherever the chart
///   allows and gives it up only where the water itself is narrower;
/// - `offing_soft_m`: sea room is cheap; below this the cost rises gently.
///
/// A berth is inside every offing by definition. Within `taper_m` of an
/// endpoint the floor and the penalties fade to nothing, so the route can
/// leave a harbour through its own entrance instead of failing, or being
/// teleported to open water it does not connect to.
pub struct SearchParams {
    pub offing_hard_m: f64,
    pub offing_min_m: f64,
    pub offing_soft_m: f64,
    /// The start and finish, Mercator.
    pub ends: Vec<[f64; 2]>,
    pub taper_m: f64,
}

impl SearchParams {
    /// One fixed offing, no fading: the old behaviour, and the analytic
    /// tests'.
    pub fn fixed(offing_min_m: f64, offing_soft_m: f64) -> Self {
        Self {
            offing_hard_m: offing_min_m,
            offing_min_m,
            offing_soft_m,
            ends: Vec::new(),
            taper_m: 0.0,
        }
    }

    /// 0 at an endpoint, rising to 1 at `taper_m` from it.
    pub fn fade(&self, p: [f64; 2]) -> f64 {
        if self.taper_m <= 0.0 {
            return 1.0;
        }
        self.ends
            .iter()
            .map(|e| ((p[0] - e[0]).hypot(p[1] - e[1]) / self.taper_m).min(1.0))
            .fold(1.0, f64::min)
    }

    /// The hard offing at a place.
    pub fn floor_at(&self, p: [f64; 2]) -> f64 {
        self.offing_hard_m * self.fade(p)
    }

    /// The cost of a metre of water `d` from the nearest hazard, as a
    /// multiplier ≥ 1 (so Euclidean distance stays an admissible heuristic).
    fn multiplier(&self, d: f64, fade: f64) -> f64 {
        let pen = if d < self.offing_min_m && self.offing_min_m > self.offing_hard_m {
            1.0 + 3.0 * ((self.offing_min_m - d) / (self.offing_min_m - self.offing_hard_m))
                .clamp(0.0, 1.0)
        } else if d < self.offing_soft_m && self.offing_soft_m > self.offing_min_m {
            ((self.offing_soft_m - d) / (self.offing_soft_m - self.offing_min_m)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        1.0 + pen * fade
    }
}

/// A* over the grid, 8-connected.
///
/// Costs are step length times a multiplier ≥ 1 (offing discomfort, soft
/// areas), so plain Euclidean distance stays an admissible heuristic and the
/// first arrival at the goal is optimal.
pub fn find_path(
    grid: &Grid,
    start: (usize, usize),
    goal: (usize, usize),
    params: &SearchParams,
) -> Result<Vec<(usize, usize)>, SearchError> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    let (w, h) = (grid.w, grid.h);
    let index = |c: (usize, usize)| c.1 * w + c.0;
    let mut g_score = vec![f32::MAX; w * h];
    let mut parent = vec![u32::MAX; w * h];
    let mut heap: BinaryHeap<Reverse<(u64, u32)>> = BinaryHeap::new();

    let hcost = |c: (usize, usize)| {
        let dx = c.0 as f64 - goal.0 as f64;
        let dy = c.1 as f64 - goal.1 as f64;
        (dx.hypot(dy) * grid.res) as f32
    };
    // f-scores go through a u64 key because f32 is not Ord; millimetre
    // quantization is far below anything the grid resolves.
    let key = |f: f32| (f as f64 * 1000.0) as u64;

    g_score[index(start)] = 0.0;
    heap.push(Reverse((key(hcost(start)), index(start) as u32)));

    const NEIGHBOURS: [(i64, i64); 8] = [
        (1, 0),
        (-1, 0),
        (0, 1),
        (0, -1),
        (1, 1),
        (1, -1),
        (-1, 1),
        (-1, -1),
    ];

    let mut popped = 0u32;
    while let Some(Reverse((f_key, current))) = heap.pop() {
        // A cancelled plan stops here too, rather than finishing a search
        // nobody will read. Checked now and then: it is a thread-local read.
        popped = popped.wrapping_add(1);
        if popped % 65_536 == 0 && crate::nav::isochrone::this_plan_cancelled() {
            return Err(SearchError::Cancelled);
        }
        let c = (current as usize % w, current as usize / w);
        // A node is pushed again each time a cheaper way to it is found; the
        // older entries are stale and would only re-expand it.
        if f_key != key(g_score[current as usize] + hcost(c)) {
            continue;
        }
        if c == goal {
            // Walk parents back.
            let mut path = vec![c];
            let mut i = current;
            while parent[i as usize] != u32::MAX {
                i = parent[i as usize];
                path.push((i as usize % w, i as usize / w));
            }
            path.reverse();
            return Ok(path);
        }
        let g_here = g_score[current as usize];
        for (dx, dy) in NEIGHBOURS {
            let nx = c.0 as i64 + dx;
            let ny = c.1 as i64 + dy;
            if nx < 0 || ny < 0 || nx as usize >= w || ny as usize >= h {
                continue;
            }
            let n = (nx as usize, ny as usize);
            if !grid.passable_by(n.0, n.1, params) {
                continue;
            }
            // Diagonal moves must not cut a blocked corner.
            if dx != 0 && dy != 0 {
                let a = ((c.0 as i64 + dx) as usize, c.1);
                let b = (c.0, (c.1 as i64 + dy) as usize);
                if !grid.passable_by(a.0, a.1, params) || !grid.passable_by(b.0, b.1, params) {
                    continue;
                }
            }
            let step = if dx != 0 && dy != 0 {
                grid.res * std::f64::consts::SQRT_2
            } else {
                grid.res
            };
            let d = grid.hazard_distance(n.0, n.1);
            let fade = params.fade(grid.centre(n.0, n.1));
            let mult = params.multiplier(d, fade) + grid.soft_cost(n.0, n.1);
            let tentative = g_here + (step * mult) as f32;
            let ni = index(n);
            if tentative < g_score[ni] {
                g_score[ni] = tentative;
                parent[ni] = current;
                heap.push(Reverse((key(tentative + hcost(n)), ni as u32)));
            }
        }
    }
    Err(SearchError::Unreachable)
}

/// The nearest passable cell to a point, spiralling outward — §7.7's nudge
/// for an endpoint on the hard (a pontoon, a quay, a charted shoal). With
/// the endpoint fading in `params`, any water off the hard will do. `None`
/// within `max_m` means the endpoint is truly buried.
pub fn nudge(
    grid: &Grid,
    from: (usize, usize),
    params: &SearchParams,
    max_m: f64,
) -> Option<(usize, usize)> {
    nudge_into(grid, from, params, max_m, None)
}

/// [`nudge`], but only to a cell inside `component` when one is given.
pub fn nudge_into(
    grid: &Grid,
    from: (usize, usize),
    params: &SearchParams,
    max_m: f64,
    component: Option<&[bool]>,
) -> Option<(usize, usize)> {
    let ok = |x: usize, y: usize| {
        grid.passable_by(x, y, params) && component.is_none_or(|c| c[y * grid.w + x])
    };
    if ok(from.0, from.1) {
        return Some(from);
    }
    let max_r = (max_m / grid.res).ceil() as i64;
    for r in 1..=max_r {
        let mut best: Option<((usize, usize), i64)> = None;
        for dy in -r..=r {
            for dx in -r..=r {
                if dx.abs().max(dy.abs()) != r {
                    continue; // ring only
                }
                let x = from.0 as i64 + dx;
                let y = from.1 as i64 + dy;
                if x < 0 || y < 0 || x as usize >= grid.w || y as usize >= grid.h {
                    continue;
                }
                if ok(x as usize, y as usize) {
                    let d2 = dx * dx + dy * dy;
                    if best.map(|(_, b)| d2 < b).unwrap_or(true) {
                        best = Some(((x as usize, y as usize), d2));
                    }
                }
            }
        }
        if let Some((cell, _)) = best {
            return Some(cell);
        }
    }
    None
}

/// Pull the cell path taut: keep only the corners that matter, each surviving
/// leg verified clear by the same supercover the rasterizer used.
pub fn string_pull(grid: &Grid, path: &[(usize, usize)], params: &SearchParams) -> Vec<[f64; 2]> {
    if path.is_empty() {
        return Vec::new();
    }
    let pts: Vec<[f64; 2]> = path.iter().map(|&(x, y)| grid.centre(x, y)).collect();
    let mut out = vec![pts[0]];
    let mut i = 0;
    while i + 1 < pts.len() {
        // Furthest j visible from i.
        let mut j = pts.len() - 1;
        while j > i + 1 && !grid.segment_clear_by(pts[i], pts[j], params) {
            j -= 1;
        }
        out.push(pts[j]);
        i = j;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 10 km × 10 km of open water at 50 m.
    fn open() -> Grid {
        Grid::new([0.0, 0.0], [10_000.0, 10_000.0], 50.0)
    }

    fn params() -> SearchParams {
        SearchParams::fixed(150.0, 400.0)
    }

    /// A square island in the middle, as two triangles.
    fn island(grid: &mut Grid) {
        let (a, b, c, d) = (
            [4_000.0, 4_000.0],
            [6_000.0, 4_000.0],
            [6_000.0, 6_000.0],
            [4_000.0, 6_000.0],
        );
        grid.stamp_triangle([a, b, c], true);
        grid.stamp_triangle([a, c, d], true);
        grid.finalize();
    }

    #[test]
    fn a_route_goes_around_an_island_not_through_it() {
        let mut grid = open();
        island(&mut grid);
        let start = grid.cell_of([500.0, 5_000.0]).unwrap();
        let goal = grid.cell_of([9_500.0, 5_000.0]).unwrap();
        let path = find_path(&grid, start, goal, &params()).expect("a way round exists");

        // The DoD: never inside the hard, never inside the offing.
        for &(x, y) in &path {
            assert!(grid.passable(x, y, params().offing_min_m), "cell ({x},{y})");
        }
        // And the pulled legs hold the same guarantee.
        let pulled = string_pull(&grid, &path, &params());
        assert!(pulled.len() >= 3, "must dodge: {} points", pulled.len());
        for w in pulled.windows(2) {
            assert!(grid.segment_clear(w[0], w[1], params().offing_min_m));
        }
        // It went around: some point clears the island's band vertically.
        assert!(pulled
            .iter()
            .any(|p| p[1] < 3_800.0 || p[1] > 6_200.0));
    }

    #[test]
    fn open_water_pulls_to_a_single_leg() {
        let mut grid = open();
        grid.finalize();
        let start = grid.cell_of([500.0, 500.0]).unwrap();
        let goal = grid.cell_of([9_500.0, 8_500.0]).unwrap();
        let path = find_path(&grid, start, goal, &params()).unwrap();
        let pulled = string_pull(&grid, &path, &params());
        assert_eq!(pulled.len(), 2, "no obstacles, no corners: {pulled:?}");
    }

    #[test]
    fn a_wall_with_no_gap_is_unreachable_not_a_hang() {
        let mut grid = open();
        // A full-height wall.
        grid.stamp_segment([5_000.0, -100.0], [5_000.0, 10_100.0], true);
        grid.finalize();
        let start = grid.cell_of([1_000.0, 5_000.0]).unwrap();
        let goal = grid.cell_of([9_000.0, 5_000.0]).unwrap();
        assert_eq!(
            find_path(&grid, start, goal, &params()),
            Err(SearchError::Unreachable)
        );
    }

    #[test]
    fn the_supercover_leaves_no_diagonal_gap() {
        let mut grid = Grid::new([0.0, 0.0], [1_000.0, 1_000.0], 50.0);
        // A thin diagonal breakwater.
        grid.stamp_segment([0.0, 0.0], [1_000.0, 1_000.0], true);
        grid.finalize();
        // No 8-connected path from below the line to above it may exist
        // without offing — the wall must be watertight against diagonal
        // slipping.
        let start = grid.cell_of([900.0, 100.0]).unwrap();
        let goal = grid.cell_of([100.0, 900.0]).unwrap();
        let p = SearchParams::fixed(0.0, 0.0);
        assert_eq!(find_path(&grid, start, goal, &p), Err(SearchError::Unreachable));
    }

    #[test]
    fn quilting_a_finer_chart_reopens_water_a_coarse_one_blocked() {
        let mut grid = open();
        // Coarse chart: generalized land covering the strait.
        let (a, b, c, d) = (
            [4_000.0, 0.0],
            [6_000.0, 0.0],
            [6_000.0, 10_000.0],
            [4_000.0, 10_000.0],
        );
        grid.stamp_triangle([a, b, c], true);
        grid.stamp_triangle([a, c, d], true);
        // The finer chart's coverage spans the whole coarse band east–west
        // and restates it as two shores with an 800 m channel between —
        // cutting the coarse "land" horizontally, the way a real survey opens
        // a sound an overview chart paints solid.
        let (ca, cb, cc, cd) = (
            [3_500.0, 3_000.0],
            [6_500.0, 3_000.0],
            [6_500.0, 7_000.0],
            [3_500.0, 7_000.0],
        );
        grid.clear_triangle([ca, cb, cc]);
        grid.clear_triangle([ca, cc, cd]);
        // South shore: y 3000..4600 across the old band.
        grid.stamp_triangle([[4_000.0, 3_000.0], [6_000.0, 3_000.0], [6_000.0, 4_600.0]], true);
        grid.stamp_triangle([[4_000.0, 3_000.0], [6_000.0, 4_600.0], [4_000.0, 4_600.0]], true);
        // North shore: y 5400..7000.
        grid.stamp_triangle([[4_000.0, 5_400.0], [6_000.0, 5_400.0], [6_000.0, 7_000.0]], true);
        grid.stamp_triangle([[4_000.0, 5_400.0], [6_000.0, 7_000.0], [4_000.0, 7_000.0]], true);
        grid.finalize();

        let start = grid.cell_of([1_000.0, 5_000.0]).unwrap();
        let goal = grid.cell_of([9_000.0, 5_000.0]).unwrap();
        // Through the channel with a modest offing.
        let p = SearchParams::fixed(100.0, 200.0);
        let path = find_path(&grid, start, goal, &p).expect("the fine chart's channel is open");
        for &(x, y) in &path {
            assert!(grid.passable(x, y, p.offing_min_m));
        }
    }

    #[test]
    fn nudging_finds_the_water_beside_a_marina() {
        let mut grid = open();
        island(&mut grid);
        // A start inside the island.
        let buried = grid.cell_of([5_000.0, 5_000.0]).unwrap();
        let freed = nudge(&grid, buried, &params(), 3_000.0)
            .expect("open water within 3 km");
        assert!(grid.passable(freed.0, freed.1, params().offing_min_m));
        // And a hopeless case gives None, not a spin.
        assert!(nudge(&grid, buried, &params(), 200.0).is_none());
    }

    /// Land as an axis-aligned block.
    fn block(grid: &mut Grid, x0: f64, y0: f64, x1: f64, y1: f64) {
        grid.stamp_triangle([[x0, y0], [x1, y0], [x1, y1]], true);
        grid.stamp_triangle([[x0, y0], [x1, y1], [x0, y1]], true);
    }

    /// The harbour the old router could not leave: a basin with a 150 m
    /// entrance, well inside any sensible offing. Tapered at the ends, the
    /// route leaves through the entrance; far from the ends the offing is
    /// still kept.
    #[test]
    fn a_marina_with_a_narrow_entrance_can_be_left() {
        let mut grid = Grid::new([0.0, 0.0], [10_000.0, 10_000.0], 25.0);
        // Basin walls round (1000..3000, 1000..3000), entrance on the east
        // side at y 1925..2075.
        block(&mut grid, 800.0, 800.0, 3200.0, 1000.0);
        block(&mut grid, 800.0, 3000.0, 3200.0, 3200.0);
        block(&mut grid, 800.0, 800.0, 1000.0, 3200.0);
        block(&mut grid, 3000.0, 800.0, 3200.0, 1925.0);
        block(&mut grid, 3000.0, 2075.0, 3200.0, 3200.0);
        grid.finalize();
        let (a, b) = ([2000.0, 2000.0], [9000.0, 9000.0]);
        let start = grid.cell_of(a).unwrap();
        let goal = grid.cell_of(b).unwrap();

        // A fixed 370 m offing: the entrance is closed and the berth with it.
        assert!(find_path(&grid, start, goal, &SearchParams::fixed(370.0, 900.0)).is_err());

        let params = SearchParams {
            offing_hard_m: 20.0,
            offing_min_m: 370.0,
            offing_soft_m: 900.0,
            ends: vec![a, b],
            taper_m: 1_850.0,
        };
        let path = find_path(&grid, start, goal, &params).expect("out through the entrance");
        let pulled = string_pull(&grid, &path, &params);
        for w in pulled.windows(2) {
            assert!(grid.segment_clear_by(w[0], w[1], &params));
        }
        // Away from the ends the preferred offing is kept where the water
        // allows it.
        for &(x, y) in &path {
            let p = grid.centre(x, y);
            if params.fade(p) >= 1.0 {
                assert!(grid.hazard_distance(x, y) >= 370.0, "cell ({x},{y}) hugs the shore");
            }
        }
    }

    /// Where the only water is a channel narrower than the preferred offing,
    /// the route uses it rather than calling the passage impossible.
    #[test]
    fn a_narrow_channel_is_used_when_it_is_the_only_way() {
        let mut grid = Grid::new([0.0, 0.0], [20_000.0, 10_000.0], 25.0);
        // A wall across the middle with a 200 m channel through it.
        block(&mut grid, 9_000.0, -100.0, 11_000.0, 4_900.0);
        block(&mut grid, 9_000.0, 5_100.0, 11_000.0, 10_100.0);
        grid.finalize();
        let (a, b) = ([2_000.0, 5_000.0], [18_000.0, 5_000.0]);
        let params = SearchParams {
            offing_hard_m: 20.0,
            offing_min_m: 370.0,
            offing_soft_m: 900.0,
            ends: vec![a, b],
            taper_m: 1_850.0,
        };
        let path = find_path(&grid, grid.cell_of(a).unwrap(), grid.cell_of(b).unwrap(), &params)
            .expect("through the channel");
        assert!(path.iter().all(|&(x, y)| !grid.is_blocked(x, y)));
    }

    /// A basin shut by a fixed bridge is its own water: an end snapped into
    /// it could never be reached, so the snap must look for connected water.
    #[test]
    fn an_end_snaps_to_water_that_connects() {
        let mut grid = Grid::new([0.0, 0.0], [10_000.0, 10_000.0], 25.0);
        // A closed basin (1000..2000)² next to the start point on land.
        block(&mut grid, 800.0, 800.0, 2200.0, 1000.0);
        block(&mut grid, 800.0, 2000.0, 2200.0, 2200.0);
        block(&mut grid, 800.0, 800.0, 1000.0, 2200.0);
        block(&mut grid, 2000.0, 800.0, 2200.0, 2200.0);
        // Land between the basin and the open sea, the start on it.
        block(&mut grid, 2200.0, 800.0, 2700.0, 2200.0);
        grid.finalize();
        let on_land = grid.cell_of([2150.0, 1500.0]).unwrap();
        let loose = SearchParams::fixed(0.0, 0.0);
        let sea = grid.component(grid.cell_of([9_000.0, 9_000.0]).unwrap());
        let snapped = nudge_into(&grid, on_land, &loose, 1_000.0, Some(&sea)).expect("the sea");
        assert!(sea[snapped.1 * grid.w + snapped.0]);
        // The plain nearest water is the closed basin — the old answer.
        let nearest = nudge(&grid, on_land, &loose, 1_000.0).unwrap();
        assert!(!sea[nearest.1 * grid.w + nearest.0]);
    }

    /// On a grid that starts closed, only charted coverage is water, and
    /// the grid can say which closed cells are merely uncharted.
    #[test]
    fn uncharted_water_is_closed_and_known_as_uncharted() {
        let mut grid = open();
        grid.close_all();
        grid.clear_triangle([[0.0, 0.0], [5_000.0, 0.0], [5_000.0, 10_000.0]]);
        grid.clear_triangle([[0.0, 0.0], [5_000.0, 10_000.0], [0.0, 10_000.0]]);
        grid.finalize();
        let (x, y) = grid.cell_of([8_000.0, 5_000.0]).unwrap();
        assert!(grid.is_blocked(x, y) && !grid.is_covered(x, y));
        let (x, y) = grid.cell_of([2_000.0, 5_000.0]).unwrap();
        assert!(!grid.is_blocked(x, y) && grid.is_covered(x, y));
    }

    #[test]
    fn segments_are_clipped_to_the_grid_before_they_are_walked() {
        let (lo, hi) = ([0.0, 0.0], [10.0, 10.0]);
        let (a, b) = clip_segment([-10.0, 5.0], [20.0, 5.0], lo, hi).unwrap();
        assert_eq!((a, b), ([0.0, 5.0], [10.0, 5.0]));
        assert!(clip_segment([-10.0, -10.0], [-5.0, 20.0], lo, hi).is_none());
    }

    #[test]
    fn soft_areas_are_avoided_when_avoidance_is_cheap() {
        let mut grid = open();
        // A soft band across the middle (an exercise area, say).
        let (a, b, c, d) = (
            [0.0, 4_500.0],
            [10_000.0, 4_500.0],
            [10_000.0, 5_500.0],
            [0.0, 5_500.0],
        );
        grid.stamp_triangle([a, b, c], false);
        grid.stamp_triangle([a, c, d], false);
        grid.finalize();

        // Crossing is unavoidable start-to-goal here, but the path through
        // should be near-perpendicular — minimal length inside the band.
        let start = grid.cell_of([5_000.0, 1_000.0]).unwrap();
        let goal = grid.cell_of([5_000.0, 9_000.0]).unwrap();
        let path = find_path(&grid, start, goal, &params()).unwrap();
        let inside = path
            .iter()
            .filter(|&&(x, y)| {
                let c = grid.centre(x, y);
                (4_500.0..5_500.0).contains(&c[1])
            })
            .count() as f64;
        let band_cells = 1_000.0 / grid.res;
        assert!(
            inside <= band_cells * 1.6,
            "crossed at {inside} cells for a {band_cells}-cell band"
        );
    }
}
