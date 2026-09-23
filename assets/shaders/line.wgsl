// Shader-based polyline rendering with screen-space stroke width and dash patterns.
//
// Uses triangle strip topology with adjacency data (prev/curr/next) to compute
// miter/bevel joins in the vertex shader. Width is in screen pixels, not world units.
// Dash patterns are computed in the fragment shader using arc-length.
//
// vs_lc / fs_lc, at the end, draw LC() complex lines with the same bindings.

// The camera, per draw: positions are metres from the draw's origin (a tile
// centre) and view_proj already has that origin folded in, in f64 on the CPU.
struct CameraUniform {
    view_proj: mat4x4<f32>,
    view_size: vec2<f32>,
    pixels_per_meter: f32,
    px_per_point: f32,
    anchor_offset: vec2<f32>,
    _pad: vec2<f32>,
}

// The style, per line batch. Nothing here depends on the camera, so it is
// only rewritten when the set of visible styles or the display changes.
struct LineUniforms {
    line_width_px: f32,          // stroke width in pixels, e.g., 2.0
    join_limit: f32,             // miter limit, e.g., 4.0
    color_index: u32,            // index into palette
    dash_on_px: f32,             // dash on length in screen pixels
    dash_off_px: f32,            // dash off (gap) length in screen pixels
    disp_prio: f32,              // display priority for depth sorting
    dot_on_px: f32,              // dot length in gap for DASD pattern
    lc_advance_px: f32,          // LC() symbol repeat, px (LC pipeline only)
    lc_first: u32,               // first LC() segment in lc_segments
    lc_count: u32,               // number of LC() segments
    lc_x_range: vec2<f32>,       // LC() symbol's reach before/after its place, px
    // Total: 48 bytes
}

@group(0) @binding(0) var<uniform> camera: CameraUniform;
@group(0) @binding(1)
var<storage, read> palette: array<vec4<f32>>;

@group(1) @binding(0) var<uniform> u: LineUniforms;
// LC() symbols in pixels: per segment [x0, y0, x1, y1] then [half stroke, ...],
// from the symbol's pivot: x along the direction of travel, y to its left.
@group(1) @binding(1) var<storage, read> lc_segments: array<vec4<f32>>;

