//! Symbol rendering with atlas-based instancing.
//!
//! Renders nautical chart symbols (buoys, beacons, rocks) using a single
//! draw call with instanced quads that sample from a pre-baked atlas texture.

use std::collections::HashMap;
use wgpu::util::DeviceExt;
use serde::Deserialize;
use image::GenericImageView;

/// Symbol identifier - maps S-57 objects to atlas positions
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum SymbolId {
    BuoyLatPort = 0,        // BOYLAT14 - red (IALA-A port)
    BuoyLatStarboard = 1,   // BOYLAT13 - green (IALA-A starboard)
    BeaconLatPort = 2,      // BCNLAT21 - red beacon
    BeaconLatStarboard = 3, // BCNLAT15 - green beacon
    RockAwash = 4,          // UWTROC03 - rock awash (WATLEV=3)
    RockSubmerged = 5,      // UWTROC04 - submerged rock
    BuoyCarNorth = 6,       // BOYCAR01 - north cardinal buoy
    BuoyCarEast = 7,        // BOYCAR02 - east cardinal buoy
    BuoyCarSouth = 8,       // BOYCAR03 - south cardinal buoy
    BuoyCarWest = 9,        // BOYCAR04 - west cardinal buoy
    Unknown = 255,
}

impl SymbolId {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "BuoyLatPort" => Some(Self::BuoyLatPort),
            "BuoyLatStarboard" => Some(Self::BuoyLatStarboard),
            "BeaconLatPort" => Some(Self::BeaconLatPort),
            "BeaconLatStarboard" => Some(Self::BeaconLatStarboard),
            "RockAwash" => Some(Self::RockAwash),
            "RockSubmerged" => Some(Self::RockSubmerged),
            "BuoyCarNorth" => Some(Self::BuoyCarNorth),
            "BuoyCarEast" => Some(Self::BuoyCarEast),
            "BuoyCarSouth" => Some(Self::BuoyCarSouth),
            "BuoyCarWest" => Some(Self::BuoyCarWest),
            _ => None,
        }
    }
}

/// Per-instance data sent to GPU
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct SymbolInstance {
    /// World position in SM (Simple Mercator) meters
    pub position: [f32; 2],
    /// Symbol ID (index into metadata)
    pub symbol_id: u32,
    /// Rotation in radians (0 = north up)
    pub rotation: f32,
}

impl SymbolInstance {
    const ATTRIBS: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![
        0 => Float32x2,  // position
        1 => Uint32,     // symbol_id
        2 => Float32,    // rotation
    ];

    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &Self::ATTRIBS,
        }
    }
}

/// Symbol metadata for GPU (matches WGSL SymbolMeta)
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SymbolMetaGpu {
    /// [uv_min_x, uv_min_y, uv_max_x, uv_max_y]
    uv_rect: [f32; 4],
    /// [pivot_x, pivot_y, width_px, height_px]
    pivot_size: [f32; 4],
}

/// Atlas JSON format
#[derive(Deserialize)]
struct AtlasJson {
    #[allow(dead_code)]
    version: u32,
    atlas_size: [u32; 2],
    cell_size: [u32; 2],
    symbols: HashMap<String, SymbolEntry>,
}

#[derive(Deserialize)]
struct SymbolEntry {
    cell: [u32; 2],
    size: [u32; 2],
    pivot: [f32; 2],
    #[allow(dead_code)]
    description: Option<String>,
}

/// Maximum number of symbol types
const MAX_SYMBOL_TYPES: usize = 16;
/// Maximum symbols per chart
const MAX_INSTANCES: usize = 16384;

/// Symbol renderer with atlas and instancing
pub struct SymbolRenderer {
    pipeline: wgpu::RenderPipeline,
    atlas_bind_group: wgpu::BindGroup,
    instance_buffer: wgpu::Buffer,
    instance_count: u32,
    // Bind group for camera (set from RenderState)
    camera_bind_group_layout: wgpu::BindGroupLayout,
    metadata_buffer: wgpu::Buffer,
}

impl SymbolRenderer {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        surface_format: wgpu::TextureFormat,
        camera_bind_group_layout: &wgpu::BindGroupLayout,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Load atlas PNG
        let atlas_png = include_bytes!("../../assets/symbols/atlas.png");
        let atlas_json = include_str!("../../assets/symbols/atlas.json");

