//! One mesh instead of a thousand shapes.
//!
//! The weather overlay is the densest thing this interface draws: a wind barb
//! every 64 points is five hundred barbs on a 1080p screen, each of which is a
//! shaft, up to five feathers and a pennant or two. Handed to egui one shape
//! at a time, that is some two and a half thousand `Shape`s per frame, and
//! egui pays for every one of them separately — a `Vec` allocated for its
//! path, the tessellator's scratch cleared and re-walked, a fresh run of
//! anti-aliasing arithmetic. On a desktop nobody notices. On a Pi 4, which is
//! what this plotter runs on, it was the better part of a frame.
//!
//! So the overlay builds its own mesh. The arithmetic is the same arithmetic
//! egui would have done — the same one-pixel feather, the same premultiplied
//! transparent rim — but it happens once into one growing vertex buffer with
//! no allocation per shape and no dispatch through the tessellator at all.
//! What egui receives is a single [`egui::Shape::Mesh`], which it forwards to
//! the GPU untouched.
//!
//! Anti-aliasing is kept because the overlay is nothing but thin diagonal
//! lines, and thin diagonal lines are exactly what looks broken without it.
//! The one thing given up is the mitred join between two segments of a
//! polyline: segments here are butt-ended, which shows only where two nearly
//! opposed segments meet, and no curve the weather draws is that sharp.

use egui::layers::ShapeIdx;
use egui::{Color32, Context, Mesh, Painter, Pos2, Rect, Shape, Vec2};

/// Sides in a circle. Twelve is round enough at the two to seven points the
/// overlay ever draws one at.
const RING: usize = 12;

/// A run of overlay geometry, accumulated into one mesh.
pub struct Batch {
    mesh: Mesh,
    /// The width of the anti-aliasing rim, in the same logical points the
    /// caller draws in. egui feathers by one *physical* pixel, so on a
    /// doubled display the rim is half a point wide.
    feather: f32,
}

impl Batch {
    /// A batch that feathers the way `ctx` does, so batched geometry and
    /// anything still drawn as a shape sit at the same crispness.
    pub fn new(ctx: &Context) -> Self {
        Self::with_feather(1.0 / ctx.pixels_per_point().max(0.01))
    }

    pub fn with_feather(feather: f32) -> Self {
        Self {
            mesh: Mesh::default(),
            feather: feather.max(0.0),
        }
    }

    /// Room for roughly `shapes` line-sized pieces, taken in one allocation
    /// rather than a dozen doublings.
    pub fn reserve(&mut self, shapes: usize) {
        self.mesh.reserve_vertices(shapes * 8);
        self.mesh.reserve_triangles(shapes * 6);
    }

    pub fn is_empty(&self) -> bool {
        self.mesh.is_empty()
    }

    /// Hand the batch to egui. Nothing is drawn for an empty one — an empty
    /// mesh still costs a draw call.
    pub fn paint(self, painter: &Painter) {
        if !self.mesh.is_empty() {
            painter.add(Shape::Mesh(self.mesh));
        }
    }

    /// Book a place in the paint list before the geometry exists.
    ///
    /// The lanes are drawn geometry-and-label together, lane by lane, but the
    /// geometry all has to reach egui as one mesh. Reserving the slot first
    /// and filling it with [`paint_at`](Self::paint_at) last keeps the mesh
    /// underneath every label, which is where it was when each lane drew its
    /// own shapes in order.
    pub fn slot(painter: &Painter) -> ShapeIdx {
        painter.add(Shape::Noop)
    }

    /// Fill a slot booked by [`slot`](Self::slot). Must be given the same
    /// painter — the clip rectangle comes from this call, not from the
    /// booking.
    pub fn paint_at(self, painter: &Painter, idx: ShapeIdx) {
        if !self.mesh.is_empty() {
            painter.set(idx, Shape::Mesh(self.mesh));
        }
    }

