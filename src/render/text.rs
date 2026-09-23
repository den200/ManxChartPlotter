//! Simple text/sounding renderer.
//!
//! Renders depth soundings (SOUNDG) as numeric text using a simple bitmap font.
//! Uses the same instanced rendering pattern as symbol.rs for efficiency.

// TextRenderer for depth soundings

/// A sounding to render with SNDFRM02 flags
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct SoundingInstance {
    /// Position in metres from the draw's origin: the tile centre in a tile
    /// packet (see `tiles::builder::tile_relative`), else the origin of the
    /// camera slot it is drawn with.
    pub position: [f32; 2],
    /// Whole-part depth value in display units (used for digit extraction)
    pub depth: f32,
    /// SNDFRM02 flags:
    /// - Bit 0: is_shallow (use SNDG2 black color)
    /// - Bit 1: is_drying (negative depth, show underscore)
    /// - Bit 2: show_uncertainty (show question mark)
    /// - Bit 3: is_swept (swept depth indicator)
    /// - Bit 4: has_decimal (decimal_digit is valid)
    /// - Bits 5-7: digit_count (1-5)
    /// - Bits 8-11: decimal_digit value (0-9)
    /// - Bits 12-28: whole_part value (0-99999)
    pub flags: u32,
    /// Scale factor for Soft SCAMIN (1.0 = normal, 0.5 = half size)
    pub scale: f32,
    /// S-52 palette color index (SNDG1/SNDG2)
    pub color_index: u32,
}

impl SoundingInstance {
    const ATTRIBS: [wgpu::VertexAttribute; 5] = wgpu::vertex_attr_array![
        0 => Float32x2,  // position
        1 => Float32,    // depth
        2 => Uint32,     // flags
        3 => Float32,    // scale
        4 => Uint32,     // color_index
    ];

    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &Self::ATTRIBS,
        }
    }
}

/// Maximum soundings per chart
const MAX_SOUNDINGS: usize = 65536;

/// Text/sounding renderer
pub struct TextRenderer {
    pipeline: wgpu::RenderPipeline,
    instance_buffer: wgpu::Buffer,
    instance_count: u32,
    camera_bind_group_layout: wgpu::BindGroupLayout,
}

impl TextRenderer {
    pub fn new(
        device: &wgpu::Device,
        surface_format: wgpu::TextureFormat,
        _camera_bind_group_layout: &wgpu::BindGroupLayout,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Create our own camera bind group layout (same as main)
        let text_camera_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("text_camera_layout"),
                entries: &[
                    super::state::camera_layout_entry(0),
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Storage { read_only: true },
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });

        // Load shader
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("sounding_shader"),
            source: wgpu::ShaderSource::Wgsl(
                include_str!("../../assets/shaders/sounding.wgsl").into(),
            ),
        });

        // Pipeline layout
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sounding_pipeline_layout"),
            bind_group_layouts: &[&text_camera_layout],
            push_constant_ranges: &[],
        });

        // Render pipeline - use TriangleList for 6 vertices per digit
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("sounding_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[SoundingInstance::desc()],
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
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth24PlusStencil8,
                depth_write_enabled: true,
                depth_compare: wgpu::CompareFunction::LessEqual,
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

        // Instance buffer (pre-allocated)
        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("sounding_instances"),
            size: (MAX_SOUNDINGS * std::mem::size_of::<SoundingInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            pipeline,
            instance_buffer,
            instance_count: 0,
            camera_bind_group_layout: text_camera_layout,
        })
    }

    /// Update sounding instances
    pub fn update_soundings(&mut self, queue: &wgpu::Queue, soundings: &[SoundingInstance]) {
        self.instance_count = soundings.len().min(MAX_SOUNDINGS) as u32;

        if !soundings.is_empty() {
            queue.write_buffer(
                &self.instance_buffer,
                0,
                bytemuck::cast_slice(&soundings[..self.instance_count as usize]),
            );
        }
    }

    /// Create bind group for camera
    pub fn create_camera_bind_group(
        &self,
        device: &wgpu::Device,
        camera_buffer: &wgpu::Buffer,
        palette_buffer: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("text_camera_bind_group"),
            layout: &self.camera_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: super::state::camera_binding(camera_buffer),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: palette_buffer.as_entire_binding(),
                },
            ],
        })
    }

    /// Render soundings
    pub fn render<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        camera_bind_group: &'a wgpu::BindGroup,
        cam: u32,
    ) {
        if self.instance_count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, camera_bind_group, &[cam]);
        render_pass.set_vertex_buffer(0, self.instance_buffer.slice(..));

        // Each sounding renders as a small quad with up to 4 digits
        // For now, 6 vertices (1 quad) per sounding - shader will draw the depth text
        render_pass.draw(0..6, 0..self.instance_count);
    }

    /// Render soundings from an external instance buffer (tile mode).
    pub fn render_with_buffer<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        camera_bind_group: &'a wgpu::BindGroup,
        cam: u32,
        instance_buffer: &'a wgpu::Buffer,
        instance_count: u32,
    ) {
        if instance_count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, camera_bind_group, &[cam]);
        render_pass.set_vertex_buffer(0, instance_buffer.slice(..));
        render_pass.draw(0..6, 0..instance_count);
    }

    /// Get the number of soundings currently loaded
    pub fn sounding_count(&self) -> u32 {
        self.instance_count
    }
}
