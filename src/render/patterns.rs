//! Pattern fill rendering for S-52 area features (AP instruction).
//!
//! Renders tiled patterns on area polygons using a texture atlas.
//! Supports both linear (grid) and staggered (brick) pattern layouts.

use std::collections::HashMap;
use std::sync::OnceLock;
use wgpu::util::DeviceExt;
use serde::Deserialize;
use image::GenericImageView;

/// Global pattern name to index lookup table
static PATTERN_LOOKUP: OnceLock<HashMap<String, u32>> = OnceLock::new();

/// Get the global pattern lookup table, loading it on first access
pub fn get_pattern_lookup() -> &'static HashMap<String, u32> {
    PATTERN_LOOKUP.get_or_init(|| {
        let atlas_json = include_str!("../../assets/patterns/atlas.json");
        let atlas_data: PatternAtlasJson = serde_json::from_str(atlas_json)
            .expect("Failed to parse patterns atlas.json");

        let mut lookup = HashMap::with_capacity(atlas_data.patterns.len());
        for (idx, name) in atlas_data.patterns.keys().enumerate() {
            lookup.insert(name.clone(), idx as u32);
        }

        log::info!("Loaded {} patterns into atlas lookup", lookup.len());
        lookup
    })
}

/// Look up a pattern ID from its S-52 name (e.g., "DRGARE01", "DIAMOND1")
pub fn pattern_id_from_name(name: &str) -> Option<u32> {
    get_pattern_lookup().get(name).copied()
}

/// Per-vertex data for pattern-filled areas
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct PatternVertex {
    /// World position in SM (Simple Mercator) meters
    pub position: [f32; 2],
    /// Pattern ID (index into metadata)
    pub pattern_id: u32,
    /// Pattern offset from object position (for seamless tiling)
    pub offset: [f32; 2],
}

impl PatternVertex {
    const ATTRIBS: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![
        0 => Float32x2,  // position
        1 => Uint32,     // pattern_id
        2 => Float32x2,  // offset
    ];

    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBS,
        }
    }
}

/// Pattern metadata for GPU (matches WGSL PatternMeta)
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PatternMetaGpu {
    /// [uv_min_x, uv_min_y, uv_max_x, uv_max_y]
    uv_rect: [f32; 4],
    /// [tile_width_px, tile_height_px, stagger_factor, _pad]
    tile_info: [f32; 4],
}

/// Pattern atlas JSON format
#[derive(Deserialize, Clone)]
struct PatternAtlasJson {
    #[allow(dead_code)]
    version: u32,
    atlas_size: [u32; 2],
    patterns: HashMap<String, PatternEntry>,
}

/// Pattern entry in atlas
#[derive(Deserialize, Clone)]
struct PatternEntry {
    /// [x, y, width, height] in atlas pixels
    rect: [u32; 4],
    /// [width, height] of original HPGL tile in plotting units
    #[allow(dead_code)]
    tile_size: [u32; 2],
    #[allow(dead_code)]
    pivot: [i32; 2],
    #[allow(dead_code)]
    origin: [i32; 2],
    /// "S" for staggered (brick), "L" for linear (grid)
    fill_type: String,
    #[allow(dead_code)]
    min_dist: u32,
    #[allow(dead_code)]
    max_dist: u32,
}

/// Maximum number of pattern types
const MAX_PATTERN_TYPES: usize = 64;
/// Maximum pattern vertices per frame
const MAX_VERTICES: usize = 65536;

/// Pattern renderer with atlas and tiling
pub struct PatternRenderer {
    pipeline: wgpu::RenderPipeline,
    atlas_bind_group: wgpu::BindGroup,
    vertex_buffer: wgpu::Buffer,
    vertex_count: u32,
    camera_bind_group_layout: wgpu::BindGroupLayout,
    #[allow(dead_code)]
    metadata_buffer: wgpu::Buffer,
}

impl PatternRenderer {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        surface_format: wgpu::TextureFormat,
        _camera_bind_group_layout: &wgpu::BindGroupLayout,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Load atlas PNG
        let atlas_png = include_bytes!("../../assets/patterns/atlas.png");
        let atlas_json = include_str!("../../assets/patterns/atlas.json");

        // Parse JSON
        let atlas_data: PatternAtlasJson = serde_json::from_str(atlas_json)?;

        // Load PNG
        let img = image::load_from_memory(atlas_png)?;
        let rgba = img.to_rgba8();
        let (width, height) = img.dimensions();

