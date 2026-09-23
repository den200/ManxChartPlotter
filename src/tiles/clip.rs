//! Geometry clipping for tile rendering.
//!
//! Implements Sutherland-Hodgman algorithm for clipping triangles and polylines
//! to tile bounds.

use super::TileBounds;

/// Epsilon for floating-point comparisons when stitching segments.
/// This prevents micro-gaps caused by floating-point rounding in intersection calculations.
const CLIP_EPSILON: f64 = 1e-9;

/// Check if two points are equal within epsilon tolerance.
fn points_equal_epsilon(a: &[f64; 2], b: &[f64; 2]) -> bool {
    (a[0] - b[0]).abs() < CLIP_EPSILON && (a[1] - b[1]).abs() < CLIP_EPSILON
}

/// Snap a coordinate to a boundary value if very close.
fn snap_to_boundary(value: f64, boundary: f64) -> f64 {
    if (value - boundary).abs() < CLIP_EPSILON {
        boundary
    } else {
        value
    }
}

/// Clip a triangle to tile bounds using Sutherland-Hodgman algorithm.
///
/// Returns a polygon (0-7 vertices) representing the clipped triangle.
/// Empty vec means triangle is entirely outside bounds.
pub fn clip_triangle(tri: [[f64; 2]; 3], bounds: &TileBounds) -> Vec<[f64; 2]> {
    let mut polygon: Vec<[f64; 2]> = tri.to_vec();

    // Clip against each edge of the bounding box
    polygon = clip_polygon_edge(&polygon, |p| p[0] >= bounds.min_x, |a, b| intersect_left(a, b, bounds.min_x));
    if polygon.is_empty() { return polygon; }

    polygon = clip_polygon_edge(&polygon, |p| p[0] <= bounds.max_x, |a, b| intersect_right(a, b, bounds.max_x));
    if polygon.is_empty() { return polygon; }

    polygon = clip_polygon_edge(&polygon, |p| p[1] >= bounds.min_y, |a, b| intersect_bottom(a, b, bounds.min_y));
    if polygon.is_empty() { return polygon; }

    polygon = clip_polygon_edge(&polygon, |p| p[1] <= bounds.max_y, |a, b| intersect_top(a, b, bounds.max_y));

    polygon
}

/// Clip polygon against a single edge using Sutherland-Hodgman
fn clip_polygon_edge<F, I>(polygon: &[[f64; 2]], inside: F, intersect: I) -> Vec<[f64; 2]>
where
    F: Fn(&[f64; 2]) -> bool,
    I: Fn([f64; 2], [f64; 2]) -> [f64; 2],
{
    if polygon.is_empty() {
        return Vec::new();
    }

    let mut output = Vec::with_capacity(polygon.len() + 1);
    let mut prev = polygon[polygon.len() - 1];
    let mut prev_inside = inside(&prev);

    for &curr in polygon {
        let curr_inside = inside(&curr);

        if curr_inside {
            if !prev_inside {
                // Entering: add intersection point
                output.push(intersect(prev, curr));
            }
            // Add current point (inside)
            output.push(curr);
        } else if prev_inside {
            // Leaving: add intersection point
            output.push(intersect(prev, curr));
        }
        // Both outside: add nothing

        prev = curr;
        prev_inside = curr_inside;
    }

    output
}

/// Line-edge intersection helpers
fn intersect_left(a: [f64; 2], b: [f64; 2], x: f64) -> [f64; 2] {
    let t = (x - a[0]) / (b[0] - a[0]);
    [x, a[1] + t * (b[1] - a[1])]
}

fn intersect_right(a: [f64; 2], b: [f64; 2], x: f64) -> [f64; 2] {
    let t = (x - a[0]) / (b[0] - a[0]);
    [x, a[1] + t * (b[1] - a[1])]
}

fn intersect_bottom(a: [f64; 2], b: [f64; 2], y: f64) -> [f64; 2] {
    let t = (y - a[1]) / (b[1] - a[1]);
    [a[0] + t * (b[0] - a[0]), y]
}

fn intersect_top(a: [f64; 2], b: [f64; 2], y: f64) -> [f64; 2] {
    let t = (y - a[1]) / (b[1] - a[1]);
    [a[0] + t * (b[0] - a[0]), y]
}

/// Triangulate a clipped polygon using fan triangulation.
///
/// Works for convex polygons (which clipped triangles always are).
/// Returns flat list of vertices (3 per triangle).
pub fn triangulate_fan(polygon: &[[f64; 2]]) -> Vec<[f64; 2]> {
    if polygon.len() < 3 {
        return Vec::new();
    }

    let mut triangles = Vec::with_capacity((polygon.len() - 2) * 3);
    let center = polygon[0];

    for i in 1..polygon.len() - 1 {
        triangles.push(center);
        triangles.push(polygon[i]);
        triangles.push(polygon[i + 1]);
    }

    triangles
}

