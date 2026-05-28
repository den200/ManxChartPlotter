struct CameraUniform {
    view_proj: mat4x4<f32>,
    view_size: vec2<f32>,
    pixels_per_meter: f32,
    _pad: f32,
}

@group(0) @binding(0) var<uniform> camera: CameraUniform;
@group(0) @binding(1) var<storage, read> palette: array<vec4<f32>>;
@group(1) @binding(0) var t_atlas: texture_2d<f32>;
@group(1) @binding(1) var s_atlas: sampler;

struct InstanceInput {
    @location(0) position: vec2<f32>,
    @location(1) offset_px: vec2<f32>,
    @location(2) size_px: vec2<f32>,
    @location(3) uv_min: vec2<f32>,
    @location(4) uv_max: vec2<f32>,
    @location(5) rotation: f32,
    @location(6) color: vec4<f32>,
    @location(7) color_index: u32,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) color_index: u32,
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    instance: InstanceInput,
) -> VertexOutput {
    var out: VertexOutput;

    var corner: vec2<f32>;
    var uv_corner: vec2<f32>;

    switch (vertex_index) {
        case 0u: { corner = vec2(-0.5, -0.5); uv_corner = vec2(0.0, 1.0); }
        case 1u: { corner = vec2(0.5, -0.5);  uv_corner = vec2(1.0, 1.0); }
        case 2u: { corner = vec2(-0.5, 0.5);  uv_corner = vec2(0.0, 0.0); }
        case 3u: { corner = vec2(-0.5, 0.5);  uv_corner = vec2(0.0, 0.0); }
        case 4u: { corner = vec2(0.5, -0.5);  uv_corner = vec2(1.0, 1.0); }
        case 5u: { corner = vec2(0.5, 0.5);   uv_corner = vec2(1.0, 0.0); }
        default: { corner = vec2(0.0); uv_corner = vec2(0.0); }
    }

    let local = instance.offset_px + corner * instance.size_px;
    let s = sin(instance.rotation);
    let c = cos(instance.rotation);
    let pixel_offset = vec2<f32>(
        local.x * c - local.y * s,
        local.x * s + local.y * c,
    );

    let world_pos = vec4<f32>(instance.position, 0.0, 1.0);
    var clip_pos = camera.view_proj * world_pos;

    let ndc_offset = pixel_offset / camera.view_size * 2.0;
    clip_pos.x += ndc_offset.x * clip_pos.w;
    clip_pos.y += ndc_offset.y * clip_pos.w;
    clip_pos.z = 0.0; // Draw labels on top of everything

    out.clip_position = clip_pos;
    out.uv = mix(instance.uv_min, instance.uv_max, uv_corner);
    out.color = instance.color;
    out.color_index = instance.color_index;

    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let sample = textureSample(t_atlas, s_atlas, in.uv).r;
    // Sharpen the bilinearly-filtered bitmap edges back toward binary ink so
    // strokes stay visually thick across a wide range of scales. Preserves
    // antialiased edges but restores the bold black core the user expects.
    let sharpened = smoothstep(0.30, 0.60, sample);
    if (sharpened < 0.02) {
        discard;
    }
    let color = palette[in.color_index];
    return vec4<f32>(color.rgb, color.a * in.color.a * sharpened);
}