    /// A straight line, feathered along its sides.
    ///
    /// A line thinner than the feather is drawn at the feather's width with
    /// its alpha scaled down instead, which is how egui keeps a hairline from
    /// flickering in and out as it crosses the pixel grid.
    pub fn line(&mut self, a: Pos2, b: Pos2, width: f32, colour: Color32) {
        if colour.a() == 0 || width <= 0.0 {
            return;
        }
        let d = b - a;
        let len = d.length();
        if len < 1e-6 {
            return;
        }
        let dir = d / len;
        let normal = Vec2::new(-dir.y, dir.x);
        let (inner, outer, colour) = self.rims(width, colour);
        if colour.a() == 0 {
            return;
        }
        let clear = Color32::TRANSPARENT;

        let base = self.mesh.vertices.len() as u32;
        for &p in &[a, b] {
            self.mesh.colored_vertex(p - normal * outer, clear);
            self.mesh.colored_vertex(p - normal * inner, colour);
            self.mesh.colored_vertex(p + normal * inner, colour);
            self.mesh.colored_vertex(p + normal * outer, clear);
        }
        for k in 0..3 {
            self.quad_indices(base + k, base + k + 1, base + k + 5, base + k + 4);
        }
    }

    /// A convex polygon, feathered around its rim: arrowheads, pennants and
    /// the dots that ride the curves.
    ///
    /// Points may be given either way round; the outward direction is settled
    /// against the centroid rather than trusted from the winding, because
    /// every caller here builds its shape from a bearing and half of those
    /// come out clockwise.
    pub fn convex(&mut self, points: &[Pos2], colour: Color32) {
        let n = points.len();
        if n < 3 || colour.a() == 0 {
            return;
        }
        let centroid = {
            let mut c = Vec2::ZERO;
            for p in points {
                c += p.to_vec2();
            }
            (c / n as f32).to_pos2()
        };
        // The bisector at each corner, scaled so the offset rim stays a
        // constant distance from both of its edges. Dividing by the squared
        // length is the standard mitre; it is capped because the tip of an
        // arrowhead is sharp enough to shoot the mitre off into the distance.
        let edge_normal = |from: Pos2, to: Pos2| {
            let d = to - from;
            let len = d.length();
            if len < 1e-6 {
                Vec2::ZERO
            } else {
                Vec2::new(-d.y, d.x) / len
            }
        };
        let half = self.feather * 0.5;
        let base = self.mesh.vertices.len() as u32;
        let mut flip = 0.0f32;
        for i in 0..n {
            let prev = points[(i + n - 1) % n];
            let next = points[(i + 1) % n];
            let mut normal = (edge_normal(prev, points[i]) + edge_normal(points[i], next)) * 0.5;
            let len_sq = normal.length_sq();
            if len_sq < 1e-6 {
                normal = Vec2::ZERO;
            } else {
                normal /= len_sq.max(0.25);
            }
            // Settle the outward sense once, on the first corner that has an
            // opinion, and hold it for the rest of the ring.
            if flip == 0.0 {
                let outward = normal.dot(points[i] - centroid);
                if outward < 0.0 {
                    flip = -1.0;
                } else if outward > 0.0 {
                    flip = 1.0;
                }
            }
            let dm = normal * (half * if flip < 0.0 { -1.0 } else { 1.0 });
            self.mesh.colored_vertex(points[i] - dm, colour);
            self.mesh.colored_vertex(points[i] + dm, Color32::TRANSPARENT);
        }
        // The fill, as a fan over the inner ring.
        for i in 2..n as u32 {
            self.mesh
                .add_triangle(base, base + (i - 1) * 2, base + i * 2);
        }
        // Then the rim, one quad per edge.
        for i in 0..n as u32 {
            let j = (i + 1) % n as u32;
            self.quad_indices(base + i * 2, base + j * 2, base + j * 2 + 1, base + i * 2 + 1);
        }
    }