/// Clip a polyline to tile bounds.
///
/// Returns a list of clipped polyline segments. A single input polyline
/// may become multiple output segments when crossing tile boundaries.
///
/// NOTE: Does NOT preserve global arc_length - dash patterns will have seams.
/// This is a known MVP artifact (see plan). [`clip_polyline_arc`] does, for
/// the LC symbols that need it.
pub fn clip_polyline(points: &[[f64; 2]], bounds: &TileBounds) -> Vec<Vec<[f64; 2]>> {
    clip_polyline_arc(points, bounds)
        .into_iter()
        .map(|(segment, _)| segment)
        .collect()
}

/// [`clip_polyline`], also returning for each piece how far along the whole
/// input polyline its first point lies (metres).
///
/// Every tile clips the same feature line independently. Measuring each
/// piece's arc length from the start of the *unclipped* line gives two tiles
/// the same figure at their shared edge, so a pattern repeated along the arc
/// length (an LC symbol) runs on across the tile boundary without a seam.
pub fn clip_polyline_arc(points: &[[f64; 2]], bounds: &TileBounds) -> Vec<(Vec<[f64; 2]>, f64)> {
    if points.len() < 2 {
        return Vec::new();
    }

    let mut result = Vec::new();
    let mut current_segment: Vec<[f64; 2]> = Vec::new();
    let mut current_start = 0.0;
    // Arc length of the input polyline up to points[i].
    let mut arc = 0.0;

    for i in 0..points.len() - 1 {
        let p0 = points[i];
        let p1 = points[i + 1];
        let seg_len = ((p1[0] - p0[0]).powi(2) + (p1[1] - p0[1]).powi(2)).sqrt();

        let clipped = clip_line_segment(p0, p1, bounds);

        if let Some((start, end)) = clipped {
            let start_arc = arc + ((start[0] - p0[0]).powi(2) + (start[1] - p0[1]).powi(2)).sqrt();
            // Check if this segment connects to the current one
            if current_segment.is_empty() {
                current_segment.push(start);
                current_segment.push(end);
                current_start = start_arc;
            } else if current_segment.last().map_or(false, |last| points_equal_epsilon(last, &start)) {
                // Continuous within epsilon - just add the end point
                current_segment.push(end);
            } else {
                // Discontinuous - start a new segment
                if current_segment.len() >= 2 {
                    result.push((std::mem::take(&mut current_segment), current_start));
                }
                current_segment.clear();
                current_segment.push(start);
                current_segment.push(end);
                current_start = start_arc;
            }
        } else {
            // Segment is outside - finish current segment
            if current_segment.len() >= 2 {
                result.push((std::mem::take(&mut current_segment), current_start));
            }
            current_segment.clear();
        }
        arc += seg_len;
    }

    // Don't forget the last segment
    if current_segment.len() >= 2 {
        result.push((current_segment, current_start));
    }

    result
}

/// Douglas-Peucker line simplification.
///
/// Removes points that deviate less than `epsilon` from the straight line
/// between their neighbors. This reduces vertex count for lines that are
/// sub-pixel at the current zoom level.
///
/// Operates on global Mercator coordinates (f64). Endpoints are always kept.
/// Returns a new simplified polyline, or the original if it has 2 or fewer points.
pub fn simplify_polyline(points: &[[f64; 2]], epsilon: f64) -> Vec<[f64; 2]> {
    let n = points.len();
    if n <= 2 {
        return points.to_vec();
    }

    // Find the point with maximum distance from the line (first, last)
    let mut max_dist = 0.0_f64;
    let mut max_idx = 0;
    let start = points[0];
    let end = points[n - 1];

    for i in 1..n - 1 {
        let d = perpendicular_distance(points[i], start, end);
        if d > max_dist {
            max_dist = d;
            max_idx = i;
        }
    }

    if max_dist > epsilon {
        // Recurse on both halves
        let mut left = simplify_polyline(&points[..=max_idx], epsilon);
        let right = simplify_polyline(&points[max_idx..], epsilon);
        // Remove duplicate junction point
        left.pop();
        left.extend_from_slice(&right);
        left
    } else {
        // All intermediate points are within tolerance — keep only endpoints
        vec![start, end]
    }
}

