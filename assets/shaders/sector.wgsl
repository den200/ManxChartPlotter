// Light sector arcs and legs, at a constant size on the screen.
//
// One instance per arc or leg: the light's position on the chart plus sizes in
// pixels. The vertex stage draws a screen-aligned quad around the projected
// centre — like a symbol, so the figure keeps its size at every zoom — and the
// fragment stage draws the ring segment or the dashed radial leg inside it.

struct CameraUniform {
    view_proj: mat4x4<f32>,
    view_size: vec2<f32>,
    pixels_per_meter: f32,
    px_per_point: f32,
    anchor_offset: vec2<f32>,
    _pad: vec2<f32>,
}

@group(0) @binding(0) var<uniform> camera: CameraUniform;
@group(0) @binding(1) var<storage, read> palette: array<vec4<f32>>;

const TAU: f32 = 6.28318530718;
const KIND_ARC: u32 = 0u;

struct Instance {
    @location(0) position: vec2<f32>,  // metres from the tile centre
    @location(1) radius_px: f32,       // arc radius / leg length
    @location(2) width_px: f32,
    @location(3) bearings: vec2<f32>,  // start, end: radians clockwise from north
    @location(4) dash_px: vec2<f32>,   // on, off (0, 0 = solid)
    @location(5) color_index: u32,
    @location(6) kind: u32,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    // Pixels from the centre, y up (north, untilted).
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) shape: vec4<f32>,  // radius, width, start, end
    @location(2) @interpolate(flat) dash: vec2<f32>,
    @location(3) @interpolate(flat) color_index: u32,
    @location(4) @interpolate(flat) kind: u32,
}

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, inst: Instance) -> VertexOutput {
    var corner: vec2<f32>;
    switch (vi) {
        case 0u: { corner = vec2(-1.0, -1.0); }
        case 1u: { corner = vec2(1.0, -1.0); }
        case 2u: { corner = vec2(-1.0, 1.0); }
        case 3u: { corner = vec2(-1.0, 1.0); }
        case 4u: { corner = vec2(1.0, -1.0); }
        default: { corner = vec2(1.0, 1.0); }
    }
    // Room for the stroke and a pixel of antialiasing.
    let half = inst.radius_px + inst.width_px + 1.0;
    let local = corner * half;

    var clip = camera.view_proj * vec4<f32>(inst.position, 0.0, 1.0);
    let ndc_offset = local / camera.view_size * 2.0;
    clip.x += ndc_offset.x * clip.w;
    clip.y += ndc_offset.y * clip.w;
    // No depth test for these (the priority loop orders them); keep z inside
    // the range. A centre behind a tilted eye would come back mirrored, so it
    // is pushed out of the depth range instead.
    clip.z = 0.5 * clip.w;
    if clip.w <= 0.0 {
        clip = vec4<f32>(0.0, 0.0, 2.0, 1.0);
    }

    var out: VertexOutput;
    out.clip_position = clip;
    out.local = local;
    out.shape = vec4<f32>(inst.radius_px, inst.width_px, inst.bearings.x, inst.bearings.y);
    out.dash = inst.dash_px;
    out.color_index = inst.color_index;
    out.kind = inst.kind;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let p = in.local;
    let radius = in.shape.x;
    let half_width = in.shape.y * 0.5;
    let start = in.shape.z;
    let end = in.shape.w;

    var d: f32;
    if in.kind == KIND_ARC {
        // Bearing of this pixel from the centre, clockwise from north.
        let b = atan2(p.x, p.y);
        let rel = (b - start) - floor((b - start) / TAU) * TAU;
        if rel > end - start {
            discard;
        }
        d = abs(length(p) - radius);
    } else {
        let u = vec2<f32>(sin(start), cos(start));
        let t = dot(p, u);
        if t < 0.0 || t > radius {
            discard;
        }
        let period = in.dash.x + in.dash.y;
        if period > 0.0 && in.dash.y > 0.0 && (t % period) > in.dash.x {
            discard;
        }
        d = abs(p.x * u.y - p.y * u.x);
    }

    // Coverage of a stroke `half_width` either side, a pixel of smoothing.
    let alpha = clamp(half_width + 0.5 - d, 0.0, 1.0);
    if alpha <= 0.0 {
        discard;
    }
    let c = palette[in.color_index];
    return vec4<f32>(c.rgb, c.a * alpha);
}
