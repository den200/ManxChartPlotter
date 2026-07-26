//! Screen-space panels drawn on the glass: the object-query bubble's backing.
//!
//! Deliberately small. navcore has no UI toolkit and does not need one for a
//! rounded rectangle — the chart already has a text renderer with an SDF atlas,
//! so a panel is a quad behind it. Anything that grows into a real interface
//! (settings, layer switches) should reconsider that, but this is one shape.

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

/// A vertex of a rounded panel, positioned in screen pixels from the top left.
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct OverlayVertex {
    pub position_px: [f32; 2],
    pub color: [f32; 4],
    /// Offset from the panel's centre, in pixels; the fragment stage uses it
    /// to round the corners.
    pub corner: [f32; 2],
    /// `[corner_radius_px, half_width_px, half_height_px, border_px]`
    pub shape: [f32; 4],
}

impl OverlayVertex {
    const ATTRIBS: [wgpu::VertexAttribute; 4] = wgpu::vertex_attr_array![
        0 => Float32x2,
        1 => Float32x4,
        2 => Float32x2,
        3 => Float32x4,
    ];

    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBS,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct OverlayUniform {
    view_size: [f32; 2],
    _pad: [f32; 2],
}

pub struct OverlayRenderer {
    pipeline: wgpu::RenderPipeline,
    uniform_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    vertex_buffer: Option<wgpu::Buffer>,
    vertex_count: u32,
}

impl OverlayRenderer {
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        depth_format: wgpu::TextureFormat,
        samples: u32,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Overlay Shader"),
            source: wgpu::ShaderSource::Wgsl(
                include_str!("../../assets/shaders/overlay.wgsl").into(),
            ),
        });

        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Overlay Uniform"),
            contents: bytemuck::cast_slice(&[OverlayUniform {
                view_size: [1.0, 1.0],
                _pad: [0.0; 2],
            }]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Overlay Bind Group Layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Overlay Bind Group"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Overlay Pipeline Layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Overlay Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[OverlayVertex::desc()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            // Writes no depth: the panel is on the glass, above the whole S-52
            // priority range, and nothing is drawn after it.
            depth_stencil: Some(wgpu::DepthStencilState {
                format: depth_format,
                depth_write_enabled: false,
                depth_compare: wgpu::CompareFunction::Always,
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState {
                count: samples,
                ..Default::default()
            },
            multiview: None,
            cache: None,
        });

        Self {
            pipeline,
            uniform_buffer,
            bind_group,
            vertex_buffer: None,
            vertex_count: 0,
        }
    }

    /// Replace the panels to be drawn. An empty slice draws nothing.
    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view_size: [f32; 2],
        vertices: &[OverlayVertex],
    ) {
        queue.write_buffer(
            &self.uniform_buffer,
            0,
            bytemuck::cast_slice(&[OverlayUniform {
                view_size,
                _pad: [0.0; 2],
            }]),
        );
        self.vertex_count = vertices.len() as u32;
        self.vertex_buffer = (!vertices.is_empty()).then(|| {
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("Overlay Vertices"),
                contents: bytemuck::cast_slice(vertices),
                usage: wgpu::BufferUsages::VERTEX,
            })
        });
    }

    pub fn draw<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>) {
        let Some(buffer) = &self.vertex_buffer else { return };
        if self.vertex_count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, buffer.slice(..));
        pass.draw(0..self.vertex_count, 0..1);
    }
}

/// The six vertices of one rounded panel.
pub fn panel(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
    border: f32,
    color: [f32; 4],
) -> Vec<OverlayVertex> {
    let (hw, hh) = (w * 0.5, h * 0.5);
    let shape = [radius, hw, hh, border];
    let corner = |cx: f32, cy: f32| OverlayVertex {
        position_px: [x + hw + cx * hw, y + hh + cy * hh],
        color,
        corner: [cx * hw, cy * hh],
        shape,
    };
    vec![
        corner(-1.0, -1.0),
        corner(1.0, -1.0),
        corner(1.0, 1.0),
        corner(-1.0, -1.0),
        corner(1.0, 1.0),
        corner(-1.0, 1.0),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_is_two_triangles_covering_the_rect() {
        let v = panel(10.0, 20.0, 100.0, 50.0, 6.0, 1.0, [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(v.len(), 6);
        let xs: Vec<f32> = v.iter().map(|p| p.position_px[0]).collect();
        let ys: Vec<f32> = v.iter().map(|p| p.position_px[1]).collect();
        assert_eq!(xs.iter().cloned().fold(f32::MAX, f32::min), 10.0);
        assert_eq!(xs.iter().cloned().fold(f32::MIN, f32::max), 110.0);
        assert_eq!(ys.iter().cloned().fold(f32::MAX, f32::min), 20.0);
        assert_eq!(ys.iter().cloned().fold(f32::MIN, f32::max), 70.0);
        // The corner offsets run from -half to +half, which is what the
        // rounded-rectangle distance in the shader expects.
        assert_eq!(v[0].corner, [-50.0, -25.0]);
        assert_eq!(v[2].corner, [50.0, 25.0]);
    }
}
