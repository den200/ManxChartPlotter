// Screen-space overlay quads: the info bubble's panel, and anything else drawn
// on the glass rather than on the chart.
//
// Positions arrive already in pixels with the origin top-left, which is how the
// panel is laid out on the CPU, so the vertex stage only has to map them into
// clip space. Depth is forced to the near plane: the overlay is not part of the
// S-52 priority scheme and must never be occluded by chart geometry.

struct Overlay {
    // [viewport_width_px, viewport_height_px]
    view_size: vec2<f32>,
    _pad: vec2<f32>,
}

@group(0) @binding(0) var<uniform> overlay: Overlay;

struct VertexInput {
    @location(0) position_px: vec2<f32>,
    @location(1) color: vec4<f32>,
    // Distance from this vertex to the panel edge, in pixels, interpolated
    // across the quad so the fragment stage can round the corners.
    @location(2) corner: vec2<f32>,
    // [corner_radius_px, half_width_px, half_height_px, border_px]
    @location(3) shape: vec4<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) corner: vec2<f32>,
    @location(2) @interpolate(flat) shape: vec4<f32>,
}

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    let ndc = vec2<f32>(
        in.position_px.x / overlay.view_size.x * 2.0 - 1.0,
        1.0 - in.position_px.y / overlay.view_size.y * 2.0,
    );
    out.clip_position = vec4<f32>(ndc, 0.0, 1.0);
    out.color = in.color;
    out.corner = in.corner;
    out.shape = in.shape;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let radius = in.shape.x;
    let half = in.shape.yz;
    let border = in.shape.w;

    // Signed distance to a rounded rectangle, in pixels: negative inside.
    let q = abs(in.corner) - (half - vec2<f32>(radius, radius));
    let dist = length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - radius;

    // One pixel of feathering, so the corners are not stair-stepped.
    let fill = 1.0 - smoothstep(-1.0, 0.0, dist);
    if (fill <= 0.001) {
        discard;
    }

    var color = in.color;
    if (border > 0.0 && dist > -border) {
        // The border is the fill colour darkened, so one vertex colour
        // describes the whole panel.
        color = vec4<f32>(color.rgb * 0.45, color.a);
    }
    return vec4<f32>(color.rgb, color.a * fill);
}
