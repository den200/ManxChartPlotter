// Area Pattern Fill Shader
// Renders tiled patterns for S-52 area features (AP instruction)
// Uses screen-space fragment coordinates with object-based offset for seamless tiling

struct CameraUniform {
    view_proj: mat4x4<f32>,
    view_size: vec2<f32>,
    pixels_per_meter: f32,
    _pad: f32,
}

// Per-pattern metadata
struct PatternMeta {
    // UV rect in atlas [u_min, v_min, u_max, v_max]
    uv_rect: vec4<f32>,
    // [tile_width_px, tile_height_px, stagger_factor, _pad]
    tile_info: vec4<f32>,
    // [origin_x_px - pivot_x_px, origin_y_px - pivot_y_px, _pad, _pad]
    offset_info: vec4<f32>,
}

@group(0) @binding(0) var<uniform> camera: CameraUniform;

@group(1) @binding(0) var t_atlas: texture_2d<f32>;
@group(1) @binding(1) var s_atlas: sampler;
@group(1) @binding(2) var<storage, read> pattern_meta: array<PatternMeta>;

// Per-instance data
struct VertexInput {
    @location(0) position: vec2<f32>,      // World position (SM meters)
    @location(1) pattern_id: u32,           // Index into pattern_meta
    @location(2) offset: vec2<f32>,         // Pattern offset from object position
    @location(3) disp_prio: u32,            // Display priority for depth sorting
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) world_pos: vec2<f32>,
    @location(1) @interpolate(flat) pattern_id: u32,
    @location(2) @interpolate(flat) offset: vec2<f32>,
}

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;

    // Transform world position to clip space
    let world_pos = vec4<f32>(in.position, 0.0, 1.0);
    out.clip_position = camera.view_proj * world_pos;
    out.clip_position.z = (1.0 - f32(in.disp_prio) / 10.0) * out.clip_position.w;

    // Pass through data to fragment shader
    out.world_pos = in.position;
    out.pattern_id = in.pattern_id;
    out.offset = in.offset;

    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let pat_info = pattern_meta[in.pattern_id];

    // Pattern tile size in pixels
    let tile_w = pat_info.tile_info.x;
    let tile_h = pat_info.tile_info.y;
    let stagger = pat_info.tile_info.z;  // 0.0 for linear, 0.5 for staggered

    // Screen-space position (use clip_position which is in screen pixels after viewport transform)
    let frag_x = in.clip_position.x;
    let frag_y = in.clip_position.y;

    // Apply object offset for seamless tiling across features
    let offset_x = in.offset.x + pat_info.offset_info.x;
    let offset_y = in.offset.y + pat_info.offset_info.y;

    // Calculate which row we're in for stagger
    // yOffM is the un-modded y offset (same as offset_y here)
    let row = floor((frag_y + offset_y) / tile_h);

    // Apply stagger for odd rows (brick pattern)
    var stagger_offset = 0.0;
    if (stagger > 0.0 && fract(row / 2.0) < 0.1) {
        stagger_offset = stagger;  // Usually 0.5
    }

    // Position within the tile, in pixels.
    let u_local = fract((frag_x - offset_x) / tile_w + stagger_offset);
    let v_local = fract((frag_y + offset_y) / tile_h);
    let in_tile = vec2<f32>(u_local * tile_w, v_local * tile_h);

    // The glyph occupies only its own size; the rest of the tile is the S-52
    // minimum distance between symbols and must stay clear.
    let sym = pat_info.offset_info.zw;
    if (in_tile.x >= sym.x || in_tile.y >= sym.y) {
        discard;
    }
    let rel = in_tile / sym;

    // Map position within the glyph to the atlas UV rect
    let uv_min = pat_info.uv_rect.xy;
    let uv_max = pat_info.uv_rect.zw;
    let uv = mix(uv_min, uv_max, vec2<f32>(rel.x, 1.0 - rel.y));

    // Sample pattern texture
    let color = textureSample(t_atlas, s_atlas, uv);

    // Discard fully transparent pixels
    if (color.a < 0.1) {
        discard;
    }

    return color;
}
