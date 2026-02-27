//! Text label rendering using a simple bitmap font atlas.

use bytemuck::{Pod, Zeroable};

const GLYPH_W: u32 = 5;
const GLYPH_H: u32 = 7;
const CELL_W: u32 = 8;
const CELL_H: u32 = 8;
const ATLAS_COLS: u32 = 16;
const ATLAS_ROWS: u32 = 6;
const ATLAS_W: u32 = ATLAS_COLS * CELL_W;
const ATLAS_H: u32 = ATLAS_ROWS * CELL_H;

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct LabelGlyphInstance {
    pub position: [f32; 2],
    pub offset_px: [f32; 2],
    pub size_px: [f32; 2],
    pub uv_min: [f32; 2],
    pub uv_max: [f32; 2],
    pub rotation: f32,
    pub color: [f32; 4],
}

impl LabelGlyphInstance {
    const ATTRIBS: [wgpu::VertexAttribute; 7] = wgpu::vertex_attr_array![
        0 => Float32x2, // position
        1 => Float32x2, // offset_px
        2 => Float32x2, // size_px
        3 => Float32x2, // uv_min
        4 => Float32x2, // uv_max
        5 => Float32,   // rotation
        6 => Float32x4, // color
    ];

    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &Self::ATTRIBS,
        }
    }
}

pub struct LabelRenderer {
    pipeline: wgpu::RenderPipeline,
    atlas_bind_group: wgpu::BindGroup,
    camera_bind_group_layout: wgpu::BindGroupLayout,
}

impl LabelRenderer {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        surface_format: wgpu::TextureFormat,
        _camera_bind_group_layout: &wgpu::BindGroupLayout,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let camera_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("label_camera_layout"),
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

        let atlas_data = build_font_atlas();
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("label_font_atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_W,
                height: ATLAS_H,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &atlas_data,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(ATLAS_W),
                rows_per_image: Some(ATLAS_H),
            },
            wgpu::Extent3d {
                width: ATLAS_W,
                height: ATLAS_H,
                depth_or_array_layers: 1,
            },
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("label_font_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let atlas_bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("label_atlas_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let atlas_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("label_atlas_bind_group"),
            layout: &atlas_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("label_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../../assets/shaders/text_atlas.wgsl").into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("label_pipeline_layout"),
            bind_group_layouts: &[&camera_layout, &atlas_bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("label_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[LabelGlyphInstance::desc()],
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
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        Ok(Self {
            pipeline,
            atlas_bind_group,
            camera_bind_group_layout: camera_layout,
        })
    }

    pub fn create_camera_bind_group(
        &self,
        device: &wgpu::Device,
        camera_buffer: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("label_camera_bind_group"),
            layout: &self.camera_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera_buffer.as_entire_binding(),
            }],
        })
    }

    pub fn render_with_buffer<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        camera_bind_group: &'a wgpu::BindGroup,
        instance_buffer: &'a wgpu::Buffer,
        instance_count: u32,
    ) {
        if instance_count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, camera_bind_group, &[]);
        render_pass.set_bind_group(1, &self.atlas_bind_group, &[]);
        render_pass.set_vertex_buffer(0, instance_buffer.slice(..));
        render_pass.draw(0..6, 0..instance_count);
    }
}

pub fn glyph_uv_rect(c: char) -> Option<[f32; 4]> {
    let code = c as u32;
    if !(32..128).contains(&code) {
        return None;
    }
    let idx = code - 32;
    let col = idx % ATLAS_COLS;
    let row = idx / ATLAS_COLS;
    if row >= ATLAS_ROWS {
        return None;
    }
    let u0 = (col * CELL_W) as f32 / ATLAS_W as f32;
    let v0 = (row * CELL_H) as f32 / ATLAS_H as f32;
    let u1 = ((col + 1) * CELL_W) as f32 / ATLAS_W as f32;
    let v1 = ((row + 1) * CELL_H) as f32 / ATLAS_H as f32;
    Some([u0, v0, u1, v1])
}

fn build_font_atlas() -> Vec<u8> {
    let mut data = vec![0u8; (ATLAS_W * ATLAS_H) as usize];

    for ch in 32u8..128u8 {
        let idx = (ch - 32) as usize;
        let col = (idx as u32) % ATLAS_COLS;
        let row = (idx as u32) / ATLAS_COLS;
        let glyph = FONT_5X7[idx];

        let base_x = col * CELL_W + 1;
        let base_y = row * CELL_H + 1;

        for y in 0..GLYPH_H {
            let row_bits = glyph[y as usize];
            for x in 0..GLYPH_W {
                let mask = 1u8 << (GLYPH_W - 1 - x);
                if (row_bits & mask) != 0 {
                    let px = base_x + x;
                    let py = base_y + y;
                    let offset = (py * ATLAS_W + px) as usize;
                    data[offset] = 255;
                }
            }
        }
    }

    data
}