/// Perpendicular distance from point `p` to the line segment (a, b).
fn perpendicular_distance(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let dx = b[0] - a[0];
    let dy = b[1] - a[1];
    let len_sq = dx * dx + dy * dy;
    if len_sq < 1e-20 {
        // a and b are the same point — return distance to a
        let ex = p[0] - a[0];
        let ey = p[1] - a[1];
        return (ex * ex + ey * ey).sqrt();
    }
    // Distance = |cross product| / |line length|
    let cross = (p[0] - a[0]) * dy - (p[1] - a[1]) * dx;
    cross.abs() / len_sq.sqrt()
}

/// Clip a single line segment to bounds using Cohen-Sutherland algorithm.
///
/// Returns Some((start, end)) if the segment intersects the bounds,
/// None if it's entirely outside.
fn clip_line_segment(mut p0: [f64; 2], mut p1: [f64; 2], bounds: &TileBounds) -> Option<([f64; 2], [f64; 2])> {
    let mut code0 = outcode(p0, bounds);
    let mut code1 = outcode(p1, bounds);

    loop {
        if code0 | code1 == 0 {
            // Both inside
            return Some((p0, p1));
        }

        if code0 & code1 != 0 {
            // Both outside same edge - no intersection
            return None;
        }

        // Pick an outside point
        let code_out = if code0 != 0 { code0 } else { code1 };

        // Find intersection point
        let (mut x, mut y) = if code_out & TOP != 0 {
            let t = (bounds.max_y - p0[1]) / (p1[1] - p0[1]);
            (p0[0] + t * (p1[0] - p0[0]), bounds.max_y)
        } else if code_out & BOTTOM != 0 {
            let t = (bounds.min_y - p0[1]) / (p1[1] - p0[1]);
            (p0[0] + t * (p1[0] - p0[0]), bounds.min_y)
        } else if code_out & RIGHT != 0 {
            let t = (bounds.max_x - p0[0]) / (p1[0] - p0[0]);
            (bounds.max_x, p0[1] + t * (p1[1] - p0[1]))
        } else {
            // LEFT
            let t = (bounds.min_x - p0[0]) / (p1[0] - p0[0]);
            (bounds.min_x, p0[1] + t * (p1[1] - p0[1]))
        };

        // Snap to exact boundary values to prevent floating-point micro-gaps
        x = snap_to_boundary(x, bounds.min_x);
        x = snap_to_boundary(x, bounds.max_x);
        y = snap_to_boundary(y, bounds.min_y);
        y = snap_to_boundary(y, bounds.max_y);

        if code_out == code0 {
            p0 = [x, y];
            code0 = outcode(p0, bounds);
        } else {
            p1 = [x, y];
            code1 = outcode(p1, bounds);
        }
    }
}

// Cohen-Sutherland outcodes
pub const LEFT: u8 = 1;
pub const RIGHT: u8 = 2;
pub const BOTTOM: u8 = 4;
pub const TOP: u8 = 8;

pub fn outcode(p: [f64; 2], bounds: &TileBounds) -> u8 {
    let mut code = 0;
    if p[0] < bounds.min_x { code |= LEFT; }
    if p[0] > bounds.max_x { code |= RIGHT; }
    if p[1] < bounds.min_y { code |= BOTTOM; }
    if p[1] > bounds.max_y { code |= TOP; }
    code
}