        // Parse JSON
        let atlas_data: AtlasJson = serde_json::from_str(atlas_json)?;

        // Load PNG
        let img = image::load_from_memory(atlas_png)?;
        let rgba = img.to_rgba8();
        let (width, height) = img.dimensions();

        // Create texture
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("symbol_atlas"),
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

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("symbol_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // Build metadata array indexed by SymbolId
        let atlas_w = atlas_data.atlas_size[0] as f32;
        let atlas_h = atlas_data.atlas_size[1] as f32;
        let cell_w = atlas_data.cell_size[0] as f32;
        let cell_h = atlas_data.cell_size[1] as f32;

        let mut metadata = [SymbolMetaGpu {
            uv_rect: [0.0; 4],
            pivot_size: [0.5, 0.5, 32.0, 32.0],
        }; MAX_SYMBOL_TYPES];

        for (name, entry) in &atlas_data.symbols {
            if let Some(id) = SymbolId::from_name(name) {
                let idx = id as usize;
                if idx < MAX_SYMBOL_TYPES {
                    let x = entry.cell[0] as f32 * cell_w;
                    let y = entry.cell[1] as f32 * cell_h;
                    let w = entry.size[0] as f32;
                    let h = entry.size[1] as f32;

                    metadata[idx] = SymbolMetaGpu {
                        uv_rect: [
                            x / atlas_w,           // uv_min_x
                            y / atlas_h,           // uv_min_y
                            (x + w) / atlas_w,     // uv_max_x
                            (y + h) / atlas_h,     // uv_max_y
                        ],
                        pivot_size: [
                            entry.pivot[0],
                            entry.pivot[1],
                            w,
                            h,
                        ],
                    };
                }
            }
        }

        // Create metadata storage buffer
        let metadata_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("symbol_metadata"),
            contents: bytemuck::cast_slice(&metadata),
            usage: wgpu::BufferUsages::STORAGE,
        });

        // Atlas bind group layout
        let atlas_bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("symbol_atlas_layout"),
            entries: &[
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
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let atlas_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("symbol_atlas_bind_group"),
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

        // Create extended camera bind group layout that includes metadata
        let symbol_camera_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("symbol_camera_layout"),
            entries: &[
                // Camera uniforms (same as main camera)
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                // Symbol metadata (storage buffer)
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX,
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
            label: Some("symbol_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../../assets/shaders/symbol.wgsl").into()),
        });

        // Pipeline layout
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("symbol_pipeline_layout"),
            bind_group_layouts: &[
                &symbol_camera_layout,   // @group(0) - camera + metadata
                &atlas_bind_group_layout, // @group(1) - atlas texture
            ],
            push_constant_ranges: &[],
        });

        // Render pipeline
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("symbol_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[SymbolInstance::desc()],
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

        // Instance buffer (pre-allocated)
        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("symbol_instances"),
            size: (MAX_INSTANCES * std::mem::size_of::<SymbolInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            pipeline,
            atlas_bind_group,
            instance_buffer,
            instance_count: 0,
            camera_bind_group_layout: symbol_camera_layout,
            metadata_buffer,
        })
    }

    /// Update symbol instances from features
    pub fn update_instances(&mut self, queue: &wgpu::Queue, instances: &[SymbolInstance]) {
        self.instance_count = instances.len().min(MAX_INSTANCES) as u32;

        if !instances.is_empty() {
            queue.write_buffer(
                &self.instance_buffer,
                0,
                bytemuck::cast_slice(&instances[..self.instance_count as usize]),
            );
        }
    }

    /// Create bind group for this renderer using camera buffer
    pub fn create_camera_bind_group(
        &self,
        device: &wgpu::Device,
        camera_buffer: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("symbol_camera_bind_group"),
            layout: &self.camera_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: camera_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.metadata_buffer.as_entire_binding(),
                },
            ],
        })
    }

    /// Render symbols
    pub fn render<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        camera_bind_group: &'a wgpu::BindGroup,
    ) {
        if self.instance_count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, camera_bind_group, &[]);
        render_pass.set_bind_group(1, &self.atlas_bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.instance_buffer.slice(..));

        // 6 vertices per quad (procedurally generated), N instances
        render_pass.draw(0..6, 0..self.instance_count);
    }
}
