use super::LineVertex;

/// Primitive restart index value for u32 index buffers.
/// When the GPU encounters this value in the index buffer, it restarts the strip.
pub const PRIMITIVE_RESTART_INDEX: u32 = u32::MAX;

/// Build line vertices with adjacency data and arc-length for shader-based rendering.
///
/// Each point in the polyline produces 2 vertices (one for each side: +1.0, -1.0).
/// At endpoints, prev/next are duplicated to handle caps correctly.
/// Arc length is cumulative distance from the start of the polyline (in meters).
/// Uses TriangleStrip topology.
pub fn build_line_vertices(points: &[[f32; 2]]) -> Vec<LineVertex> {
    let n = points.len();
    if n < 2 {
        return vec![];
    }

    let mut vertices = Vec::with_capacity(n * 2);
    let mut arc_len: f32 = 0.0;

    for i in 0..n {
        // Accumulate arc length from previous point
        if i > 0 {
            let dx = points[i][0] - points[i - 1][0];
            let dy = points[i][1] - points[i - 1][1];
            arc_len += (dx * dx + dy * dy).sqrt();
        }

        let prev = if i == 0 { points[0] } else { points[i - 1] };
        let curr = points[i];
        let next = if i == n - 1 { points[n - 1] } else { points[i + 1] };

        // Emit +side vertex
        vertices.push(LineVertex {
            prev,
            curr,
            next,
            side: 1.0,
            arc_len,
        });
        // Emit -side vertex
        vertices.push(LineVertex {
            prev,
            curr,
            next,
            side: -1.0,
            arc_len,
        });
    }
    vertices
}

/// Build line vertices for multiple polylines with primitive restart indices.
///
/// Returns (vertices, indices) where indices use primitive restart (u32::MAX)
/// to separate polylines. This is more efficient and correct than degenerate vertices
/// because the GPU hardware properly terminates each strip.
///
/// Use with `PrimitiveState::strip_index_format = Some(IndexFormat::Uint32)`.
pub fn build_line_vertices_multi_indexed(polylines: &[Vec<[f32; 2]>]) -> (Vec<LineVertex>, Vec<u32>) {
    let total_points: usize = polylines.iter().map(|p| p.len()).sum();
    // Each point = 2 vertices
    let estimated_vertex_capacity = total_points * 2;
    // Each vertex gets an index, plus primitive restart between polylines
    let estimated_index_capacity = estimated_vertex_capacity + polylines.len();

    let mut vertices = Vec::with_capacity(estimated_vertex_capacity);
    let mut indices = Vec::with_capacity(estimated_index_capacity);

    for points in polylines.iter() {
        let line_verts = build_line_vertices(points);
        if line_verts.is_empty() {
            continue;
        }

        // Insert primitive restart to break the strip (except for first polyline)
        if !indices.is_empty() {
            indices.push(PRIMITIVE_RESTART_INDEX);
        }

        // Add indices for this polyline's vertices
        let base_index = vertices.len() as u32;
        for i in 0..line_verts.len() {
            indices.push(base_index + i as u32);
        }

        vertices.extend(line_verts);
    }

    (vertices, indices)
}