    /// A grid of coloured quads sharing their corners.
    ///
    /// This is the weather fill — a Windy-style wash of colour over the
    /// chart. Shared corners are the whole point: a 44×29 grid is 1 300
    /// vertices this way and 5 100 as separate quads, and the GPU does the
    /// gradient between them for nothing. No feathering, because the cells
    /// abut and a rim on each would draw a lattice of seams.
    ///
    /// `at` and `colour` are both row-major, `cols × rows`. A corner with
    /// nothing to say leaves every cell that touches it undrawn — the edge of
    /// the forecast should be a hole, not an invented calm.
    pub fn grid(
        &mut self,
        at: &[[f32; 2]],
        cols: usize,
        rows: usize,
        colour: &[Option<Color32>],
    ) {
        if cols < 2 || rows < 2 || colour.len() < cols * rows || at.len() < cols * rows {
            return;
        }
        let base = self.mesh.vertices.len() as u32;
        self.mesh.reserve_vertices(cols * rows);
        self.mesh.reserve_triangles((cols - 1) * (rows - 1) * 2);
        for i in 0..cols * rows {
            self.mesh.colored_vertex(
                Pos2::new(at[i][0], at[i][1]),
                colour[i].unwrap_or(Color32::TRANSPARENT),
            );
        }
        let at = |c: usize, r: usize| base + (r * cols + c) as u32;
        for r in 0..rows - 1 {
            for c in 0..cols - 1 {
                // All four corners or none: a cell straddling the edge of the
                // data would otherwise fade to transparent black.
                if colour[r * cols + c].is_none()
                    || colour[r * cols + c + 1].is_none()
                    || colour[(r + 1) * cols + c].is_none()
                    || colour[(r + 1) * cols + c + 1].is_none()
                {
                    continue;
                }
                self.quad_indices(at(c, r), at(c + 1, r), at(c + 1, r + 1), at(c, r + 1));
            }
        }
    }

    /// An axis-aligned rectangle. No rim: its edges already lie on the pixel
    /// grid, and feathering them only makes a column of wind look blurred.
    pub fn rect(&mut self, r: Rect, colour: Color32) {
        if colour.a() == 0 || r.width() <= 0.0 || r.height() <= 0.0 {
            return;
        }
        self.quad(
            r.left_top(),
            r.right_top(),
            r.right_bottom(),
            r.left_bottom(),
            colour,
        );
    }

    /// A quadrilateral, unfeathered. The lanes fill under their curves with a
    /// run of these; a rim on each would draw a seam down every shared edge.
    pub fn quad(&mut self, a: Pos2, b: Pos2, c: Pos2, d: Pos2, colour: Color32) {
        if colour.a() == 0 {
            return;
        }
        let base = self.mesh.vertices.len() as u32;
        for p in [a, b, c, d] {
            self.mesh.colored_vertex(p, colour);
        }
        self.quad_indices(base, base + 1, base + 2, base + 3);
    }

    /// A filled disc, feathered. Twelve sides is indistinguishable from round
    /// at the sizes the overlay uses and costs a third of the triangles that
    /// egui's own circle does.
    pub fn disc(&mut self, centre: Pos2, radius: f32, colour: Color32) {
        if radius <= 0.0 {
            return;
        }
        self.convex(&Self::ring(centre, radius), colour);
    }

    /// A circle drawn as an outline. This is the calm barb, and in light airs
    /// it is every barb on the screen, which is why the ring is built on the
    /// stack.
    pub fn circle_outline(&mut self, centre: Pos2, radius: f32, width: f32, colour: Color32) {
        if radius <= 0.0 {
            return;
        }
        let ring = Self::ring(centre, radius);
        for i in 0..RING {
            self.line(ring[i], ring[(i + 1) % RING], width, colour);
        }
    }

