// Area Pattern Fill Shader
// Renders tiled patterns for S-52 area features (AP instruction)
// The grid is anchored to the chart; the cell and glyph are a fixed size in
// pixels, as S-52 states pattern spacing in millimetres on the display.

struct CameraUniform {
    view_proj: mat4x4<f32>,
    view_size: vec2<f32>,
    pixels_per_meter: f32,
    // Physical pixels per logical point: 2 on the Retina display the sizes
    // here were calibrated on, 1 on a standard screen such as the Pi's.
    px_per_point: f32,
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

    // Pattern tile size in pixels, fitted on a 2x display and following the
    // display's density from there, like the symbols.
    let density = camera.px_per_point * 0.5;
    let tile_w = pat_info.tile_info.x * density;
    let tile_h = pat_info.tile_info.y * density;
    let stagger = pat_info.tile_info.z;  // 0.0 for linear, 0.5 for staggered

    // Where this fragment sits on the pattern grid, in pixels.
    //
    // Anchored to the chart, not to the window. S-52 states a pattern's symbol
    // size and its minimum spacing in millimetres on the display, so the cell
    // stays a fixed number of pixels at every zoom — but the *grid* has to be
    // pinned to the ground, or the symbols sit still while the chart slides
    // under them. Taking the fragment's screen coordinate did exactly that: the
    // fish of a fish-haven area never moved, at any zoom or pan, as if they
    // were painted on the glass.
    //
    // Mercator metres times pixels-per-metre gives the same units the cell size
    // is in, and using the world origin as the anchor makes neighbouring tiles
    // agree on the grid, so a pattern crosses a tile boundary without a seam.
    // Screen y runs down and world y runs north, hence the negation.
    let grid = vec2<f32>(in.world_pos.x, -in.world_pos.y) * camera.pixels_per_meter;
    let frag_x = grid.x;
    let frag_y = grid.y;

    // Apply object offset for seamless tiling across features
    let offset_x = in.offset.x + pat_info.offset_info.x * density;
    let offset_y = in.offset.y + pat_info.offset_info.y * density;

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
    let sym = pat_info.offset_info.zw * density;
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