struct VertexInput {
    @location(0) prev: vec2<f32>,
    @location(1) curr: vec2<f32>,
    @location(2) next: vec2<f32>,
    @location(3) side: f32,
    @location(4) arc_len: f32,   // cumulative arc length in meters
}

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) arc_len: f32,   // pass to fragment for dash pattern
}

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    // Project all points to clip space
    let clip_prev = camera.view_proj * vec4(in.prev, 0.0, 1.0);
    let clip_curr = camera.view_proj * vec4(in.curr, 0.0, 1.0);
    let clip_next = camera.view_proj * vec4(in.next, 0.0, 1.0);

    // Convert to NDC
    let ndc_prev = clip_prev.xy / clip_prev.w;
    let ndc_curr = clip_curr.xy / clip_curr.w;
    let ndc_next = clip_next.xy / clip_next.w;

    // Compute direction vectors
    var dir_from_prev = ndc_curr - ndc_prev;
    var dir_to_next = ndc_next - ndc_curr;

    let len_prev = length(dir_from_prev);
    let len_next = length(dir_to_next);

    // Normalize or use fallback
    if len_prev > 0.0001 {
        dir_from_prev = dir_from_prev / len_prev;
    }
    if len_next > 0.0001 {
        dir_to_next = dir_to_next / len_next;
    }

    // Handle endpoints and degenerate cases
    if len_prev < 0.0001 && len_next < 0.0001 {
        // Both directions are zero - degenerate vertex, use arbitrary direction
        dir_from_prev = vec2<f32>(1.0, 0.0);
        dir_to_next = vec2<f32>(1.0, 0.0);
    } else if len_prev < 0.0001 {
        dir_from_prev = dir_to_next;
    } else if len_next < 0.0001 {
        dir_to_next = dir_from_prev;
    }

    // Compute tangent as bisector of the two directions
    let tangent_sum = dir_from_prev + dir_to_next;
    let tangent_len = length(tangent_sum);

    // Handle 180° turn (directions cancel out) - use perpendicular to incoming direction
    var tangent: vec2<f32>;
    var miter: vec2<f32>;
    var miter_scale: f32 = 1.0;

    if tangent_len < 0.0001 {
        // 180° turn: use perpendicular to incoming direction
        tangent = vec2<f32>(-dir_from_prev.y, dir_from_prev.x);
        miter = dir_from_prev;  // Perpendicular to tangent
        miter_scale = 1.0;  // No miter extension for 180° turn (bevel)
    } else {
        tangent = tangent_sum / tangent_len;
        miter = vec2<f32>(-tangent.y, tangent.x);

        // Miter length calculation (avoid spikes at sharp corners)
        let normal_prev = vec2<f32>(-dir_from_prev.y, dir_from_prev.x);
        let cos_half = max(abs(dot(miter, normal_prev)), 0.1);
        miter_scale = min(1.0 / cos_half, u.join_limit);
    }

    // Convert pixel width to NDC offset
    let px_to_ndc = 2.0 / camera.view_size;
    let offset = miter * in.side * u.line_width_px * 0.5 * miter_scale * px_to_ndc;

    let final_pos = ndc_curr + offset;

    var out: VertexOutput;
    out.position = vec4<f32>(final_pos, 0.0, 1.0);
    out.position.z = 1.0 - (u.disp_prio / 10.0);
    out.arc_len = in.arc_len;
    // This stage does its own perspective divide so the stroke keeps a constant
    // pixel width, which means it cannot rely on clip-space culling: a vertex
    // behind the tilted camera divides by a negative w and lands back on screen
    // mirrored. Push it out of the depth range instead so the GPU drops it.
    if clip_curr.w <= 0.0 {
        out.position.z = 2.0;
    }
    return out;
}

fn segment_distance(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>) -> f32 {
    let ab = b - a;
    let len2 = dot(ab, ab);
    var t = 0.0;
    if len2 > 0.0 {
        t = clamp(dot(p - a, ab) / len2, 0.0, 1.0);
    }
    return length(p - (a + ab * t));
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // Convert arc_len (meters) to screen pixels
    let arc_px = in.arc_len * camera.pixels_per_meter;
    let pattern_len = u.dash_on_px + u.dash_off_px;

    // Apply dash pattern: discard pixels in the "off" (gap) portion
    if pattern_len > 0.0 && u.dash_off_px > 0.0 {
        let pos_in_pattern = arc_px % pattern_len;
        if pos_in_pattern > u.dash_on_px {
            // In the gap portion
            if u.dot_on_px > 0.0 {
                // DASD: check if we're in the centered dot within the gap
                let gap_pos = pos_in_pattern - u.dash_on_px;
                let gap_center = u.dash_off_px * 0.5;
                let half_dot = u.dot_on_px * 0.5;
                if gap_pos < gap_center - half_dot || gap_pos > gap_center + half_dot {
                    discard;
                }
                // else: inside the dot, draw it
            } else {
                discard;
            }
        }
    }

    return palette[u.color_index];
}

// ---------------------------------------------------------------------------
// LC() lines. One instance per straight segment of the line. A repeat of the
// symbol sits at every multiple of lc_advance_px of arc length; each segment
// draws — whole, turned with the segment — the repeats that fall on it, so its
// quad covers just those, `u.line_width_px / 2` either side. All in pixels at
// the current zoom.

struct LcInstance {
    @location(0) p0: vec2<f32>,    // metres from the tile centre
    @location(1) p1: vec2<f32>,
    @location(2) arc0: f32,        // arc length at p0 along the whole line, m
}

struct LcOutput {
    @builtin(position) position: vec4<f32>,
    // Pixels from p0: along the segment, and to its left.
    @location(0) local: vec2<f32>,
    // Arc length at p0 and the segment's length along the line, pixels.
    @location(1) @interpolate(flat) seg: vec2<f32>,
}

