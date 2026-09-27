//! Symbol rendering with atlas-based instancing.
//!
//! Renders nautical chart symbols (buoys, beacons, rocks) using a single
//! draw call with instanced quads that sample from a pre-baked atlas texture.
//!
//! Supports 1000+ S-52 symbols via dynamic atlas lookup.

use crate::render::debug_render_mode;
use image::GenericImageView;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;
use wgpu::util::DeviceExt;

/// Symbol identifier - kept for backwards compatibility but now uses dynamic index
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

/// Map legacy SymbolId to S-52 symbol name (atlas key).
pub fn symbol_name_from_id(id: SymbolId) -> &'static str {
    match id {
        SymbolId::BuoyLatPort => "BOYLAT14",
        SymbolId::BuoyLatStarboard => "BOYLAT13",
        SymbolId::BeaconLatPort => "BCNLAT21",
        SymbolId::BeaconLatStarboard => "BCNLAT15",
        SymbolId::RockAwash => "UWTROC03",
        SymbolId::RockSubmerged => "UWTROC04",
        SymbolId::BuoyCarNorth => "BOYCAR01",
        SymbolId::BuoyCarEast => "BOYCAR02",
        SymbolId::BuoyCarSouth => "BOYCAR03",
        SymbolId::BuoyCarWest => "BOYCAR04",
        SymbolId::Unknown => "QUESMRK1",
    }
}

/// Global symbol name to index lookup table
static SYMBOL_LOOKUP: OnceLock<HashMap<String, u32>> = OnceLock::new();

/// Get the global symbol lookup table, loading it on first access
pub fn get_symbol_lookup() -> &'static HashMap<String, u32> {
    SYMBOL_LOOKUP.get_or_init(|| {
        let atlas_json = include_str!("../../assets/symbols/atlas.json");
        let atlas_data: AtlasJson =
            serde_json::from_str(atlas_json).expect("Failed to parse atlas.json");

        // Build lookup from symbol name to index
        let mut lookup = HashMap::with_capacity(atlas_data.symbols.len());
        for (idx, name) in ordered_symbol_names(&atlas_data).iter().enumerate() {
            lookup.insert((*name).clone(), idx as u32);
        }

        log::info!("Loaded {} symbols into atlas lookup", lookup.len());
        lookup
    })
}

/// Look up a symbol ID from its S-52 name (e.g., "BOYLAT13", "UWTROC03")
pub fn symbol_id_from_s52_name(name: &str) -> Option<u32> {
    get_symbol_lookup().get(name).copied()
}

/// Per-instance data sent to GPU
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct SymbolInstance {
    /// Position in metres from the draw's origin: the tile centre in a tile
    /// packet (see `tiles::builder::tile_relative`), else the origin of the
    /// camera slot it is drawn with.
    pub position: [f32; 2],
    /// Symbol ID (index into metadata)
    pub symbol_id: u32,
    /// Rotation in radians (0 = north up)
    pub rotation: f32,
    /// Display priority for depth sorting
    pub disp_prio: u32,
    /// Scale factor for Soft SCAMIN (1.0 = normal, 0.5 = half size)
    pub scale: f32,
    /// 1 when `rotation` is a true bearing (S-52's ORIENT, a heading), so the
    /// symbol turns with the chart when it is not north-up; 0 when the symbol
    /// simply stands upright on the screen, as S-52 draws an unrotated one.
    pub true_bearing: u32,
}