/// Check if a point is inside tile bounds
pub fn point_in_bounds(p: [f64; 2], bounds: &TileBounds) -> bool {
    p[0] >= bounds.min_x && p[0] <= bounds.max_x &&
    p[1] >= bounds.min_y && p[1] <= bounds.max_y
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two neighbouring tiles clipping one line agree on the arc length at
    /// their shared edge, and a piece that re-enters a tile carries the
    /// distance it travelled outside.
    #[test]
    fn clipped_pieces_carry_their_arc_length_along_the_whole_line() {
        let line = [[-50.0, 10.0], [150.0, 10.0], [150.0, 90.0], [50.0, 90.0]];
        let west = TileBounds::new(0.0, 100.0, 0.0, 100.0);
        let east = TileBounds::new(100.0, 200.0, 0.0, 100.0);

        let w = clip_polyline_arc(&line, &west);
        let e = clip_polyline_arc(&line, &east);
        assert_eq!(w.len(), 2, "{w:?}");
        assert_eq!(e.len(), 1, "{e:?}");
        // West tile: enters at x=0 after 50 m; re-enters at x=100 on the top
        // edge after 200 + 80 + 50 = 330 m.
        assert!((w[0].1 - 50.0).abs() < 1e-9);
        assert!((w[1].1 - 330.0).abs() < 1e-9);
        // East tile: starts at x=100, 150 m along — where the west piece ends.
        assert!((e[0].1 - 150.0).abs() < 1e-9);
        let w0 = &w[0].0;
        let west_len: f64 = w0
            .windows(2)
            .map(|p| ((p[1][0] - p[0][0]).powi(2) + (p[1][1] - p[0][1]).powi(2)).sqrt())
            .sum();
        assert!((w[0].1 + west_len - e[0].1).abs() < 1e-9);
        // And the plain clip is unchanged.
        assert_eq!(clip_polyline(&line, &west), w.into_iter().map(|p| p.0).collect::<Vec<_>>());
    }

    #[test]
    fn clip_triangle_fully_inside() {
        let bounds = TileBounds::new(0.0, 100.0, 0.0, 100.0);
        let tri = [[10.0, 10.0], [50.0, 90.0], [90.0, 10.0]];
        let clipped = clip_triangle(tri, &bounds);
        assert_eq!(clipped.len(), 3);
    }

    #[test]
    fn clip_triangle_fully_outside() {
        let bounds = TileBounds::new(0.0, 100.0, 0.0, 100.0);
        let tri = [[200.0, 200.0], [250.0, 290.0], [290.0, 200.0]];
        let clipped = clip_triangle(tri, &bounds);
        assert!(clipped.is_empty());
    }

    #[test]
    fn clip_triangle_partial() {
        let bounds = TileBounds::new(0.0, 100.0, 0.0, 100.0);
        // Triangle extends beyond right edge
        let tri = [[50.0, 50.0], [150.0, 50.0], [100.0, 100.0]];
        let clipped = clip_triangle(tri, &bounds);
        // Should be clipped to a polygon with 4 or 5 vertices
        assert!(clipped.len() >= 3 && clipped.len() <= 7);
    }

    #[test]
    fn triangulate_fan_simple() {
        let polygon = vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
        let triangles = triangulate_fan(&polygon);
        // 4-vertex polygon = 2 triangles = 6 vertices
        assert_eq!(triangles.len(), 6);
    }

    #[test]
    fn clip_polyline_inside() {
        let bounds = TileBounds::new(0.0, 100.0, 0.0, 100.0);
        let points = vec![[10.0, 10.0], [50.0, 50.0], [90.0, 90.0]];
        let clipped = clip_polyline(&points, &bounds);
        assert_eq!(clipped.len(), 1);
        assert_eq!(clipped[0].len(), 3);
    }

    #[test]
    fn clip_polyline_outside() {
        let bounds = TileBounds::new(0.0, 100.0, 0.0, 100.0);
        let points = vec![[200.0, 200.0], [250.0, 250.0], [290.0, 290.0]];
        let clipped = clip_polyline(&points, &bounds);
        assert!(clipped.is_empty());
    }

    #[test]
    fn clip_polyline_crossing() {
        let bounds = TileBounds::new(0.0, 100.0, 0.0, 100.0);
        // Line crosses through the bounds
        let points = vec![[-50.0, 50.0], [150.0, 50.0]];
        let clipped = clip_polyline(&points, &bounds);
        assert_eq!(clipped.len(), 1);
        // Should be clipped to [0, 50] - [100, 50]
        assert!((clipped[0][0][0] - 0.0).abs() < 0.01);
        assert!((clipped[0][1][0] - 100.0).abs() < 0.01);
    }

    #[test]
    fn point_in_bounds_works() {
        let bounds = TileBounds::new(0.0, 100.0, 0.0, 100.0);
        assert!(point_in_bounds([50.0, 50.0], &bounds));
        assert!(!point_in_bounds([150.0, 50.0], &bounds));
        assert!(point_in_bounds([0.0, 0.0], &bounds)); // Edge is inside
        assert!(point_in_bounds([100.0, 100.0], &bounds)); // Corner is inside
    }

    #[test]
    fn simplify_keeps_endpoints() {
        let points = vec![[0.0, 0.0], [1.0, 0.0], [2.0, 0.0]];
        let result = simplify_polyline(&points, 0.5);
        // All points are collinear — should reduce to 2 endpoints
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], [0.0, 0.0]);
        assert_eq!(result[1], [2.0, 0.0]);
    }

    #[test]
    fn simplify_keeps_detail_above_epsilon() {
        let points = vec![[0.0, 0.0], [1.0, 1.0], [2.0, 0.0]];
        // Mid-point is 1.0 away from baseline — epsilon 0.5 should keep it
        let result = simplify_polyline(&points, 0.5);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn simplify_removes_detail_below_epsilon() {
        let points = vec![[0.0, 0.0], [1.0, 0.1], [2.0, 0.0]];
        // Mid-point is 0.1 away from baseline — epsilon 0.5 should remove it
        let result = simplify_polyline(&points, 0.5);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn simplify_two_points_unchanged() {
        let points = vec![[0.0, 0.0], [10.0, 10.0]];
        let result = simplify_polyline(&points, 1.0);
        assert_eq!(result.len(), 2);
    }
}