// 5x7 font bitmap for ASCII 32..127. Bits are LSB -> leftmost pixel.
const FONT_5X7: [[u8; 7]; 96] = [
    [0x00,0x00,0x00,0x00,0x00,0x00,0x00], // ' '
    [0x04,0x04,0x04,0x04,0x04,0x00,0x04], // '!'
    [0x0A,0x0A,0x0A,0x00,0x00,0x00,0x00], // '"'
    [0x0A,0x0A,0x1F,0x0A,0x1F,0x0A,0x0A], // '#'
    [0x04,0x0F,0x14,0x0E,0x05,0x1E,0x04], // '$'
    [0x19,0x19,0x02,0x04,0x08,0x13,0x13], // '%'
    [0x0C,0x12,0x14,0x08,0x15,0x12,0x0D], // '&'
    [0x06,0x04,0x08,0x00,0x00,0x00,0x00], // '\''
    [0x02,0x04,0x08,0x08,0x08,0x04,0x02], // '('
    [0x08,0x04,0x02,0x02,0x02,0x04,0x08], // ')'
    [0x00,0x04,0x15,0x0E,0x15,0x04,0x00], // '*'
    [0x00,0x04,0x04,0x1F,0x04,0x04,0x00], // '+'
    [0x00,0x00,0x00,0x00,0x06,0x04,0x08], // ','
    [0x00,0x00,0x00,0x1F,0x00,0x00,0x00], // '-'
    [0x00,0x00,0x00,0x00,0x00,0x0C,0x0C], // '.'
    [0x01,0x02,0x04,0x08,0x10,0x00,0x00], // '/'
    [0x0E,0x11,0x13,0x15,0x19,0x11,0x0E], // '0'
    [0x04,0x0C,0x04,0x04,0x04,0x04,0x0E], // '1'
    [0x0E,0x11,0x01,0x02,0x04,0x08,0x1F], // '2'
    [0x1F,0x02,0x04,0x02,0x01,0x11,0x0E], // '3'
    [0x02,0x06,0x0A,0x12,0x1F,0x02,0x02], // '4'
    [0x1F,0x10,0x1E,0x01,0x01,0x11,0x0E], // '5'
    [0x06,0x08,0x10,0x1E,0x11,0x11,0x0E], // '6'
    [0x1F,0x01,0x02,0x04,0x08,0x08,0x08], // '7'
    [0x0E,0x11,0x11,0x0E,0x11,0x11,0x0E], // '8'
    [0x0E,0x11,0x11,0x0F,0x01,0x02,0x0C], // '9'
    [0x00,0x0C,0x0C,0x00,0x0C,0x0C,0x00], // ':'
    [0x00,0x0C,0x0C,0x00,0x0C,0x04,0x08], // ';'
    [0x02,0x04,0x08,0x10,0x08,0x04,0x02], // '<'
    [0x00,0x00,0x1F,0x00,0x1F,0x00,0x00], // '='
    [0x08,0x04,0x02,0x01,0x02,0x04,0x08], // '>'
    [0x0E,0x11,0x01,0x02,0x04,0x00,0x04], // '?'
    [0x0E,0x11,0x01,0x0D,0x15,0x15,0x0E], // '@'
    [0x0E,0x11,0x11,0x1F,0x11,0x11,0x11], // 'A'
    [0x1E,0x11,0x11,0x1E,0x11,0x11,0x1E], // 'B'
    [0x0E,0x11,0x10,0x10,0x10,0x11,0x0E], // 'C'
    [0x1C,0x12,0x11,0x11,0x11,0x12,0x1C], // 'D'
    [0x1F,0x10,0x10,0x1E,0x10,0x10,0x1F], // 'E'
    [0x1F,0x10,0x10,0x1E,0x10,0x10,0x10], // 'F'
    [0x0E,0x11,0x10,0x17,0x11,0x11,0x0E], // 'G'
    [0x11,0x11,0x11,0x1F,0x11,0x11,0x11], // 'H'
    [0x0E,0x04,0x04,0x04,0x04,0x04,0x0E], // 'I'
    [0x07,0x02,0x02,0x02,0x02,0x12,0x0C], // 'J'
    [0x11,0x12,0x14,0x18,0x14,0x12,0x11], // 'K'
    [0x10,0x10,0x10,0x10,0x10,0x10,0x1F], // 'L'
    [0x11,0x1B,0x15,0x11,0x11,0x11,0x11], // 'M'
    [0x11,0x19,0x15,0x13,0x11,0x11,0x11], // 'N'
    [0x0E,0x11,0x11,0x11,0x11,0x11,0x0E], // 'O'
    [0x1E,0x11,0x11,0x1E,0x10,0x10,0x10], // 'P'
    [0x0E,0x11,0x11,0x11,0x15,0x12,0x0D], // 'Q'
    [0x1E,0x11,0x11,0x1E,0x14,0x12,0x11], // 'R'
    [0x0F,0x10,0x10,0x0E,0x01,0x01,0x1E], // 'S'
    [0x1F,0x04,0x04,0x04,0x04,0x04,0x04], // 'T'
    [0x11,0x11,0x11,0x11,0x11,0x11,0x0E], // 'U'
    [0x11,0x11,0x11,0x11,0x11,0x0A,0x04], // 'V'
    [0x11,0x11,0x11,0x11,0x15,0x1B,0x11], // 'W'
    [0x11,0x11,0x0A,0x04,0x0A,0x11,0x11], // 'X'
    [0x11,0x11,0x0A,0x04,0x04,0x04,0x04], // 'Y'
    [0x1F,0x01,0x02,0x04,0x08,0x10,0x1F], // 'Z'
    [0x0E,0x08,0x08,0x08,0x08,0x08,0x0E], // '['
    [0x10,0x08,0x04,0x02,0x01,0x00,0x00], // '\\'
    [0x0E,0x02,0x02,0x02,0x02,0x02,0x0E], // ']'
    [0x04,0x0A,0x11,0x00,0x00,0x00,0x00], // '^'
    [0x00,0x00,0x00,0x00,0x00,0x00,0x1F], // '_'
    [0x08,0x04,0x02,0x00,0x00,0x00,0x00], // '`'
    [0x00,0x00,0x0E,0x01,0x0F,0x11,0x0F], // 'a'
    [0x10,0x10,0x1E,0x11,0x11,0x11,0x1E], // 'b'
    [0x00,0x00,0x0E,0x11,0x10,0x11,0x0E], // 'c'
    [0x01,0x01,0x0F,0x11,0x11,0x11,0x0F], // 'd'
    [0x00,0x00,0x0E,0x11,0x1F,0x10,0x0E], // 'e'
    [0x06,0x08,0x1E,0x08,0x08,0x08,0x08], // 'f'
    [0x00,0x00,0x0F,0x11,0x11,0x0F,0x01], // 'g'
    [0x10,0x10,0x1E,0x11,0x11,0x11,0x11], // 'h'
    [0x04,0x00,0x0C,0x04,0x04,0x04,0x0E], // 'i'
    [0x02,0x00,0x06,0x02,0x02,0x12,0x0C], // 'j'
    [0x10,0x10,0x12,0x14,0x18,0x14,0x12], // 'k'
    [0x0C,0x04,0x04,0x04,0x04,0x04,0x0E], // 'l'
    [0x00,0x00,0x1A,0x15,0x15,0x15,0x15], // 'm'
    [0x00,0x00,0x1E,0x11,0x11,0x11,0x11], // 'n'
    [0x00,0x00,0x0E,0x11,0x11,0x11,0x0E], // 'o'
    [0x00,0x00,0x1E,0x11,0x11,0x1E,0x10], // 'p'
    [0x00,0x00,0x0F,0x11,0x11,0x0F,0x01], // 'q'
    [0x00,0x00,0x16,0x19,0x10,0x10,0x10], // 'r'
    [0x00,0x00,0x0F,0x10,0x0E,0x01,0x1E], // 's'
    [0x08,0x08,0x1E,0x08,0x08,0x08,0x06], // 't'
    [0x00,0x00,0x11,0x11,0x11,0x11,0x0F], // 'u'
    [0x00,0x00,0x11,0x11,0x11,0x0A,0x04], // 'v'
    [0x00,0x00,0x11,0x11,0x15,0x1B,0x11], // 'w'
    [0x00,0x00,0x11,0x0A,0x04,0x0A,0x11], // 'x'
    [0x00,0x00,0x11,0x11,0x11,0x0F,0x01], // 'y'
    [0x00,0x00,0x1F,0x02,0x04,0x08,0x1F], // 'z'
    [0x02,0x04,0x04,0x08,0x04,0x04,0x02], // '{'
    [0x04,0x04,0x04,0x00,0x04,0x04,0x04], // '|'
    [0x08,0x04,0x04,0x02,0x04,0x04,0x08], // '}'
    [0x08,0x15,0x02,0x00,0x00,0x00,0x00], // '~'
    [0x00,0x00,0x00,0x00,0x00,0x00,0x00], // DEL
];