impl SymbolInstance {
    const ATTRIBS: [wgpu::VertexAttribute; 6] = wgpu::vertex_attr_array![
        0 => Float32x2,  // position
        1 => Uint32,     // symbol_id
        2 => Float32,    // rotation
        3 => Uint32,     // disp_prio
        4 => Float32,    // scale
        5 => Uint32,     // true_bearing
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

/// Atlas JSON format (v2 - direct pixel coordinates)
#[derive(Deserialize, Clone)]
struct AtlasJson {
    #[allow(dead_code)]
    version: u32,
    #[allow(dead_code)]
    format: Option<String>, // "direct" for v2
    #[allow(dead_code)]
    atlas_size: [u32; 2],
    #[serde(default)]
    cell_size: Option<[u32; 2]>, // v1 only
    symbols: HashMap<String, SymbolEntry>,
}

/// Symbol entry - supports both v1 (cell-based) and v2 (direct) formats
#[derive(Deserialize, Clone)]
struct SymbolEntry {
    // v2 format: direct pixel coordinates [x, y, width, height]
    rect: Option<[u32; 4]>,
    // v1 format: cell-based
    cell: Option<[u32; 2]>,
    /// The size the symbol is *drawn* at, in nominal (3.125 px/mm) pixels.
    ///
    /// Separate from `rect` because a vector symbol is rasterised finer than it
    /// is drawn — the atlas holds it oversampled so magnifying it on a dense
    /// display has real detail to work with. For raster symbols the two are the
    /// same, and for the v1 cell format this is the size within the cell.
    size: Option<[u32; 2]>,
    // Both formats
    pivot: [f32; 2],
    #[allow(dead_code)]
    description: Option<String>,
}

/// Maximum number of symbol types (expanded for full S-52 coverage)
const MAX_SYMBOL_TYPES: usize = 2048;
/// Maximum symbols per chart
const MAX_INSTANCES: usize = 16384;

fn ordered_symbol_names(atlas_data: &AtlasJson) -> Vec<&String> {
    let mut names: Vec<_> = atlas_data.symbols.keys().collect();
    names.sort();
    names
}

/// Symbol renderer with atlas and instancing
pub struct SymbolRenderer {
    pipeline: wgpu::RenderPipeline,
    pipeline_solid: wgpu::RenderPipeline,
    pipeline_no_depth: wgpu::RenderPipeline,
    pipeline_atlas_debug: wgpu::RenderPipeline,
    atlas_bind_group: wgpu::BindGroup,
    atlas_texture: wgpu::Texture,
    atlas_size: [u32; 2],
    instance_buffer: wgpu::Buffer,
    instance_count: u32,
    // Bind group for camera (set from RenderState)
    camera_bind_group_layout: wgpu::BindGroupLayout,
    metadata_buffer: wgpu::Buffer,
}

fn atlas_png_for_palette(palette_name: &str) -> (&'static [u8], &'static str) {
    match palette_name {
        "DUSK" => (
            include_bytes!("../../assets/symbols/atlas-dusk.png"),
            "atlas-dusk.png",
        ),
        "NIGHT" => (
            include_bytes!("../../assets/symbols/atlas-dark.png"),
            "atlas-dark.png",
        ),
        _ => (include_bytes!("../../assets/symbols/atlas.png"), "atlas.png"),
    }
}

impl SymbolRenderer {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        surface_format: wgpu::TextureFormat,
        _camera_bind_group_layout: &wgpu::BindGroupLayout,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Load atlas PNG
        let (atlas_png, _) = atlas_png_for_palette("DAY_BRIGHT");
        let atlas_json = include_str!("../../assets/symbols/atlas.json");

        // Parse JSON
        let atlas_data: AtlasJson = serde_json::from_str(atlas_json)?;

        // Load PNG
        let img = image::load_from_memory(atlas_png)?;
        let rgba = img.to_rgba8();
        let (width, height) = img.dimensions();

        // Create texture
        let atlas_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("symbol_atlas"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // UNORM (not sRGB): the surface is now UNORM and shaders pass colours
            // through verbatim. The atlas PNG bytes are sRGB display values; an
            // sRGB texture would be linearised on sample and then stored darkened
            // on the UNORM surface. Keep it UNORM so symbol colours pass through.
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &atlas_texture,
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
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        let view = atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("symbol_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // Build metadata array indexed by symbol index
        let atlas_w = width as f32;
        let atlas_h = height as f32;
        let cell_w = atlas_data.cell_size.map(|c| c[0] as f32).unwrap_or(64.0);
        let cell_h = atlas_data.cell_size.map(|c| c[1] as f32).unwrap_or(64.0);

        // Initialize lookup table (must match order in get_symbol_lookup)
        let symbol_names = ordered_symbol_names(&atlas_data);
        let symbol_count = symbol_names.len().min(MAX_SYMBOL_TYPES);

        let mut metadata = vec![
            SymbolMetaGpu {
                uv_rect: [0.0; 4],
                pivot_size: [0.5, 0.5, 32.0, 32.0],
            };
            MAX_SYMBOL_TYPES
        ];

        for (idx, name) in symbol_names.iter().enumerate() {
            if idx >= MAX_SYMBOL_TYPES {
                break;
            }
            let entry = &atlas_data.symbols[*name];

            // Handle both v1 (cell-based) and v2 (direct rect) formats
            let (x, y, w, h) = if let Some(rect) = entry.rect {
                // v2: direct pixel coordinates [x, y, width, height]
                (
                    rect[0] as f32,
                    rect[1] as f32,
                    rect[2] as f32,
                    rect[3] as f32,
                )
            } else if let (Some(cell), Some(size)) = (entry.cell, entry.size) {
                // v1: cell-based
                (
                    cell[0] as f32 * cell_w,
                    cell[1] as f32 * cell_h,
                    size[0] as f32,
                    size[1] as f32,
                )
            } else {
                // Fallback to default
                (0.0, 0.0, 32.0, 32.0)
            };

            metadata[idx] = SymbolMetaGpu {
                uv_rect: [
                    x / atlas_w,       // uv_min_x
                    y / atlas_h,       // uv_min_y
                    (x + w) / atlas_w, // uv_max_x
                    (y + h) / atlas_h, // uv_max_y
                ],
                // Display size, which is the atlas rect only when the symbol
                // was copied from the raster sheet rather than rendered from
                // its vector definition.
                pivot_size: match (entry.rect, entry.size) {
                    (Some(_), Some(size)) => {
                        [entry.pivot[0], entry.pivot[1], size[0] as f32, size[1] as f32]
                    }
                    _ => [entry.pivot[0], entry.pivot[1], w, h],
                },
            };
        }

        log::info!("Built GPU metadata for {} symbols", symbol_count);

        // Create metadata storage buffer
        let metadata_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("symbol_metadata"),
            contents: bytemuck::cast_slice(&metadata),
            usage: wgpu::BufferUsages::STORAGE,
        });

        // Atlas bind group layout
        let atlas_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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
        let symbol_camera_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("symbol_camera_layout"),
                entries: &[
                    // Camera uniforms (same as main camera)
                    super::state::camera_layout_entry(0),
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
            source: wgpu::ShaderSource::Wgsl(
                include_str!("../../assets/shaders/symbol.wgsl").into(),
            ),
        });

        // Pipeline layout
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("symbol_pipeline_layout"),
            bind_group_layouts: &[
                &symbol_camera_layout,    // @group(0) - camera + metadata
                &atlas_bind_group_layout, // @group(1) - atlas texture
            ],
            push_constant_ranges: &[],
        });

        let make_pipeline = |label: &'static str,
                             vertex_entry: &'static str,
                             fragment_entry: &'static str,
                             buffers: &[wgpu::VertexBufferLayout<'_>],
                             depth_stencil: Option<wgpu::DepthStencilState>,
                             cull_mode: Option<wgpu::Face>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: vertex_entry,
                    buffers,
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: fragment_entry,
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
                    cull_mode,
                    polygon_mode: wgpu::PolygonMode::Fill,
                    unclipped_depth: false,
                    conservative: false,
                },
                depth_stencil,
                multisample: wgpu::MultisampleState {
                count: super::msaa_samples(),
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
                multiview: None,
                cache: None,
            })
        };

        let symbol_instance_layout = [SymbolInstance::desc()];

        let depth_stencil = Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth24PlusStencil8,
            depth_write_enabled: true,
            depth_compare: wgpu::CompareFunction::LessEqual,
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        });

        let pipeline = make_pipeline(
            "symbol_pipeline",
            "vs_main",
            "fs_main",
            &symbol_instance_layout,
            depth_stencil.clone(),
            None,
        );

        let pipeline_solid = make_pipeline(
            "symbol_pipeline_solid",
            "vs_main",
            "fs_solid",
            &symbol_instance_layout,
            depth_stencil.clone(),
            None,
        );

        let pipeline_no_depth = make_pipeline(
            "symbol_pipeline_no_depth",
            "vs_main",
            "fs_main",
            &symbol_instance_layout,
            None,
            None,
        );

        let pipeline_atlas_debug = make_pipeline(
            "symbol_pipeline_atlas_debug",
            "vs_atlas_debug",
            "fs_atlas_debug",
            &[],
            None,
            None,
        );

        // Instance buffer (pre-allocated)
        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("symbol_instances"),
            size: (MAX_INSTANCES * std::mem::size_of::<SymbolInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            pipeline,
            pipeline_solid,
            pipeline_no_depth,
            pipeline_atlas_debug,
            atlas_bind_group,
            atlas_texture,
            atlas_size: [width, height],
            instance_buffer,
            instance_count: 0,
            camera_bind_group_layout: symbol_camera_layout,
            metadata_buffer,
        })
    }

    pub fn switch_palette(
        &mut self,
        queue: &wgpu::Queue,
        palette_name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (atlas_png, atlas_label) = atlas_png_for_palette(palette_name);
        let img = image::load_from_memory(atlas_png)?;
        let rgba = img.to_rgba8();
        let (width, height) = img.dimensions();

        if [width, height] != self.atlas_size {
            return Err(format!(
                "symbol atlas '{}' is {}x{}, expected {}x{}",
                atlas_label, width, height, self.atlas_size[0], self.atlas_size[1]
            )
            .into());
        }

        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.atlas_texture,
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
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        log::info!("Switched symbol atlas to {}", atlas_label);
        Ok(())
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
                    resource: super::state::camera_binding(camera_buffer),
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
        cam: u32,
    ) {
        if self.instance_count == 0 {
            return;
        }

        let pipeline = match debug_render_mode() {
            1 => &self.pipeline_solid,
            2 => &self.pipeline_no_depth,
            _ => &self.pipeline,
        };
        render_pass.set_pipeline(pipeline);
        render_pass.set_bind_group(0, camera_bind_group, &[cam]);
        render_pass.set_bind_group(1, &self.atlas_bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.instance_buffer.slice(..));

        // 6 vertices per quad (procedurally generated), N instances
        render_pass.draw(0..6, 0..self.instance_count);
    }

    /// Render symbols from an external instance buffer (for tile mode)
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

        let pipeline = match debug_render_mode() {
            1 => &self.pipeline_solid,
            2 => &self.pipeline_no_depth,
            _ => &self.pipeline,
        };
        render_pass.set_pipeline(pipeline);
        render_pass.set_bind_group(0, camera_bind_group, &[cam]);
        render_pass.set_bind_group(1, &self.atlas_bind_group, &[]);
        render_pass.set_vertex_buffer(0, instance_buffer.slice(..));

        // 6 vertices per quad (procedurally generated), N instances
        render_pass.draw(0..6, 0..instance_count);
    }

    /// Render a range of symbol instances from an external buffer.
    pub fn render_range_with_buffer<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        camera_bind_group: &'a wgpu::BindGroup,
        cam: u32,
        instance_buffer: &'a wgpu::Buffer,
        start: u32,
        count: u32,
    ) {
        if count == 0 {
            return;
        }

        let pipeline = match debug_render_mode() {
            1 => &self.pipeline_solid,
            2 => &self.pipeline_no_depth,
            _ => &self.pipeline,
        };
        render_pass.set_pipeline(pipeline);
        render_pass.set_bind_group(0, camera_bind_group, &[cam]);
        render_pass.set_bind_group(1, &self.atlas_bind_group, &[]);
        render_pass.set_vertex_buffer(0, instance_buffer.slice(..));

        // 6 vertices per quad (procedurally generated), range of instances
        render_pass.draw(0..6, start..start + count);
    }

    /// Set pipeline and bind groups once (call before a batch of draw_range calls).
    pub fn set_state<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        camera_bind_group: &'a wgpu::BindGroup,
        cam: u32,
    ) {
        let pipeline = match debug_render_mode() {
            1 => &self.pipeline_solid,
            2 => &self.pipeline_no_depth,
            _ => &self.pipeline,
        };
        render_pass.set_pipeline(pipeline);
        render_pass.set_bind_group(0, camera_bind_group, &[cam]);
        render_pass.set_bind_group(1, &self.atlas_bind_group, &[]);
    }

    /// Draw a range of symbol instances (call set_state first).
    pub fn draw_range<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        instance_buffer: &'a wgpu::Buffer,
        start: u32,
        count: u32,
    ) {
        if count == 0 {
            return;
        }
        render_pass.set_vertex_buffer(0, instance_buffer.slice(..));
        render_pass.draw(0..6, start..start + count);
    }

    pub fn render_atlas_debug<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        camera_bind_group: &'a wgpu::BindGroup,
        cam: u32,
    ) {
        render_pass.set_pipeline(&self.pipeline_atlas_debug);
        render_pass.set_bind_group(0, camera_bind_group, &[cam]);
        render_pass.set_bind_group(1, &self.atlas_bind_group, &[]);
        render_pass.draw(0..6, 0..1);
    }

    pub fn instance_count(&self) -> u32 {
        self.instance_count
    }
}
