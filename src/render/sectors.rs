//! Light sector arcs and legs, drawn at a constant size on the screen.
//!
//! S-52 gives a sector light's arc radius and leg length in millimetres on the
//! display (`CA()` / LIGHTS05). They used to be built as ordinary polylines in
//! Mercator metres, converted at the *tile's* scale — so between tile levels
//! the camera's 0.71–1.41× of that scale made them grow and shrink by up to
//! 41%, and they jumped at every level change; at the closest zooms, far past
//! the deepest tile level, they were several times their size.
//!
//! Here each arc or leg is one instance: a centre on the chart plus pixel
//! sizes. The vertex shader puts a screen-aligned quad around the projected
//! centre (as the symbol shader does for symbols) and the fragment shader
//! draws the ring segment or the dashed leg in it, so the size on screen never
//! depends on the zoom.

use bytemuck::{Pod, Zeroable};

/// What a [`SectorInstance`] draws.
pub const SECTOR_ARC: u32 = 0;
/// A radial leg from the centre along `start`, `radius_px` long.
pub const SECTOR_LEG: u32 = 1;

/// One arc or leg of a light's sector figure.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Pod, Zeroable)]
pub struct SectorInstance {
    /// The light, in metres from the tile centre.
    pub position: [f32; 2],
    /// Arc radius, or leg length, in physical pixels.
    pub radius_px: f32,
    /// Stroke width in physical pixels.
    pub width_px: f32,
    /// [start, end] bearings in radians, clockwise from north, end > start.
    /// A leg uses only `start`.
    pub bearings: [f32; 2],
    /// [on, off] dash lengths in pixels; [0, 0] is a solid stroke.
    pub dash_px: [f32; 2],
    /// Colour, as an index into the palette.
    pub color_index: u32,
    /// [`SECTOR_ARC`] or [`SECTOR_LEG`].
    pub kind: u32,
    /// S-52 display priority (draw order within the tile).
    pub disp_prio: u32,
}

impl SectorInstance {
    const ATTRIBS: [wgpu::VertexAttribute; 7] = wgpu::vertex_attr_array![
        0 => Float32x2, // position
        1 => Float32,   // radius_px
        2 => Float32,   // width_px
        3 => Float32x2, // bearings
        4 => Float32x2, // dash_px
        5 => Uint32,    // color_index
        6 => Uint32,    // kind
    ];

    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &Self::ATTRIBS,
        }
    }
}

/// The pipeline for [`SectorInstance`]s. Uses the chart's group 0 (camera
/// slot + palette) and nothing else.
pub struct SectorRenderer {
    pipeline: wgpu::RenderPipeline,
}

impl SectorRenderer {
    pub fn new(
        device: &wgpu::Device,
        surface_format: wgpu::TextureFormat,
        camera_palette_layout: &wgpu::BindGroupLayout,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("sector_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../../assets/shaders/sector.wgsl").into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sector_pipeline_layout"),
            bind_group_layouts: &[camera_palette_layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("sector_pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[SectorInstance::desc()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            // Like the lines they replace: ordered by the priority loop on the
            // CPU, no depth test of their own.
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth24PlusStencil8,
                depth_write_enabled: false,
                depth_compare: wgpu::CompareFunction::Always,
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState {
                count: super::msaa_samples(),
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview: None,
            cache: None,
        });
        Self { pipeline }
    }

    pub fn set_pipeline<'a>(&'a self, render_pass: &mut wgpu::RenderPass<'a>) {
        render_pass.set_pipeline(&self.pipeline);
    }

    /// Draw `count` instances from `start` (group 0 must already be set).
    pub fn draw_range<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        buffer: &'a wgpu::Buffer,
        start: u32,
        count: u32,
    ) {
        if count == 0 {
            return;
        }
        render_pass.set_vertex_buffer(0, buffer.slice(..));
        render_pass.draw(0..6, start..start + count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The WGSL instance layout reads these offsets; keep them in step.
    #[test]
    fn instance_layout_matches_the_shader() {
        assert_eq!(std::mem::size_of::<SectorInstance>(), 44);
        let offsets: Vec<u64> = SectorInstance::ATTRIBS.iter().map(|a| a.offset).collect();
        assert_eq!(offsets, vec![0, 8, 12, 16, 24, 32, 36]);
    }
}