        // Create texture
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("pattern_atlas"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
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
            &rgba,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(4 * width),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        // Use Repeat address mode for pattern tiling
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("pattern_sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        // Build metadata array indexed by pattern index
        let atlas_w = atlas_data.atlas_size[0] as f32;
        let atlas_h = atlas_data.atlas_size[1] as f32;

        let pattern_names: Vec<_> = atlas_data.patterns.keys().collect();
        let pattern_count = pattern_names.len().min(MAX_PATTERN_TYPES);

        let mut metadata = vec![PatternMetaGpu {
            uv_rect: [0.0; 4],
            tile_info: [32.0, 32.0, 0.0, 0.0],
        }; MAX_PATTERN_TYPES];

        // HPGL scale: 100 units = 1mm, we render at 0.1 scale = 10 pixels per 100 HPGL units
        const _HPGL_SCALE: f32 = 0.1;

        for (idx, name) in pattern_names.iter().enumerate() {
            if idx >= MAX_PATTERN_TYPES {
                break;
            }
            let entry = &atlas_data.patterns[*name];
            let rect = entry.rect;

            // Rendered tile size in pixels (from atlas rect)
            let tile_w_px = rect[2] as f32;
            let tile_h_px = rect[3] as f32;

            // Stagger factor: 0.5 for staggered (brick), 0.0 for linear (grid)
            let stagger = if entry.fill_type == "S" { 0.5 } else { 0.0 };

            metadata[idx] = PatternMetaGpu {
                uv_rect: [
                    rect[0] as f32 / atlas_w,                     // uv_min_x
                    rect[1] as f32 / atlas_h,                     // uv_min_y
                    (rect[0] + rect[2]) as f32 / atlas_w,         // uv_max_x
                    (rect[1] + rect[3]) as f32 / atlas_h,         // uv_max_y
                ],
                tile_info: [
                    tile_w_px,
                    tile_h_px,
                    stagger,
                    0.0,  // padding
                ],
            };
        }

        log::info!("Loaded {} pattern metadata entries", pattern_count);

        // Create metadata buffer
        let metadata_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pattern_metadata"),
            contents: bytemuck::cast_slice(&metadata),
            usage: wgpu::BufferUsages::STORAGE,
        });

        // Create camera bind group layout (group 0)
        let camera_bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pattern_camera_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        // Create atlas bind group layout (group 1)
        let atlas_bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pattern_atlas_layout"),
            entries: &[
                // texture
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                // sampler
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                // metadata storage buffer
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        // Create atlas bind group
        let atlas_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pattern_atlas_bind_group"),
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
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: metadata_buffer.as_entire_binding(),
                },
            ],
        });

        // Load shader
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pattern_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../../assets/shaders/pattern.wgsl").into()),
        });

        // Create pipeline layout
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pattern_pipeline_layout"),
            bind_group_layouts: &[&camera_bind_group_layout, &atlas_bind_group_layout],
            push_constant_ranges: &[],
        });

        // Create vertex buffer (will be filled per-frame)
        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pattern_vertex_buffer"),
            size: (MAX_VERTICES * std::mem::size_of::<PatternVertex>()) as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Create pipeline
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("pattern_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[PatternVertex::desc()],
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
                cull_mode: None,  // No culling for 2D
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
            vertex_buffer,
            vertex_count: 0,
            camera_bind_group_layout,
            metadata_buffer,
        })
    }

    /// Get the camera bind group layout for external use
    pub fn camera_bind_group_layout(&self) -> &wgpu::BindGroupLayout {
        &self.camera_bind_group_layout
    }

    /// Update vertices for this frame
    pub fn update(&mut self, queue: &wgpu::Queue, vertices: &[PatternVertex]) {
        let count = vertices.len().min(MAX_VERTICES);
        if count > 0 {
            queue.write_buffer(
                &self.vertex_buffer,
                0,
                bytemuck::cast_slice(&vertices[..count]),
            );
        }
        self.vertex_count = count as u32;
    }

    /// Render all pattern-filled areas
    pub fn render<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        camera_bind_group: &'a wgpu::BindGroup,
    ) {
        if self.vertex_count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, camera_bind_group, &[]);
        render_pass.set_bind_group(1, &self.atlas_bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
        render_pass.draw(0..self.vertex_count, 0..1);
    }

    /// Render pattern-filled areas using an external vertex buffer (for tile-based rendering)
    pub fn render_with_buffer<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        camera_bind_group: &'a wgpu::BindGroup,
        buffer: &'a wgpu::Buffer,
        vertex_count: u32,
    ) {
        if vertex_count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, camera_bind_group, &[]);
        render_pass.set_bind_group(1, &self.atlas_bind_group, &[]);
        render_pass.set_vertex_buffer(0, buffer.slice(..));
        render_pass.draw(0..vertex_count, 0..1);
    }

    /// Render a range of pattern-filled area vertices from an external buffer.
    pub fn render_range_with_buffer<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        camera_bind_group: &'a wgpu::BindGroup,
        buffer: &'a wgpu::Buffer,
        start: u32,
        count: u32,
    ) {
        if count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, camera_bind_group, &[]);
        render_pass.set_bind_group(1, &self.atlas_bind_group, &[]);
        render_pass.set_vertex_buffer(0, buffer.slice(..));
        render_pass.draw(start..start + count, 0..1);
    }

    /// Set pipeline and bind groups once (call before a batch of draw_range calls).
    pub fn set_state<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        camera_bind_group: &'a wgpu::BindGroup,
    ) {
        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, camera_bind_group, &[]);
        render_pass.set_bind_group(1, &self.atlas_bind_group, &[]);
    }

    /// Draw a range of pattern vertices (call set_state first).
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
        render_pass.draw(start..start + count, 0..1);
    }
}

/// Calculate pattern offset from object world position for seamless tiling
/// This ensures patterns on adjacent features align correctly
pub fn calculate_pattern_offset(world_x: f32, world_y: f32, tile_width: f32, tile_height: f32) -> [f32; 2] {
    // Simple modulo to get offset within tile
    let x_offset = world_x % tile_width;
    let y_offset = world_y % tile_height;
    [x_offset, y_offset]
}