@vertex
fn vs_lc(@builtin(vertex_index) vi: u32, inst: LcInstance) -> LcOutput {
    var out: LcOutput;
    let c0 = camera.view_proj * vec4<f32>(inst.p0, 0.0, 1.0);
    let c1 = camera.view_proj * vec4<f32>(inst.p1, 0.0, 1.0);
    // Behind a tilted eye: drop it (see vs_main).
    if c0.w <= 0.0 || c1.w <= 0.0 {
        out.position = vec4<f32>(0.0, 0.0, 2.0, 1.0);
        return out;
    }
    let half_view = camera.view_size * 0.5;
    let s0 = c0.xy / c0.w * half_view;
    let s1 = c1.xy / c1.w * half_view;
    let d = s1 - s0;
    let len = length(d);
    var t = vec2<f32>(1.0, 0.0);
    if len > 1e-4 {
        t = d / len;
    }
    let n = vec2<f32>(-t.y, t.x);
    let e = u.line_width_px * 0.5;

    // The repeats this segment owns: those placed in [start, end), measured
    // along the line in metres, so neighbouring segments (and tiles) share
    // their ends exactly.
    let adv = u.lc_advance_px;
    let start = inst.arc0 * camera.pixels_per_meter;
    let end = (inst.arc0 + length(inst.p1 - inst.p0)) * camera.pixels_per_meter;
    let k_first = ceil(start / adv);
    let k_last = ceil(end / adv) - 1.0;
    if k_last < k_first {
        out.position = vec4<f32>(0.0, 0.0, 2.0, 1.0);
        return out;
    }
    let first_at = k_first * adv - start;
    let last_at = k_last * adv - start;

    var corner: vec2<f32>;
    switch (vi) {
        case 0u: { corner = vec2(0.0, 0.0); }
        case 1u: { corner = vec2(1.0, 0.0); }
        case 2u: { corner = vec2(0.0, 1.0); }
        case 3u: { corner = vec2(0.0, 1.0); }
        case 4u: { corner = vec2(1.0, 0.0); }
        default: { corner = vec2(1.0, 1.0); }
    }
    let along = mix(first_at + u.lc_x_range.x, last_at + u.lc_x_range.y, corner.x);
    let across = mix(-e, e, corner.y);
    let p = s0 + t * along + n * across;
    out.position = vec4<f32>(p / half_view, 0.5, 1.0);
    out.local = vec2<f32>(along, across);
    out.seg = vec2<f32>(start, end - start);
    return out;
}

@fragment
fn fs_lc(in: LcOutput) -> @location(0) vec4<f32> {
    let adv = u.lc_advance_px;
    let start = in.seg.x;
    let end = in.seg.x + in.seg.y;
    let a = start + in.local.x;
    // The repeats whose drawing can reach this pixel — placed between
    // a - reach_after and a - reach_before — that this segment owns.
    let k_lo = max(ceil((a - u.lc_x_range.y) / adv), ceil(start / adv));
    let k_hi = min(floor((a - u.lc_x_range.x) / adv), ceil(end / adv) - 1.0);
    var cov = 0.0;
    for (var j = 0; j < 4; j++) {
        let k = k_lo + f32(j);
        if k > k_hi {
            break;
        }
        let s = k * adv;
        let p = vec2<f32>(a - s, in.local.y);
        for (var i = 0u; i < u.lc_count; i++) {
            let seg = lc_segments[2u * (u.lc_first + i)];
            let half_stroke = lc_segments[2u * (u.lc_first + i) + 1u].x;
            let dist = segment_distance(p, seg.xy, seg.zw);
            cov = max(cov, clamp(half_stroke + 0.5 - dist, 0.0, 1.0));
        }
    }
    if cov <= 0.0 {
        discard;
    }
    let c = palette[u.color_index];
    return vec4<f32>(c.rgb, c.a * cov);
}
