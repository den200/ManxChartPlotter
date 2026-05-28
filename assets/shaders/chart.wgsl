// Chart rendering shader with palette-indexed colors
// Used for rendering land areas and depth areas
// Color lookup via storage buffer enables Day/Dusk/Night switching
// without rebuilding tile geometry.

struct Uniforms {
    view_proj: mat4x4<f32>,
}

@group(0) @binding(0)
var<uniform> uniforms: Uniforms;

@group(0) @binding(1)
var<storage, read> palette: array<vec4<f32>>;

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) color_index: u32,
    @location(2) disp_prio: u32,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
}

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = uniforms.view_proj * vec4<f32>(in.position, 0.0, 1.0);
    out.clip_position.z = 1.0 - (f32(in.disp_prio) / 10.0);
    out.color = palette[in.color_index];
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
