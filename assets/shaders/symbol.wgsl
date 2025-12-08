// Symbol rendering shader
// Uses instanced quads with atlas lookup
// Each symbol is a quad with screen-space fixed size

struct CameraUniform {
    view_proj: mat4x4<f32>,
    view_size: vec2<f32>,
    pixels_per_meter: f32,
    _pad: f32,
}

struct SymbolMeta {
    // [uv_min_x, uv_min_y, uv_max_x, uv_max_y]
    uv_rect: vec4<f32>,
    // [pivot_x, pivot_y, width_px, height_px]
    pivot_size: vec4<f32>,
}

@group(0) @binding(0) var<uniform> camera: CameraUniform;
@group(0) @binding(1) var<storage, read> symbol_meta: array<SymbolMeta>;

@group(1) @binding(0) var t_atlas: texture_2d<f32>;
@group(1) @binding(1) var s_atlas: sampler;

struct InstanceInput {
    @location(0) position: vec2<f32>,  // World position (SM meters)
    @location(1) symbol_id: u32,       // Index into symbol_meta
    @location(2) rotation: f32,        // Rotation in radians
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) tex_coord: vec2<f32>,
}

// Symbol size in screen pixels (fixed regardless of zoom)
const SYMBOL_SIZE_PIXELS: f32 = 32.0;

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    instance: InstanceInput,
) -> VertexOutput {
    var out: VertexOutput;

    // Get symbol metadata
    let sym_info = symbol_meta[instance.symbol_id];
    let uv_rect = sym_info.uv_rect;
    let pivot = sym_info.pivot_size.xy;
    let size_px = sym_info.pivot_size.zw;

    // Generate quad corners from vertex_index (0-5 for two triangles)
    // Vertices: 0=BL, 1=BR, 2=TL, 3=TL, 4=BR, 5=TR
    var corner: vec2<f32>;
    var uv: vec2<f32>;

    switch (vertex_index) {
        case 0u: { corner = vec2(0.0, 0.0); uv = vec2(uv_rect.x, uv_rect.w); }  // BL
        case 1u: { corner = vec2(1.0, 0.0); uv = vec2(uv_rect.z, uv_rect.w); }  // BR
        case 2u: { corner = vec2(0.0, 1.0); uv = vec2(uv_rect.x, uv_rect.y); }  // TL
        case 3u: { corner = vec2(0.0, 1.0); uv = vec2(uv_rect.x, uv_rect.y); }  // TL
        case 4u: { corner = vec2(1.0, 0.0); uv = vec2(uv_rect.z, uv_rect.w); }  // BR
        case 5u: { corner = vec2(1.0, 1.0); uv = vec2(uv_rect.z, uv_rect.y); }  // TR
        default: { corner = vec2(0.0); uv = vec2(0.0); }
    }

    // Offset from pivot (pivot is normalized [0,1] within symbol)
    let offset = corner - pivot;

    // Symbol size - maintain aspect ratio, scale to SYMBOL_SIZE_PIXELS
    let aspect = size_px.x / max(size_px.y, 1.0);
    let scaled_size = vec2<f32>(
        SYMBOL_SIZE_PIXELS * aspect,
        SYMBOL_SIZE_PIXELS
    );
    let pixel_offset = offset * scaled_size;

    // Apply rotation
    let c = cos(instance.rotation);
    let s = sin(instance.rotation);
    let rotated_offset = vec2<f32>(
        pixel_offset.x * c - pixel_offset.y * s,
        pixel_offset.x * s + pixel_offset.y * c
    );

    // Transform world position to clip space
    let world_pos = vec4<f32>(instance.position, 0.0, 1.0);
    var clip_pos = camera.view_proj * world_pos;

    // Add screen-space offset (symbols stay fixed size)
    // Convert pixel offset to NDC
    let ndc_offset = rotated_offset / camera.view_size * 2.0;
    clip_pos.x += ndc_offset.x * clip_pos.w;
    clip_pos.y += ndc_offset.y * clip_pos.w;

    out.clip_position = clip_pos;
    out.tex_coord = uv;

    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let color = textureSample(t_atlas, s_atlas, in.tex_coord);

    // Discard fully transparent pixels (alpha test)
    if (color.a < 0.1) {
        discard;
    }

    return color;
}