    fn ring(centre: Pos2, radius: f32) -> [Pos2; RING] {
        std::array::from_fn(|i| {
            let a = i as f32 / RING as f32 * std::f32::consts::TAU;
            Pos2::new(centre.x + radius * a.cos(), centre.y + radius * a.sin())
        })
    }

    /// Where the solid core of a line ends and its fade does, either side of
    /// the centre.
    ///
    /// A line no wider than the feather cannot have a core at all, so it is
    /// drawn at the feather's width and pays for the difference in alpha —
    /// egui's own rule, and the reason a hairline does not shimmer in and out
    /// as it crosses the pixel grid.
    fn rims(&self, width: f32, colour: Color32) -> (f32, f32, Color32) {
        if self.feather > 0.0 && width <= self.feather {
            (
                0.0,
                self.feather,
                colour.linear_multiply(width / self.feather),
            )
        } else {
            (
                ((width - self.feather) * 0.5).max(0.0),
                (width + self.feather) * 0.5,
                colour,
            )
        }
    }

    fn quad_indices(&mut self, a: u32, b: u32, c: u32, d: u32) {
        self.mesh.add_triangle(a, b, c);
        self.mesh.add_triangle(a, c, d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch() -> Batch {
        Batch::with_feather(1.0)
    }

    #[test]
    fn an_empty_batch_holds_nothing() {
        assert!(batch().is_empty());
    }

    /// A line is three quads across: the feather, the core, the feather.
    #[test]
    fn a_line_is_eight_vertices_and_six_triangles() {
        let mut b = batch();
        b.line(Pos2::new(0.0, 0.0), Pos2::new(10.0, 0.0), 2.0, Color32::RED);
        assert_eq!(b.mesh.vertices.len(), 8);
        assert_eq!(b.mesh.indices.len(), 18);
    }

    /// The rim must be transparent, or the anti-aliasing draws a halo instead
    /// of a fade.
    #[test]
    fn the_rim_of_a_line_fades_to_nothing() {
        let mut b = batch();
        b.line(Pos2::new(0.0, 0.0), Pos2::new(10.0, 0.0), 2.0, Color32::RED);
        let v = &b.mesh.vertices;
        assert_eq!(v[0].color, Color32::TRANSPARENT);
        assert_eq!(v[1].color, Color32::RED);
        assert_eq!(v[2].color, Color32::RED);
        assert_eq!(v[3].color, Color32::TRANSPARENT);
    }

    /// Degenerate input must produce nothing rather than NaN vertices, which
    /// take the whole draw call down with them.
    #[test]
    fn nothing_is_drawn_for_nothing() {
        let mut b = batch();
        b.line(Pos2::new(3.0, 3.0), Pos2::new(3.0, 3.0), 2.0, Color32::RED);
        b.line(Pos2::new(0.0, 0.0), Pos2::new(9.0, 0.0), 0.0, Color32::RED);
        b.line(
            Pos2::new(0.0, 0.0),
            Pos2::new(9.0, 0.0),
            2.0,
            Color32::TRANSPARENT,
        );
        b.convex(&[Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)], Color32::RED);
        b.rect(Rect::from_min_max(Pos2::new(4.0, 4.0), Pos2::new(4.0, 9.0)), Color32::RED);
        assert!(b.is_empty());
    }

    /// The mitre is capped, so an arrowhead's tip cannot shoot off screen.
    #[test]
    fn a_sharp_corner_does_not_run_away() {
        let mut b = batch();
        let tip = Pos2::new(0.0, -40.0);
        b.convex(
            &[tip, Pos2::new(-1.0, 0.0), Pos2::new(1.0, 0.0)],
            Color32::RED,
        );
        // The tip's own pair of vertices are the first two: the rim must sit
        // beside the tip, not somewhere out along the mitre.
        let far = b.mesh.vertices[..2]
            .iter()
            .map(|v| (v.pos - tip).length())
            .fold(0.0f32, f32::max);
        assert!(far < 4.0, "the tip's rim reached {far} points away");
        assert!(b.mesh.vertices.iter().all(|v| v.pos.is_finite()));
    }

    /// Whichever way round the caller wound the polygon, the solid half of
    /// the rim must be the inside one.
    #[test]
    fn a_polygon_is_solid_whichever_way_it_is_wound() {
        let tri = [
            Pos2::new(0.0, 0.0),
            Pos2::new(10.0, 0.0),
            Pos2::new(5.0, 8.0),
        ];
        let centroid = Pos2::new(5.0, 8.0 / 3.0);
        for wound in [tri.to_vec(), tri.iter().rev().copied().collect()] {
            let mut b = batch();
            b.convex(&wound, Color32::RED);
            for pair in b.mesh.vertices.chunks(2) {
                let (solid, clear) = (pair[0], pair[1]);
                assert_eq!(clear.color, Color32::TRANSPARENT);
                assert!(
                    (solid.pos - centroid).length() < (clear.pos - centroid).length(),
                    "the solid vertex sat outside the transparent one"
                );
            }
        }
    }

    /// A cell is drawn only when the field has something to say at all four
    /// of its corners, so the edge of a forecast is a clean hole rather than
    /// a fade into transparent black.
    #[test]
    fn the_fill_grid_shares_corners_and_skips_the_edge_of_the_data() {
        let red = Some(Color32::RED);
        // 3×2 corners = two cells across, one down.
        let at = [
            [0.0, 0.0],
            [10.0, 0.0],
            [20.0, 0.0],
            [0.0, 10.0],
            [10.0, 10.0],
            [20.0, 10.0],
        ];
        let mut b = batch();
        b.grid(&at, 3, 2, &[red, red, red, red, red, red]);
        assert_eq!(b.mesh.vertices.len(), 6, "corners were not shared");
        assert_eq!(b.mesh.indices.len(), 2 * 6, "expected two cells");

        // Knock out one corner: the cell touching it goes, its neighbour stays.
        let mut b = batch();
        b.grid(&at, 3, 2, &[red, red, None, red, red, red]);
        assert_eq!(b.mesh.vertices.len(), 6);
        assert_eq!(b.mesh.indices.len(), 6, "the holed cell was still drawn");

        // The lattice is projected, not stepped, so the cells need not be
        // square — a corner may land anywhere and the mesh must still close.
        let skew = [
            [0.0, 0.0],
            [12.0, 1.0],
            [21.0, 3.0],
            [1.0, 9.0],
            [13.0, 11.0],
            [22.0, 14.0],
        ];
        let mut b = batch();
        b.grid(&skew, 3, 2, &[red; 6]);
        assert_eq!(b.mesh.indices.len(), 2 * 6);

        // Degenerate grids draw nothing rather than panicking on the indices.
        let mut b = batch();
        b.grid(&at, 1, 5, &[red; 5]);
        b.grid(&at, 3, 2, &[red, red]);
        b.grid(&at[..2], 3, 2, &[red; 6]);
        assert!(b.is_empty());
    }

    /// A hairline keeps its width and loses its alpha instead of vanishing
    /// between two pixels.
    #[test]
    fn a_hairline_pays_in_alpha() {
        let mut b = Batch::with_feather(2.0);
        b.line(Pos2::new(0.0, 0.0), Pos2::new(10.0, 0.0), 1.0, Color32::WHITE);
        let core = b.mesh.vertices[1];
        assert!(core.color.a() < 255, "alpha was not scaled down");
        assert!(core.pos.y.abs() < 1e-3, "a hairline has no core to spread");
        // Faded across the feather either side, exactly as egui does it.
        let spread = b
            .mesh
            .vertices
            .iter()
            .map(|v| v.pos.y)
            .fold(f32::NEG_INFINITY, f32::max);
        assert!((spread - 2.0).abs() < 1e-3, "hairline faded over {spread}");
    }
}
