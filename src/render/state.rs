//! WGPU render state and chart rendering pipeline.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;
use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;
use winit::window::Window;

use super::camera::Camera;
use super::colors::{Color, OCEAN_BACKGROUND};
use super::line_vertices::build_line_vertices_multi_indexed;
use super::s52_styles::{COASTLINE_STYLE, CONTOUR_STYLE, style_for_key};
use super::symbols::SymbolRenderer;
use super::patterns::PatternRenderer;
use super::label::LabelRenderer;
use super::text::TextRenderer;
use super::debug_render_mode;
use crate::cache::CachedDecryptor;
use crate::decrypt::KeyStore;
use crate::s52::{S52Engine, LineStyleKey};
use crate::senc::{ChartData, ChartCatalog, features_to_instances, soundings_to_instances};
use crate::tiles::{TileId, visible_tiles, zoom_from_camera_raw};
use crate::tiles::cache::{TileGpuCache, TileCacheKey, LineBatchGpu};


/// Vertex with position, color index, and display priority
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct Vertex {
    pub position: [f32; 2],
    pub color_index: u32,
    pub disp_prio: u32,
    /// Darkening applied to the palette colour, 0..1 — see `AreaVertex::shade`.
    pub shade: f32,
}

impl Vertex {
    const ATTRIBS: [wgpu::VertexAttribute; 4] = wgpu::vertex_attr_array![
        0 => Float32x2,
        1 => Uint32,
        2 => Uint32,
        3 => Float32,
    ];

    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBS,
        }
    }
}

/// Uniform buffer for view-projection matrix and camera info
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct Uniforms {
    pub view_proj: [[f32; 4]; 4],
    /// View size in pixels [width, height]
    pub view_size: [f32; 2],
    /// Pixels per meter at current zoom
    pub pixels_per_meter: f32,
    /// Padding for alignment
    pub _pad: f32,
}

/// Line vertex with adjacency data for shader-based polyline rendering.
/// 32 bytes per vertex (3×vec2 + 2×f32).
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct LineVertex {
    /// Previous point in polyline (SM meters)
    pub prev: [f32; 2],
    /// Current point in polyline (SM meters)
    pub curr: [f32; 2],
    /// Next point in polyline (SM meters)
    pub next: [f32; 2],
    /// Side: +1.0 or -1.0 for left/right expansion
    pub side: f32,
    /// Cumulative arc length from start of polyline (SM meters)
    pub arc_len: f32,
}

impl LineVertex {
    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<LineVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                // prev: [f32; 2] at offset 0
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // curr: [f32; 2] at offset 8
                wgpu::VertexAttribute {
                    offset: 8,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // next: [f32; 2] at offset 16
                wgpu::VertexAttribute {
                    offset: 16,
                    shader_location: 2,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // side: f32 at offset 24
                wgpu::VertexAttribute {
                    offset: 24,
                    shader_location: 3,
                    format: wgpu::VertexFormat::Float32,
                },
                // arc_len: f32 at offset 28
                wgpu::VertexAttribute {
                    offset: 28,
                    shader_location: 4,
                    format: wgpu::VertexFormat::Float32,
                },
            ],
        }
    }
}

/// Uniforms for line shader (112 bytes).
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct LineUniforms {
    /// View-projection matrix (SM -> clip space)
    pub view_proj: [[f32; 4]; 4],
    /// Viewport size in pixels [width, height]
    pub viewport_size: [f32; 2],
    /// Line width in screen pixels
    pub line_width_px: f32,
    /// Miter limit (bevel fallback when exceeded)
    pub join_limit: f32,
    /// Line color index in palette
    pub color_index: u32,
    /// Dash ON length in pixels
    pub dash_on_px: f32,
    /// Dash OFF length in pixels
    pub dash_off_px: f32,
    /// Pixels per meter at current zoom (for arc_len conversion)
    pub px_per_meter: f32,
    /// Display priority for depth sorting
    pub disp_prio: f32,
    /// Dot on length in screen pixels (>0 for DASD dash-dot pattern)
    pub dot_on_px: f32,
    /// Padding for 16-byte alignment
    pub _pad: [u32; 2],
}

/// Line style configuration for a batch of lines.
#[derive(Clone, Copy, Debug)]
pub struct LineStyle {
    /// Line color RGBA (legacy fallback)
    pub color: Color,
    /// Color index in the S-52 palette
    pub color_index: u32,
    /// Line width in screen pixels
    pub width_px: f32,
    /// Dash on length in screen pixels (0.0 = solid)
    pub dash_on_px: f32,
    /// Dash off (gap) length in screen pixels (0.0 = solid)
    pub dash_off_px: f32,
    /// Dot on length in screen pixels (>0 for DASD dash-dot pattern)
    pub dot_on_px: f32,
}

impl LineStyle {
    /// Create a solid line style
    pub const fn solid(color: Color, width_px: f32) -> Self {
        Self {
            color,
            color_index: 0,
            width_px,
            dash_on_px: 0.0,
            dash_off_px: 0.0,
            dot_on_px: 0.0,
        }
    }

    /// Create a dashed line style with 6px dash, 6px gap
    pub const fn dashed(color: Color, width_px: f32) -> Self {
        Self {
            color,
            color_index: 0,
            width_px,
            dash_on_px: 6.0,
            dash_off_px: 6.0,
            dot_on_px: 0.0,
        }
    }
}

/// A batch of lines with shared style and vertex/index buffers.
pub struct LineBatch {
    /// Style configuration for this batch
    pub style: LineStyle,
    /// GPU vertex buffer
    pub vertex_buffer: wgpu::Buffer,
    /// GPU index buffer (uses primitive restart)
    pub index_buffer: wgpu::Buffer,
    /// Number of indices in the buffer
    pub index_count: u32,
    /// Number of vertices in the buffer
    pub vertex_count: u32,
}

/// The nearest ancestor of `tile` that is already built, if any.
///
/// Walks up the pyramid at most four levels: beyond a 16x magnification a
/// coarser tile tells you nothing useful and it is better to show the clear
/// colour than a smear. Drawn through a scissor rectangle covering the missing
/// tile, this is what stops a zoom change from blanking the map — the old level
/// stands in, coarser, until the new one arrives.
fn nearest_resident_ancestor(tile: TileId, resident: impl Fn(TileId) -> bool) -> Option<TileId> {
    let mut ancestor = tile;
    for _ in 0..4 {
        if ancestor.z == 0 {
            return None;
        }
        ancestor = TileId {
            z: ancestor.z - 1,
            x: ancestor.x / 2,
            y: ancestor.y / 2,
        };
        if resident(ancestor) {
            return Some(ancestor);
        }
    }
    None
}

/// The multisampled colour target the main pass draws into.
fn create_msaa_view(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("MSAA Colour Target"),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: super::msaa_samples(),
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
        .create_view(&wgpu::TextureViewDescriptor::default())
}


/// Complete render state for chart visualization
pub struct RenderState {
    pub window: Arc<Window>,
    pub surface: wgpu::Surface<'static>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub config: wgpu::SurfaceConfiguration,
    pub size: winit::dpi::PhysicalSize<u32>,

    // HiDPI scaling
    /// Display scale factor (1.0 for standard, 2.0 for Retina, etc.)
    pub scale_factor: f32,
    /// Effective pixels per millimeter (4.0 * scale_factor for 96 DPI base)
    pub effective_ppmm: f32,

    // Pipeline
    pub pipeline: wgpu::RenderPipeline,
    pub uniform_buffer: wgpu::Buffer,
    pub uniform_bind_group: wgpu::BindGroup,
    /// Palette color buffer for indexed area colors (Day/Dusk/Night switching)
    pub palette_buffer: wgpu::Buffer,
    pub uniform_bind_group_layout: wgpu::BindGroupLayout,

    // Depth buffer for layer priority
    pub depth_texture: wgpu::Texture,
    /// Multisampled colour target; resolved into the surface each frame.
    msaa_view: wgpu::TextureView,
    pub depth_view: wgpu::TextureView,

    // Geometry
    pub vertex_buffer: Option<wgpu::Buffer>,
    pub vertex_count: u32,

    // Camera
    pub camera: Camera,

    // Symbol rendering
    pub symbol_renderer: Option<SymbolRenderer>,
    pub symbol_camera_bind_group: Option<wgpu::BindGroup>,

    // Pattern fill rendering (area patterns)
    pub pattern_renderer: Option<PatternRenderer>,
    pub pattern_camera_bind_group: Option<wgpu::BindGroup>,

    // Text/sounding rendering
    pub text_renderer: Option<TextRenderer>,
    pub text_camera_bind_group: Option<wgpu::BindGroup>,

    // Label rendering (text atlas)
    pub label_renderer: Option<LabelRenderer>,
    /// egui. Optional only so a headless capture can run without it.
    ui: Option<crate::render::ui::Ui>,
    /// The chart shop, on its own thread. Created on first use, because most
    /// sessions never open it.
    shop: Option<crate::shop::service::ShopService>,
    /// The Signal K stream, on its own thread. Also created on first use — a
    /// chart table with no boat around it should not open a socket.
    signalk: Option<crate::signalk::SignalKService>,
    /// What every boat is doing, as last heard — ours and the AIS traffic.
    fleet: crate::signalk::Fleet,
    /// The camera is tracking the boat, so the catalogue clamp must not drag
    /// it back to the middle of the charts.
    following: bool,
    /// S-52 mariner symbols — own ship and AIS — rebuilt whenever the boats
    /// move, which is why they need an instance buffer of their own rather
    /// than sharing the tiles'.
    mariner_buffer: Option<wgpu::Buffer>,
    mariner_count: u32,
    mariner_symbols: Option<crate::render::mariner::MarinerSymbols>,
    /// Where chart sets live — the directory navcore was pointed at, which is
    /// also where a downloaded set is unpacked so it sits beside the others.
    chart_root: Option<std::path::PathBuf>,
    /// The world position the bubble is pinned to, so it tracks the object as
    /// the chart pans rather than sitting still on the glass.
    pick_anchor: Option<[f32; 2]>,
    pick_objects: Vec<crate::pick::PickedObject>,
    /// Sequence number of a query the worker has not answered yet.
    ///
    /// Until it comes back there is nothing to show, and the event loop has to
    /// keep waking or the answer sits in the channel until the next input —
    /// which is what made the first click report an empty result and the second
    /// one reveal it.
    pick_pending: Option<u64>,

    pub label_camera_bind_group: Option<wgpu::BindGroup>,

    // Stencil-write pipeline for coverage masking (background chart suppression)

    // Background area pipeline (stencil test: pass when stencil==0)
    pub bg_pipeline: wgpu::RenderPipeline,

    // Line rendering (shader-based polylines with per-batch styling)
    pub line_pipeline: wgpu::RenderPipeline,
    // Background line pipeline (stencil test: pass when stencil==0)
    pub bg_line_pipeline: wgpu::RenderPipeline,
    pub line_bind_group_layout: wgpu::BindGroupLayout,
    pub line_uniform_buffer: wgpu::Buffer,
    pub line_bind_group: wgpu::BindGroup,
    /// Alignment for dynamic uniform buffer offsets (from device limits)
    pub line_uniform_align: u32,
    /// Line batches with different styles (coastlines, contours, etc.)
    pub line_batches: Vec<LineBatch>,

    // Global dynamic buffers for decluttered text/labels
    pub global_text_buffer: Option<wgpu::Buffer>,
    pub global_text_count: u32,
    pub global_label_buffer: Option<wgpu::Buffer>,
    pub global_label_count: u32,
    /// Sounding digit symbols, built from the globally decluttered soundings.
    global_sounding_buffer: Option<wgpu::Buffer>,
    global_sounding_count: u32,

    // Tile-based rendering (for multi-chart mode)
    /// Chart catalog (metadata only) - None for single chart mode
    catalog: Option<Arc<ChartCatalog>>,
    /// GPU tile cache with LRU eviction
    tile_cache: TileGpuCache,
    /// Key store for chart decryption (Arc for sharing)
    keys: Option<Arc<KeyStore>>,
    /// Chart decryptor with disk cache — moved to worker in tile mode
    decryptor: Option<CachedDecryptor>,
    /// Background tile worker thread (tile mode only)
    tile_worker: Option<crate::tiles::worker::TileWorkerHandle>,
    /// Set of tile IDs currently requested from the worker (pending results)
    pending_tiles: std::collections::HashSet<TileId>,
    /// Completed tile results deferred from previous frame (upload budget exceeded)
    deferred_tile_results: Vec<crate::tiles::worker::TileResponse>,
    /// Persistent parsed chart cache (keyed by chart.id)
    chart_cache: HashMap<u64, ChartData>,
    /// Current style hash for cache invalidation
    style_hash: u64,
    /// Tile mode active flag
    tile_mode: bool,
    /// Cached visible tiles for current frame (computed once, used in draw)
    visible_tiles_cache: Vec<TileId>,
    /// Current zoom level with hysteresis (sticky — only changes when raw z drifts ≥0.6)
    current_z: u8,
    /// Fallback tiles from previous z-level (drawn while new tiles load after z-change)
    previous_visible_tiles: Vec<TileId>,
    /// What to draw for each visible tile, in order.
    ///
    /// Usually the tile itself. Where a tile is not yet built, the nearest
    /// ancestor that *is* — drawn through a scissor rectangle covering exactly
    /// the missing tile's area, so a coarser tile fills the hole without
    /// spilling over its neighbours. Without this the map goes to the clear
    /// colour on every zoom change and stays there until the whole level is
    /// built, which is seconds.
    tile_draw_list: Vec<(TileCacheKey, Option<[u32; 4]>)>,
    /// Style hash for previous z-level fallback tiles
    previous_style_hash: u64,
    /// S-52 presentation engine for display category filtering
    s52_engine: Option<S52Engine>,
    /// Dirty flag — set on camera move, tile upload, resize; cleared after render
    needs_redraw: bool,
    /// Incremental key for visible-tile text layout inputs
    last_text_layout_key: u64,
    /// Cached unique visible line styles for the current visible tile set
    cached_visible_line_styles: Vec<LineStyleKey>,
    /// Key for cached visible line styles
    cached_visible_line_styles_key: u64,
    /// Key for the last uploaded line-uniform contents
    last_line_uniforms_key: u64,
    /// If set, the draw pass records what it issued (NAVCORE_DUMP_DRAW).
    pub draw_log: Option<std::cell::RefCell<Vec<serde_json::Value>>>,
    /// If set, render() copies the next frame to this PNG path (NAVCORE_SHOT headless capture)
    pending_capture: Option<String>,
}

/// Per-frame report of which visible tiles are resident, pending or known to
/// have no chart data. `NAVCORE_TILE_DEBUG=1`.
fn tile_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("NAVCORE_TILE_DEBUG")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false)
    })
}

fn profile_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("NAVCORE_PROFILE")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false)
    })
}

impl RenderState {
    pub async fn new(window: Arc<Window>) -> Self {
        let size = window.inner_size();
        let scale_factor = window.scale_factor() as f32;
        let effective_ppmm = 4.0 * scale_factor; // 96 DPI base = 4.0 ppmm

        // WGPU setup
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        let surface = instance.create_surface(window.clone()).expect("Failed to create surface");

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .expect("Failed to find adapter");

        let adapter_info = adapter.get_info();
        super::set_msaa_samples(super::choose_msaa_samples(&adapter_info));
        println!(
            "DEBUG: Using GPU: {} ({:?}), MSAA {}x",
            adapter_info.name,
            adapter_info.device_type,
            super::msaa_samples()
        );

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::default(),
                    label: Some("navcore_device"),
                    memory_hints: Default::default(),
                },
                None,
            )
            .await
            .expect("Failed to create device");

        let surface_caps = surface.get_capabilities(&adapter);
        // Prefer a NON-sRGB (UNORM) surface. S-52 palette colors (from
        // chartsymbols.xml) are already authored as 8-bit sRGB display values,
        // and the shaders emit them directly. With an sRGB surface, wgpu would
        // sRGB-encode them a SECOND time, washing every color out (e.g. DEPMS
        // 152,197,242 -> ~205,227,249). A UNORM surface stores the bytes
        // verbatim, matching OpenCPN's direct-blit behaviour exactly.
        let surface_format = surface_caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(surface_caps.formats[0]);

        println!("DEBUG: Surface format: {:?}", surface_format);
        println!("DEBUG: Available formats: {:?}", surface_caps.formats);

        let config = wgpu::SurfaceConfiguration {
            // COPY_SRC lets NAVCORE_SHOT read the rendered frame back for headless captures.
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            format: surface_format,
            width: size.width,
            height: size.height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        // Shader
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Chart Shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../../assets/shaders/chart.wgsl").into()),
        });

        // Uniforms
        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Uniform Buffer"),
            contents: bytemuck::cast_slice(&[Uniforms {
                view_proj: glam::Mat4::IDENTITY.to_cols_array_2d(),
                view_size: [size.width as f32, size.height as f32],
                pixels_per_meter: 1.0,
                _pad: 0.0,
            }]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        // Default palette buffer (64 colors, initialized to black — populated on chart load)
        let default_palette: Vec<[f32; 4]> = vec![[0.0, 0.0, 0.0, 1.0]; 64];
        let palette_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Palette Buffer"),
            contents: bytemuck::cast_slice(&default_palette),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        });

        let uniform_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                entries: &[
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
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Storage { read_only: true },
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
                label: Some("uniform_bind_group_layout"),
            });

        let uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &uniform_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: palette_buffer.as_entire_binding(),
                },
            ],
            label: Some("uniform_bind_group"),
        });

        // Depth buffer
        let depth_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Depth Texture"),
            size: wgpu::Extent3d {
                width: size.width.max(1),
                height: size.height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: super::msaa_samples(),
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth24PlusStencil8,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let depth_view = depth_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let msaa_view = create_msaa_view(&device, config.format, size.width, size.height);

        // Pipeline
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Pipeline Layout"),
            bind_group_layouts: &[&uniform_bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Chart Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[Vertex::desc()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None, // No culling for 2D
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

        // Default camera (will be updated when chart is loaded)
        let camera = Camera::new(0.0, 0.0, 1000.0, size.width as f32, size.height as f32);

        // Line rendering pipeline (shader-based polylines with screen-space width)
        let line_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Line Shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../../assets/shaders/line.wgsl").into()),
        });

        let line_bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("line_bind_group_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: std::num::NonZeroU64::new(std::mem::size_of::<LineUniforms>() as u64),
                },
                count: None,
            }],
        });

        let line_uniform_align = device.limits().min_uniform_buffer_offset_alignment;
        let slot_size = (line_uniform_align as usize).max(std::mem::size_of::<LineUniforms>());
        let max_line_styles = 128;
        let line_uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Line Uniform Buffer"),
            size: (slot_size * max_line_styles) as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let line_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("line_bind_group"),
            layout: &line_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &line_uniform_buffer,
                    offset: 0,
                    size: std::num::NonZeroU64::new(std::mem::size_of::<LineUniforms>() as u64),
                }),
            }],
        });

        let line_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Line Pipeline Layout"),
            bind_group_layouts: &[&uniform_bind_group_layout, &line_bind_group_layout],
            push_constant_ranges: &[],
        });

        let line_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Line Pipeline"),
            layout: Some(&line_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &line_shader,
                entry_point: "vs_main",
                buffers: &[LineVertex::desc()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &line_shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                // Enable primitive restart with u32::MAX as restart marker
                strip_index_format: Some(wgpu::IndexFormat::Uint32),
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth24PlusStencil8,
                // Lines carry no usable depth: the per-style uniform writes
                // disp_prio 0, so line.wgsl puts every line at z = 1.0 (far
                // plane). Depth-testing them therefore erased any line drawn
                // over an area of priority >= 1 — invisible until navcore's
                // priority ladder was one step low and Group 1 also landed on
                // z = 1.0. Draw order is already sequenced per priority on the
                // CPU (areas, then lines, then symbols), so the line passes need
                // no depth test of their own.
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

        // Background chart pipelines. Identical to the foreground ones — the
        // separation is draw *order*, nothing else: a tile's coarser charts are
        // drawn before its finer ones so the finer ones win by overdraw.
        //
        // These used to carry a stencil test that suppressed background
        // geometry wherever a finer chart declared coverage. It cannot be made
        // right with one stencil buffer per tile: the mask a chart needs is
        // "every chart finer than me", which differs per chart, and one tile-wide
        // union masked a mid-scale chart with its own coverage. Worse, coverage
        // is a claim about where a cell has *data*, not about where it *paints*,
        // so even a per-chart mask removes the chart underneath in every gap the
        // finer cell leaves. That was the pale rectangles across Zealand.
        // Painter's order alone quilts correctly and cannot punch a hole.
        let bg_stencil_state = wgpu::StencilState::default();

        let bg_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("BG Chart Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[Vertex::desc()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
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
                stencil: bg_stencil_state.clone(),
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

        // Background line pipeline: identical to line_pipeline but with stencil test
        let bg_line_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("BG Line Pipeline"),
            layout: Some(&line_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &line_shader,
                entry_point: "vs_main",
                buffers: &[LineVertex::desc()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &line_shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                strip_index_format: Some(wgpu::IndexFormat::Uint32),
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth24PlusStencil8,
                // Lines carry no usable depth: the per-style uniform writes
                // disp_prio 0, so line.wgsl puts every line at z = 1.0 (far
                // plane). Depth-testing them therefore erased any line drawn
                // over an area of priority >= 1 — invisible until navcore's
                // priority ladder was one step low and Group 1 also landed on
                // z = 1.0. Draw order is already sequenced per priority on the
                // CPU (areas, then lines, then symbols), so the line passes need
                // no depth test of their own.
                depth_write_enabled: false,
                depth_compare: wgpu::CompareFunction::Always,
                stencil: bg_stencil_state,
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

        // Symbol renderer (optional - may fail if assets not found)
        let (symbol_renderer, symbol_camera_bind_group) = match SymbolRenderer::new(
            &device,
            &queue,
            surface_format,
            &uniform_bind_group_layout,
        ) {
            Ok(renderer) => {
                let bind_group = renderer.create_camera_bind_group(&device, &uniform_buffer);
                println!("DEBUG: Symbol renderer initialized");
                (Some(renderer), Some(bind_group))
            }
            Err(e) => {
                eprintln!("WARNING: Failed to initialize symbol renderer: {}", e);
                (None, None)
            }
        };

        // Pattern renderer (optional - may fail if assets not found)
        let (pattern_renderer, pattern_camera_bind_group) = match PatternRenderer::new(
            &device,
            &queue,
            surface_format,
            &uniform_bind_group_layout,
        ) {
            Ok(renderer) => {
                let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("pattern_camera_bind_group"),
                    layout: renderer.camera_bind_group_layout(),
                    entries: &[wgpu::BindGroupEntry {
                        binding: 0,
                        resource: uniform_buffer.as_entire_binding(),
                    }],
                });
                println!("DEBUG: Pattern renderer initialized");
                (Some(renderer), Some(bind_group))
            }
            Err(e) => {
                eprintln!("WARNING: Failed to initialize pattern renderer: {}", e);
                (None, None)
            }
        };

        // Text/sounding renderer
        let (text_renderer, text_camera_bind_group) = match TextRenderer::new(
            &device,
            surface_format,
            &uniform_bind_group_layout,
        ) {
            Ok(renderer) => {
                let bind_group =
                    renderer.create_camera_bind_group(&device, &uniform_buffer, &palette_buffer);
                println!("DEBUG: Text renderer initialized");
                (Some(renderer), Some(bind_group))
            }
            Err(e) => {
                eprintln!("WARNING: Failed to initialize text renderer: {}", e);
                (None, None)
            }
        };

        let (label_renderer, label_camera_bind_group) = match LabelRenderer::new(
            &device,
            &queue,
            surface_format,
            &uniform_bind_group_layout,
        ) {
            Ok(renderer) => {
                let bind_group =
                    renderer.create_camera_bind_group(&device, &uniform_buffer, &palette_buffer);
                println!("DEBUG: Label renderer initialized");
                (Some(renderer), Some(bind_group))
            }
            Err(e) => {
                eprintln!("WARNING: Failed to initialize label renderer: {}", e);
                (None, None)
            }
        };

        Self {
            // NAVCORE_DUMP_DRAW=<path> records what the draw pass issued.
            draw_log: std::env::var("NAVCORE_DUMP_DRAW")
                .ok()
                .filter(|v| !v.is_empty())
                .map(|_| std::cell::RefCell::new(Vec::new())),
            window,
            surface,
            device,
            queue,
            config,
            size,
            scale_factor,
            effective_ppmm,
            pipeline,
            uniform_buffer,
            uniform_bind_group,
            palette_buffer,
            uniform_bind_group_layout,
            depth_texture,
            msaa_view,
            depth_view,
            vertex_buffer: None,
            vertex_count: 0,
            camera,
            symbol_renderer,
            symbol_camera_bind_group,
            pattern_renderer,
            pattern_camera_bind_group,
            text_renderer,
            text_camera_bind_group,
            label_renderer,
            ui: None,
            shop: None,
            signalk: None,
            fleet: Default::default(),
            following: false,
            mariner_buffer: None,
            mariner_count: 0,
            mariner_symbols: crate::render::mariner::MarinerSymbols::resolve(),
            chart_root: None,
            pick_anchor: None,
            pick_objects: Vec::new(),
            pick_pending: None,

            label_camera_bind_group,
            global_text_buffer: None,
            global_text_count: 0,
            global_label_buffer: None,
            global_label_count: 0,
            global_sounding_buffer: None,
            global_sounding_count: 0,
            bg_pipeline,
            line_pipeline,
            bg_line_pipeline,
            line_bind_group_layout,
            line_uniform_buffer,
            line_bind_group,
            line_uniform_align,
            line_batches: Vec::new(),
            // Tile mode (disabled by default - single chart mode)
            catalog: None,
            tile_cache: TileGpuCache::default_auto(),
            keys: None,
            decryptor: None,
            tile_worker: None,
            pending_tiles: std::collections::HashSet::new(),
            deferred_tile_results: Vec::new(),
            chart_cache: HashMap::new(),
            style_hash: 0,
            tile_mode: false,
            visible_tiles_cache: Vec::new(),
            current_z: 0,
            previous_visible_tiles: Vec::new(),
            tile_draw_list: Vec::new(),
            previous_style_hash: 0,
            s52_engine: None,
            needs_redraw: true,
            last_text_layout_key: 0,
            cached_visible_line_styles: Vec::new(),
            cached_visible_line_styles_key: 0,
            last_line_uniforms_key: 0,
            pending_capture: None,
        }
    }

    /// Load chart data and create vertex buffers
    pub fn load_chart(&mut self, chart: &ChartData) {
        let mut vertices = Vec::new();

        // Debug: show ALL area feature type codes to identify diagonal lines
        use std::collections::HashMap;
        let mut area_type_counts: HashMap<u16, usize> = HashMap::new();
        for feature in chart.areas() {
            *area_type_counts.entry(feature.type_code).or_insert(0) += 1;
        }
        println!("DEBUG: Area feature type breakdown:");
        let mut sorted_types: Vec<_> = area_type_counts.iter().collect();
        sorted_types.sort_by_key(|(code, _)| *code);
        for (type_code, count) in sorted_types {
            println!("  type_code={}: {} features", type_code, count);
        }

        // Process area features - only land and depth areas
        // Skip meta features (M_COVR, M_QUAL, etc.) and unknown types
        // SM coordinates are already relative to chart center - use directly
        let mut rendered_area_types: HashMap<u16, usize> = HashMap::new();
        let mut total_plain_triangles = 0usize;
        let mut total_strip_triangles = 0usize;
        let mut total_fan_triangles = 0usize;
        let mut total_skinny_triangles = 0usize;
        let mut total_degenerate_triangles = 0usize;

        for feature in chart.areas() {
            // Skip meta features (type codes 300+) - these define metadata, not chart content
            if feature.type_code >= 300 {
                continue;
            }

            // Only render features we have colors for (land, depth areas, built-up areas, lakes)
            // Unknown features would get transparent color and waste vertices
            let is_known_area = feature.is_land()
                || feature.is_depth_area()
                || feature.type_code == BUAARE
                || feature.type_code == LAKARE;
            if !is_known_area {
                continue;
            }

            // Legacy path: use color index 0 as fallback (LANDA is typical first token)
            let color_index: u32 = 0;

            if let Some(ref geom) = feature.area_geometry {
                *rendered_area_types.entry(feature.type_code).or_insert(0) += 1;

                // Track primitive statistics
                let (plain, strip, fan) = geom.primitive_stats();
                total_plain_triangles += plain;
                total_strip_triangles += strip;
                total_fan_triangles += fan;

                // Analyze triangle quality for this feature
                let (_total, skinny, degenerate) = geom.analyze_triangle_quality();
                total_skinny_triangles += skinny;
                total_degenerate_triangles += degenerate;

                // SM coordinates used directly - no conversion needed
                let tri_verts = geom.to_vertices();

                // Just push all vertices - large triangles are valid for chart-spanning features
                for pos in tri_verts {
                    vertices.push(Vertex {
                        position: pos,
                        color_index,
                        disp_prio: 0,
                        shade: 0.0,
                    });
                }
            }
        }
        println!("DEBUG: Actually rendered area types: {:?}", rendered_area_types);
        println!("DEBUG: Primitive stats - plain: {}, strip: {}, fan: {} triangles",
            total_plain_triangles, total_strip_triangles, total_fan_triangles);
        if total_skinny_triangles > 0 || total_degenerate_triangles > 0 {
            println!("DEBUG: Triangle quality - skinny: {}, degenerate: {}",
                total_skinny_triangles, total_degenerate_triangles);
        }

        // Debug: Check which recognized features have geometry
        let land_with_geom = chart.land_areas().filter(|f| f.area_geometry.is_some()).count();
        let depth_with_geom = chart.depth_areas().filter(|f| f.area_geometry.is_some()).count();
        println!("DEBUG: Land areas with geometry: {}, Depth areas with geometry: {}", land_with_geom, depth_with_geom);

        let area_vertex_count = vertices.len();

        // Debug: show ALL line feature type codes
        let mut line_type_counts: HashMap<u16, usize> = HashMap::new();
        for feature in chart.lines() {
            *line_type_counts.entry(feature.type_code).or_insert(0) += 1;
        }
        println!("DEBUG: Line feature type breakdown:");
        let mut sorted_line_types: Vec<_> = line_type_counts.iter().collect();
        sorted_line_types.sort_by_key(|(code, _)| *code);
        for (type_code, count) in sorted_line_types {
            println!("  type_code={}: {} features", type_code, count);
        }

        // Process line features using new shader-based polyline rendering
        // Collect all polylines for coastlines and contours
        let mut coastline_polylines: Vec<Vec<[f32; 2]>> = Vec::new();
        for feature in chart.coastlines() {
            if let Some(ref geom) = feature.line_geometry {
                let segments = geom.resolve(&chart.edge_table);
                for segment in segments {
                    if segment.len() >= 2 {
                        coastline_polylines.push(segment);
                    }
                }
            }
        }

        let mut contour_polylines: Vec<Vec<[f32; 2]>> = Vec::new();
        for feature in chart.depth_contours() {
            if let Some(ref geom) = feature.line_geometry {
                let segments = geom.resolve(&chart.edge_table);
                for segment in segments {
                    if segment.len() >= 2 {
                        contour_polylines.push(segment);
                    }
                }
            }
        }

        println!("DEBUG: Lines: coastlines={} polylines, contours={} polylines",
            coastline_polylines.len(), contour_polylines.len());

        // Calculate bounds FIRST (before polylines are consumed)
        let mut min_x = f32::MAX;
        let mut max_x = f32::MIN;
        let mut min_y = f32::MAX;
        let mut max_y = f32::MIN;

        // Include area vertex bounds
        for v in &vertices {
            min_x = min_x.min(v.position[0]);
            max_x = max_x.max(v.position[0]);
            min_y = min_y.min(v.position[1]);
            max_y = max_y.max(v.position[1]);
        }

        // Include line bounds from polylines
        for polyline in coastline_polylines.iter().chain(contour_polylines.iter()) {
            for point in polyline {
                min_x = min_x.min(point[0]);
                max_x = max_x.max(point[0]);
                min_y = min_y.min(point[1]);
                max_y = max_y.max(point[1]);
            }
        }

        // Clear existing line batches
        self.line_batches.clear();

        // Build coastline batch (solid, 2px, dark brown)
        if !coastline_polylines.is_empty() {
            let (coastline_verts, coastline_indices) = build_line_vertices_multi_indexed(&coastline_polylines);
            if !coastline_indices.is_empty() {
                self.line_batches.push(LineBatch {
                    style: COASTLINE_STYLE,
                    vertex_buffer: self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("Coastline Vertex Buffer"),
                        contents: bytemuck::cast_slice(&coastline_verts),
                        usage: wgpu::BufferUsages::VERTEX,
                    }),
                    index_buffer: self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("Coastline Index Buffer"),
                        contents: bytemuck::cast_slice(&coastline_indices),
                        usage: wgpu::BufferUsages::INDEX,
                    }),
                    index_count: coastline_indices.len() as u32,
                    vertex_count: coastline_verts.len() as u32,
                });
                println!("DEBUG: Coastline batch: {} vertices, {} indices", coastline_verts.len(), coastline_indices.len());
            }
        }

        // Build contour batch (dashed, 1px, gray-blue)
        if !contour_polylines.is_empty() {
            let (contour_verts, contour_indices) = build_line_vertices_multi_indexed(&contour_polylines);
            if !contour_indices.is_empty() {
                self.line_batches.push(LineBatch {
                    style: CONTOUR_STYLE,
                    vertex_buffer: self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("Contour Vertex Buffer"),
                        contents: bytemuck::cast_slice(&contour_verts),
                        usage: wgpu::BufferUsages::VERTEX,
                    }),
                    index_buffer: self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("Contour Index Buffer"),
                        contents: bytemuck::cast_slice(&contour_indices),
                        usage: wgpu::BufferUsages::INDEX,
                    }),
                    index_count: contour_indices.len() as u32,
                    vertex_count: contour_verts.len() as u32,
                });
                println!("DEBUG: Contour batch: {} vertices, {} indices", contour_verts.len(), contour_indices.len());
            }
        }

        // Total line index count for logging
        let total_line_indices: u32 = self.line_batches.iter().map(|b| b.index_count).sum();
        println!("DEBUG: Loaded {} area vertices, {} line indices in {} batches",
            area_vertex_count, total_line_indices, self.line_batches.len());

        println!("Loaded {} total area vertices from chart", vertices.len());

        // Create vertex buffer for areas (if any)
        if !vertices.is_empty() {
            self.vertex_buffer = Some(self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("Chart Vertex Buffer"),
                contents: bytemuck::cast_slice(&vertices),
                usage: wgpu::BufferUsages::VERTEX,
            }));
            self.vertex_count = vertices.len() as u32;
        }

        // Check if we have any geometry at all
        if min_x == f32::MAX {
            println!("DEBUG: No geometry to render");
            return;
        }

        println!("DEBUG: Combined bounds: X=[{:.0}, {:.0}], Y=[{:.0}, {:.0}]", min_x, max_x, min_y, max_y);
        if !vertices.is_empty() {
            println!("DEBUG: First 3 area vertices: {:?}", &vertices[..3.min(vertices.len())]);
        }

        // Update camera to fit all geometry (areas + lines)
        // SM coordinates are centered at (0,0) - camera at origin
        // Zoom = meters per pixel (larger = more zoomed out)
        let width = max_x - min_x;
        let height = max_y - min_y;
        let screen_w = self.size.width as f32;
        let screen_h = self.size.height as f32;
        let zoom = if width > 0.0 && height > 0.0 {
            // Fit chart in view with 10% padding
            let zoom_x = (width * 1.1) / screen_w;   // meters per pixel
            let zoom_y = (height * 1.1) / screen_h;  // meters per pixel
            zoom_x.max(zoom_y)  // Use larger to fit both dimensions
        } else {
            1.0
        };

        // Center camera on actual geometry bounds (not assumed 0,0)
        let center_x = (min_x + max_x) / 2.0;
        let center_y = (min_y + max_y) / 2.0;
        self.camera = Camera::new(center_x, center_y, zoom, screen_w, screen_h);
        println!("DEBUG: Camera at ({:.0}, {:.0}), zoom: {:.2} m/px (SM bounds: {:.0}x{:.0})", center_x, center_y, zoom, width, height);

        // Log view_proj matrix to verify it's sensible
        let vp = self.camera.view_projection_matrix();
        println!("DEBUG: View-proj matrix:\n  {:?}\n  {:?}\n  {:?}\n  {:?}",
            vp.row(0), vp.row(1), vp.row(2), vp.row(3));

        // Update symbol instances
        if let Some(ref mut symbol_renderer) = self.symbol_renderer {
            let ref_lat = chart.header.ref_lat;
            let ref_lon = chart.header.ref_lon;
            let instances = features_to_instances(&chart.features, ref_lat, ref_lon);
            println!("DEBUG: Created {} symbol instances", instances.len());
            symbol_renderer.update_instances(&self.queue, &instances);
        }

        // Update sounding instances (SOUNDG with multipoint geometry)
        // Use SNDFRM02 for safety depth coloring
        if let Some(ref mut text_renderer) = self.text_renderer {
            let settings = self.s52_engine.as_ref()
                .map(|e| &e.settings)
                .cloned()
                .unwrap_or_default();
            let soundings = soundings_to_instances(&chart.features, &settings);
            text_renderer.update_soundings(&self.queue, &soundings);
        }
    }

    /// Load a test triangle to verify the rendering pipeline works.
    /// Uses NDC coordinates (-1 to 1) with identity-like matrix.
    pub fn load_test_triangle(&mut self) {
        println!("DEBUG: Loading test triangle to verify pipeline...");

        // Simple triangle in NDC space
        let vertices = vec![
            Vertex { position: [0.0, 0.5], color_index: 0, disp_prio: 0, shade: 0.0 },   // Top
            Vertex { position: [-0.5, -0.5], color_index: 0, disp_prio: 0, shade: 0.0 }, // Bottom-left
            Vertex { position: [0.5, -0.5], color_index: 0, disp_prio: 0, shade: 0.0 },  // Bottom-right
        ];

        // Create vertex buffer
        self.vertex_buffer = Some(self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Test Triangle Buffer"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        }));
        self.vertex_count = vertices.len() as u32;

        // Set camera to identity-like: maps -1..1 to -1..1
        // For ortho_rh(-half_w, half_w, -half_h, half_h) to map to NDC:
        // half_w = 1, half_h = 1
        // So: viewport_width * zoom / 2.0 = 1 → zoom = 2.0 / viewport_width
        let zoom = 2.0 / self.size.width as f32;
        self.camera = Camera::new(0.0, 0.0, zoom, self.size.width as f32, self.size.height as f32);

        let vp = self.camera.view_projection_matrix();
        println!("DEBUG: Test triangle view_proj matrix:");
        println!("  {:?}", vp.row(0));
        println!("  {:?}", vp.row(1));
        println!("  {:?}", vp.row(2));
        println!("  {:?}", vp.row(3));
    }

    /// Upload the active palette colors to the GPU storage buffer.
    /// Called when S52 engine loads or palette changes.
    fn upload_palette(&mut self, tables: &crate::s52::LookupTables) {
        let palette_data = tables.active_palette_f32();
        if palette_data.is_empty() {
            return;
        }

        // Recreate palette buffer with correct size
        self.palette_buffer = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Palette Buffer"),
            contents: bytemuck::cast_slice(&palette_data),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        });

        // Recreate bind group with new palette buffer
        self.uniform_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &self.uniform_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.uniform_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.palette_buffer.as_entire_binding(),
                },
            ],
            label: Some("uniform_bind_group"),
        });

        if let Some(ref text_renderer) = self.text_renderer {
            self.text_camera_bind_group = Some(text_renderer.create_camera_bind_group(
                &self.device,
                &self.uniform_buffer,
                &self.palette_buffer,
            ));
        }
        if let Some(ref label_renderer) = self.label_renderer {
            self.label_camera_bind_group = Some(label_renderer.create_camera_bind_group(
                &self.device,
                &self.uniform_buffer,
                &self.palette_buffer,
            ));
        }

        log::info!("Uploaded palette with {} colors to GPU", palette_data.len());
    }

    /// Switch the active palette (Day/Dusk/Night) — zero tile rebuilds.
    pub fn switch_palette(&mut self, palette_name: &str) {
        if let Some(ref mut ui) = self.ui {
            ui.set_dark(matches!(palette_name, "DUSK" | "NIGHT"));
        }
        if let Some(ref mut engine) = self.s52_engine {
            engine.tables.set_active_palette(palette_name);
            let palette_data = engine.tables.active_palette_f32();
            if !palette_data.is_empty() {
                self.queue.write_buffer(
                    &self.palette_buffer,
                    0,
                    bytemuck::cast_slice(&palette_data),
                );
                if let Some(ref mut symbol_renderer) = self.symbol_renderer {
                    if let Err(err) = symbol_renderer.switch_palette(&self.queue, palette_name) {
                        log::warn!(
                            "Palette '{}' switched, but symbol atlas switch failed: {}",
                            palette_name,
                            err
                        );
                    }
                }
                log::info!("Switched palette to '{}' ({} colors)", palette_name, palette_data.len());
            }
        }
    }

    /// Load chart catalog for tile-based multi-chart rendering.
    /// Takes ownership of decryptor (not Clone) and wraps catalog in Arc.
    /// Remember where chart sets live, so a download lands beside them.
    pub fn set_chart_root(&mut self, root: std::path::PathBuf) {
        self.chart_root = Some(root);
    }

    pub fn load_catalog(
        &mut self,
        catalog: ChartCatalog,
        keys: Arc<KeyStore>,
        decryptor: CachedDecryptor,
    ) {
        // Camera position must be in GLOBAL MERCATOR meters
        // catalog.combined_extent is already in global Mercator (computed during catalog build)
        let extent = &catalog.combined_extent;
        let center_x = ((extent.min_x + extent.max_x) / 2.0) as f32;
        let center_y = ((extent.min_y + extent.max_y) / 2.0) as f32;
        let width = (extent.max_x - extent.min_x) as f32;
        let height = (extent.max_y - extent.min_y) as f32;

        let zoom_x = (width * 1.1) / self.size.width as f32;
        let zoom_y = (height * 1.1) / self.size.height as f32;
        let zoom = zoom_x.max(zoom_y);

        // Camera now in global Mercator meters (not chart-local SM)
        self.camera = Camera::new(
            center_x, center_y, zoom,
            self.size.width as f32, self.size.height as f32,
        );

        // Optional override for debugging/screenshots: NAVCORE_VIEW="lat,lon,mpp"
        // positions the camera at a specific WGS84 point with a given zoom
        // (meters per pixel). Lets us reproduce a reference view exactly.
        if let Ok(view) = std::env::var("NAVCORE_VIEW") {
            let parts: Vec<f32> = view
                .split(',')
                .filter_map(|s| s.trim().parse::<f32>().ok())
                .collect();
            if parts.len() == 3 {
                let (mx, my) = crate::tiles::latlon_to_mercator(parts[0] as f64, parts[1] as f64);
                self.camera = Camera::new(
                    mx as f32, my as f32, parts[2],
                    self.size.width as f32, self.size.height as f32,
                );
                println!(
                    "NAVCORE_VIEW override: lat={} lon={} mpp={} -> Mercator ({:.0},{:.0})",
                    parts[0], parts[1], parts[2], mx, my
                );
            } else {
                eprintln!("NAVCORE_VIEW must be 'lat,lon,mpp'; ignoring '{}'", view);
            }
        }

        // NAVCORE_TILT=<degrees> pitches the camera back. Off by default: a
        // chart is a plan, and every S-52 measurement — bearing, distance,
        // symbol size in millimetres — is stated on that plan.
        if let Ok(t) = std::env::var("NAVCORE_TILT") {
            match t.trim().parse::<f32>() {
                Ok(deg) => {
                    self.camera.tilt =
                        deg.to_radians().clamp(0.0, crate::render::camera::MAX_TILT);
                    println!("NAVCORE_TILT: {:.1}°", self.camera.tilt.to_degrees());
                }
                Err(_) => eprintln!("NAVCORE_TILT must be a number of degrees; ignoring '{}'", t),
            }
        }

        println!("Catalog: {} charts, camera at ({:.0}, {:.0}) Mercator, zoom {:.0} m/px",
            catalog.charts.len(), center_x, center_y, zoom);

        let catalog_arc = Arc::new(catalog);
        let keys_arc = keys;

        self.tile_mode = true;
        self.tile_cache = TileGpuCache::default_auto();
        self.style_hash = 0;
        self.chart_cache = HashMap::new();

        // Load S-52 presentation engine for display category filtering
        // Load once for main thread (palette ops), once for worker (tile building)
        // Mariner settings come from the environment so a capture and the
        // conformance dumps of the same view resolve features identically.
        // Both engines must get them: the worker's engine is the one that
        // actually decides what goes into a tile.
        let env_settings = crate::s52::MarinerSettings::from_env();
        let mut worker_engine: Option<S52Engine> = None;
        match S52Engine::load("assets/s52/chartsymbols.xml") {
            Ok(mut engine) => {
                engine.set_settings(env_settings.clone());
                println!("{}", engine.summary());
                // Upload active palette to GPU
                self.upload_palette(&engine.tables);
                self.s52_engine = Some(engine);
                // Load a second engine for the worker thread
                if let Ok(mut engine2) = S52Engine::load("assets/s52/chartsymbols.xml") {
                    engine2.set_settings(env_settings);
                    worker_engine = Some(engine2);
                }
            }
            Err(e) => {
                eprintln!("Warning: Could not load S-52 engine: {}", e);
                eprintln!("Display category filtering will be disabled.");
            }
        }

        // NAVCORE_PALETTE=day|dusk|night. `switch_palette` existed and nothing
        // ever called it, so Dusk and Night were unreachable — which is how
        // atlas-dusk.png and atlas-dark.png came to be months out of date with
        // the layout atlas.json describes without anyone noticing.
        if let Ok(p) = std::env::var("NAVCORE_PALETTE") {
            let name = match p.to_ascii_lowercase().as_str() {
                "day" | "day_bright" => Some("DAY_BRIGHT"),
                "dusk" => Some("DUSK"),
                "night" | "dark" => Some("NIGHT"),
                other => {
                    eprintln!("NAVCORE_PALETTE: unknown value {:?}", other);
                    None
                }
            };
            if let Some(name) = name {
                self.switch_palette(name);
            }
        }


        // Spawn background tile worker thread
        // Worker takes ownership of decryptor and chart_cache
        let worker = crate::tiles::worker::spawn_tile_worker(
            Arc::clone(&catalog_arc),
            Arc::clone(&keys_arc),
            decryptor,
            worker_engine,
        );
        self.tile_worker = Some(worker);
        self.pending_tiles.clear();
        self.deferred_tile_results.clear();
        println!("Background tile worker spawned");

        // NAVCORE_PICK=lat,lon opens the info bubble at a position, so a
        // headless capture can show it.
        if let Ok(spec) = std::env::var("NAVCORE_PICK") {
            let parts: Vec<f64> = spec.split(',').filter_map(|v| v.trim().parse().ok()).collect();
            if parts.len() == 2 {
                let (mx, my) = crate::tiles::latlon_to_mercator(parts[0], parts[1]);
                self.pick_at_world(mx as f32, my as f32);
            } else {
                eprintln!("NAVCORE_PICK must be 'lat,lon'; ignoring {:?}", spec);
            }
        }

        self.catalog = Some(catalog_arc);
        self.keys = Some(keys_arc);
        // decryptor is now owned by worker - clear main thread reference
        self.decryptor = None;
    }

    pub fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        if new_size.width > 0 && new_size.height > 0 {
            self.size = new_size;
            self.config.width = new_size.width;
            self.config.height = new_size.height;
            self.surface.configure(&self.device, &self.config);

            // Recreate depth texture
            self.depth_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("Depth Texture"),
                size: wgpu::Extent3d {
                    width: self.config.width.max(1),
                    height: self.config.height.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: super::msaa_samples(),
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Depth24PlusStencil8,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            self.depth_view = self.depth_texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.msaa_view = create_msaa_view(
                &self.device, self.config.format, self.config.width, self.config.height,
            );

            self.camera.resize(new_size.width as f32, new_size.height as f32);
            self.needs_redraw = true;
        }
    }

    /// Update scale factor for HiDPI display changes
    pub fn set_scale_factor(&mut self, scale_factor: f32) {
        self.scale_factor = scale_factor;
        self.effective_ppmm = 4.0 * scale_factor; // 96 DPI base = 4.0 ppmm
    }

    fn current_text_layout_key(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.style_hash.hash(&mut hasher);
        self.tile_cache.revision().hash(&mut hasher);
        self.visible_tiles_cache.hash(&mut hasher);
        self.size.width.hash(&mut hasher);
        self.size.height.hash(&mut hasher);
        self.camera.position.x.to_bits().hash(&mut hasher);
        self.camera.position.y.to_bits().hash(&mut hasher);
        self.camera.zoom.to_bits().hash(&mut hasher);
        hasher.finish()
    }

    fn current_visible_line_styles_key(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.style_hash.hash(&mut hasher);
        self.tile_cache.revision().hash(&mut hasher);
        self.visible_tiles_cache.hash(&mut hasher);
        hasher.finish()
    }

    fn current_line_uniforms_key(&self, styles_key: u64) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        styles_key.hash(&mut hasher);
        self.size.width.hash(&mut hasher);
        self.size.height.hash(&mut hasher);
        self.effective_ppmm.to_bits().hash(&mut hasher);
        self.camera.position.x.to_bits().hash(&mut hasher);
        self.camera.position.y.to_bits().hash(&mut hasher);
        self.camera.zoom.to_bits().hash(&mut hasher);
        hasher.finish()
    }


    /// The pixels-per-mm figure S-52 line styling is evaluated at.
    ///
    /// The *physical* density, because S-52 specifies line widths and dash
    /// periods in millimetres on the display — that is the whole point of
    /// giving them in millimetres. OpenCPN evaluates them against a
    /// `canvas_pix_per_mm` that stays at the logical 96-dpi figure on a Retina
    /// canvas, which halves every stroke and dash; matching that would make
    /// navcore's output depend on the display density the same way.
    fn line_style_ppmm(&self) -> f32 {
        self.effective_ppmm.max(1.0)
    }

    fn collect_visible_line_styles(&self) -> Vec<LineStyleKey> {
        let mut styles = Vec::new();
        for tile_id in &self.visible_tiles_cache {
            let key = TileCacheKey::new(*tile_id, self.style_hash);
            if let Some(buffers) = self.tile_cache.get(&key) {
                for batch in &buffers.line_batches {
                    if batch.index_count > 0 {
                        styles.push(batch.key.style.clone());
                    }
                }
            }
        }
        styles.sort();
        styles.dedup();
        styles
    }

    pub fn render(&mut self) -> Result<(), wgpu::SurfaceError> {
        let render_start = profile_enabled().then(Instant::now);

        // Before anything is drawn: what the boats have said this frame moves
        // the camera (follow) and the mariner symbols, and both must be
        // settled before the camera is clamped and the chart is drawn.
        self.poll_shop();
        self.poll_signalk();
        self.update_mariner_symbols();

        // In tile mode, keep the camera near the loaded chart extent.
        // This prevents panning/gesture glitches from throwing the view outside the
        // valid Mercator range, which would yield empty tiles (ocean-only).
        if self.tile_mode {
            self.clamp_camera_to_catalog();
        }

        // Update uniforms
        let uniforms = Uniforms {
            view_proj: self.camera.view_projection_matrix().to_cols_array_2d(),
            view_size: [self.size.width as f32, self.size.height as f32],
            pixels_per_meter: 1.0 / self.camera.zoom, // Convert zoom (m/px) to px/m
            _pad: 0.0,
        };
        self.queue.write_buffer(&self.uniform_buffer, 0, bytemuck::cast_slice(&[uniforms]));

        // CRITICAL: Build tiles BEFORE starting render pass (avoid stalls during draw)
        if self.tile_mode {
            let build_start = profile_enabled().then(Instant::now);
            self.build_visible_tiles();
            if let Some(start) = build_start {
                log::info!("profile.build_visible_tiles: {} ms", start.elapsed().as_millis());
            }

            let text_layout_key = self.current_text_layout_key();
            if self.needs_redraw && self.last_text_layout_key != text_layout_key {
                let text_start = profile_enabled().then(Instant::now);
                self.build_global_text_buffers();
                self.last_text_layout_key = text_layout_key;
                if let Some(start) = text_start {
                    log::info!(
                        "profile.build_global_text_buffers: {} ms",
                        start.elapsed().as_millis()
                    );
                }
            }
        }

        // Pre-write line uniform styles to dynamic UBO BEFORE the render pass.
        // Each unique style gets its own aligned slot; draw calls reference by dynamic offset.
        let line_style_offsets: HashMap<LineStyleKey, u32> = if self.tile_mode {
            let px_per_meter = 1.0 / self.camera.zoom;
            let view_proj = self.camera.view_projection_matrix().to_cols_array_2d();
            let viewport_size = [self.size.width as f32, self.size.height as f32];
            let align = self.line_uniform_align;
            let styles_key = self.current_visible_line_styles_key();
            if self.cached_visible_line_styles_key != styles_key {
                self.cached_visible_line_styles = self.collect_visible_line_styles();
                self.cached_visible_line_styles_key = styles_key;
            }

            let mut offsets: HashMap<LineStyleKey, u32> = HashMap::new();
            for (slot, style_key) in self.cached_visible_line_styles.iter().enumerate() {
                offsets.insert(style_key.clone(), (slot as u32) * align);
            }

            let line_uniforms_key = self.current_line_uniforms_key(styles_key);
            if self.last_line_uniforms_key != line_uniforms_key {
                let uniform_start = profile_enabled().then(Instant::now);
                let tables = self.s52_engine.as_ref().map(|e| &e.tables);
                for (slot, style_key) in self.cached_visible_line_styles.iter().enumerate() {
                    let offset = (slot as u32) * align;
                    let style = style_for_key(style_key, self.line_style_ppmm(), tables);
                    let line_uniforms = LineUniforms {
                        view_proj,
                        viewport_size,
                        line_width_px: style.width_px,
                        join_limit: 4.0,
                        color_index: style.color_index,
                        dash_on_px: style.dash_on_px,
                        dash_off_px: style.dash_off_px,
                        px_per_meter,
                        disp_prio: 0.0,
                        dot_on_px: style.dot_on_px,
                        _pad: [0, 0],
                    };
                    self.queue.write_buffer(
                        &self.line_uniform_buffer,
                        offset as u64,
                        bytemuck::cast_slice(&[line_uniforms]),
                    );
                }
                self.last_line_uniforms_key = line_uniforms_key;
                if let Some(start) = uniform_start {
                    log::info!(
                        "profile.line_uniform_upload: {} ms styles={}",
                        start.elapsed().as_millis(),
                        self.cached_visible_line_styles.len()
                    );
                }
            }
            offsets
        } else {
            // Single-chart mode: pre-write each batch's style to its own slot
            let px_per_meter = 1.0 / self.camera.zoom;
            let view_proj = self.camera.view_projection_matrix().to_cols_array_2d();
            let viewport_size = [self.size.width as f32, self.size.height as f32];
            let align = self.line_uniform_align;

            for (i, batch) in self.line_batches.iter().enumerate() {
                let offset = (i as u32) * align;
                let line_uniforms = LineUniforms {
                    view_proj,
                    viewport_size,
                    line_width_px: batch.style.width_px,
                    join_limit: 4.0,
                    color_index: batch.style.color_index,
                    dash_on_px: batch.style.dash_on_px,
                    dash_off_px: batch.style.dash_off_px,
                    px_per_meter,
                    disp_prio: 4.0,
                    dot_on_px: batch.style.dot_on_px,
                    _pad: [0, 0],
                };
                self.queue.write_buffer(
                    &self.line_uniform_buffer,
                    offset as u64,
                    bytemuck::cast_slice(&[line_uniforms]),
                );
            }
            HashMap::new() // Not used in single-chart draw path
        };

        let output = self.surface.get_current_texture()?;
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Render Encoder"),
        });

        {
            let multisampled = super::msaa_samples() > 1;
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Render Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    // Draw multisampled, resolve into the surface. StoreOp is
                    // Discard because only the resolved image is ever read.
                    // With multisampling off there is nothing to resolve and the
                    // surface is the target.
                    view: if multisampled { &self.msaa_view } else { &view },
                    resolve_target: multisampled.then_some(&view),
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: OCEAN_BACKGROUND[0] as f64,
                            g: OCEAN_BACKGROUND[1] as f64,
                            b: OCEAN_BACKGROUND[2] as f64,
                            a: 1.0,
                        }),
                        store: if multisampled {
                            wgpu::StoreOp::Discard
                        } else {
                            wgpu::StoreOp::Store
                        },
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(0),
                        store: wgpu::StoreOp::Store,
                    }),
                }),
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            if self.tile_mode {
                // Tile-based multi-chart rendering
                let draw_start = profile_enabled().then(Instant::now);
                self.draw_tiles(&mut render_pass, &line_style_offsets);
                if let Some(start) = draw_start {
                    log::info!("profile.draw_tiles: {} ms", start.elapsed().as_millis());
                }
                self.draw_mariner_symbols(&mut render_pass);
            } else {
                // Single chart mode (existing code)
                // 1. Render areas (land, depth)
                render_pass.set_pipeline(&self.pipeline);
                render_pass.set_bind_group(0, &self.uniform_bind_group, &[]);

                if let Some(ref buffer) = self.vertex_buffer {
                    render_pass.set_vertex_buffer(0, buffer.slice(..));
                    render_pass.draw(0..self.vertex_count, 0..1);
                }

                // 2. Render lines (coastlines + depth contours) with shader-based pipeline
                render_pass.set_pipeline(&self.line_pipeline);
                render_pass.set_bind_group(0, &self.uniform_bind_group, &[]);
                let mut slot = 0u32;
                for batch in &self.line_batches {
                    let offset = slot * self.line_uniform_align;
                    render_pass.set_bind_group(1, &self.line_bind_group, &[offset]);
                    render_pass.set_vertex_buffer(0, batch.vertex_buffer.slice(..));
                    render_pass.set_index_buffer(batch.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                    render_pass.draw_indexed(0..batch.index_count, 0, 0..1);
                    slot += 1;
                }

                // 3. Render symbols on top
                if let (Some(ref symbol_renderer), Some(ref symbol_bind_group)) =
                    (&self.symbol_renderer, &self.symbol_camera_bind_group)
                {
                    if debug_render_mode() == 3 {
                        symbol_renderer.render_atlas_debug(&mut render_pass, symbol_bind_group);
                    } else {
                        symbol_renderer.render(&mut render_pass, symbol_bind_group);
                    }
                }

                // 4. Render text/soundings on top of everything
                if let (Some(ref text_renderer), Some(ref text_bind_group)) =
                    (&self.text_renderer, &self.text_camera_bind_group)
                {
                    text_renderer.render(&mut render_pass, text_bind_group);
                }

                if debug_render_mode() != 0 {
                    let line_vertex_count: u32 = self.line_batches.iter().map(|b| b.vertex_count).sum();
                    let line_index_count: u32 = self.line_batches.iter().map(|b| b.index_count).sum();
                    let symbol_instances = self.symbol_renderer.as_ref().map(|r| r.instance_count()).unwrap_or(0);
                    let symbol_draws = if debug_render_mode() == 3 {
                        if self.symbol_renderer.is_some() { 1 } else { 0 }
                    } else {
                        if symbol_instances > 0 { 1 } else { 0 }
                    };
                    log::info!(
                        "debug_render: single lines vertices={} indices={} batches={} symbols={} draws={} mode={}",
                        line_vertex_count,
                        line_index_count,
                        self.line_batches.len(),
                        symbol_instances,
                        symbol_draws,
                        debug_render_mode()
                    );
                }
            }
        }


        // The UI, in its own pass over the resolved surface. After this point
        // the chart is finished with the frame.
        let ui_actions = if let Some(mut ui) = self.ui.take() {
            let anchor = self.pick_anchor.map(|a| {
                let s = self.camera.world_to_screen(a[0], a[1]);
                let ppp = self.scale_factor.max(0.01);
                [s.x / ppp, s.y / ppp]
            });
            let own_ship = self.own_ship_view();
            let ais = self.ais_targets();
            let mpp = self.camera.zoom * self.scale_factor.max(0.01);
            let actions = ui.run(
                &self.window.clone(),
                &self.device,
                &self.queue,
                &mut encoder,
                &view,
                [self.config.width, self.config.height],
                crate::render::ui::UiState {
                    picked: self.pick_anchor.map(|_| self.pick_objects.as_slice()),
                    pick_anchor: anchor,
                    own_ship,
                    ais,
                    mpp,
                },
                &self.fleet,
            );
            self.ui = Some(ui);
            actions
        } else {
            Vec::new()
        };
        for action in ui_actions {
            match action {
                crate::render::ui::UiAction::DismissPick => self.dismiss_pick(),
                crate::render::ui::UiAction::ShopSignIn { email, password } => {
                    self.shop_sign_in(email, password);
                }
                crate::render::ui::UiAction::ShopRefresh => {
                    self.shop_send(crate::shop::service::Request::Refresh);
                }
                crate::render::ui::UiAction::ShopSignOut => {
                    self.shop_send(crate::shop::service::Request::SignOut);
                }
                crate::render::ui::UiAction::ShopRegister { system_name } => {
                    match Self::machine_fingerprint() {
                        Some(fingerprint) => self.shop_send(
                            crate::shop::service::Request::Register {
                                system_name,
                                fingerprint,
                            },
                        ),
                        None => {
                            if let Some(ref mut ui) = self.ui {
                                ui.shop.status = "No chart licence on this machine, so it \
                                                  cannot be registered."
                                    .into();
                            }
                        }
                    }
                }
                crate::render::ui::UiAction::ShopDownload { chart_id, edition } => {
                    let installed = self
                        .ui
                        .as_ref()
                        .and_then(|u| u.shop.installed.get(&chart_id).copied());
                    let root = self.chart_root.clone().unwrap_or_else(|| "charts".into());
                    if let Some(ref mut ui) = self.ui {
                        ui.shop.pending = None;
                    }
                    self.shop_send(crate::shop::service::Request::Download {
                        chart_id,
                        installed,
                        edition,
                        root,
                    });
                }
                crate::render::ui::UiAction::ShopConfirmDownload { chart_id } => {
                    self.prepare_lapsed_download(&chart_id);
                }
                crate::render::ui::UiAction::ShopCancelDownload => {
                    if let Some(ref mut ui) = self.ui {
                        ui.shop.pending = None;
                    }
                }
                crate::render::ui::UiAction::SignalKConnect { url } => {
                    let stream = crate::signalk::service::normalise_url(&url);
                    if stream.is_empty() {
                        if let Some(ref mut ui) = self.ui {
                            ui.instruments.status = "Enter the address of your Signal K server"
                                .into();
                        }
                    } else {
                        log::info!("signalk: connecting to {stream}");
                        self.signalk
                            .get_or_insert_with(crate::signalk::SignalKService::new)
                            .connect(stream);
                        self.save_settings();
                    }
                    self.needs_redraw = true;
                }
                crate::render::ui::UiAction::SignalKDisconnect => {
                    if let Some(ref sk) = self.signalk {
                        sk.disconnect();
                    }
                    // Drop the readings with the connection. Keeping them would
                    // leave a bar of numbers that look live and are not.
                    self.fleet.clear();
                    if let Some(ref mut ui) = self.ui {
                        ui.instruments.connected = false;
                        ui.instruments.available.clear();
                    }
                    self.needs_redraw = true;
                }
                crate::render::ui::UiAction::SettingsChanged => self.save_settings(),
            }
        }

        self.queue.submit(std::iter::once(encoder.finish()));

        // Headless capture: copy the just-rendered surface texture before it is presented.
        if let Some(path) = self.pending_capture.take() {
            self.save_capture(&output.texture, &path);
        }

        output.present();

        self.needs_redraw = false;

        if let Some(start) = render_start {
            log::info!("profile.render_total: {} ms", start.elapsed().as_millis());
        }

        Ok(())
    }

    fn clamp_camera_to_catalog(&mut self) {
        let Some(catalog) = self.catalog.as_deref() else { return };

        // Allow some panning outside the chart area, but keep the view bounded.
        let extent = catalog.combined_extent.expand(1.2);

        let half_w = self.camera.viewport_width * self.camera.zoom / 2.0;
        let half_h = self.camera.viewport_height * self.camera.zoom / 2.0;

        let min_x = (extent.min_x as f32) + half_w;
        let max_x = (extent.max_x as f32) - half_w;
        let min_y = (extent.min_y as f32) + half_h;
        let max_y = (extent.max_y as f32) - half_h;

        // If the extent is smaller than the viewport at the current zoom there
        // is no meaningful range to clamp into, and the view is centred on the
        // charts instead.
        //
        // Except while following the boat. That case is not hypothetical: at
        // the zoom navcore opens at, a single chart set fits on screen whole,
        // so this branch ran every frame and snapped the camera back a moment
        // after the boat moved it — follow appeared to do nothing at all.
        // Following still stays inside the charts, it simply is not dragged to
        // their middle.
        let centre_x = ((extent.min_x + extent.max_x) / 2.0) as f32;
        let centre_y = ((extent.min_y + extent.max_y) / 2.0) as f32;

        if min_x.is_finite() && max_x.is_finite() && min_x <= max_x {
            self.camera.position.x = self.camera.position.x.clamp(min_x, max_x);
        } else if self.following {
            self.camera.position.x = self
                .camera
                .position
                .x
                .clamp(extent.min_x as f32, extent.max_x as f32);
        } else {
            self.camera.position.x = centre_x;
        }

        if min_y.is_finite() && max_y.is_finite() && min_y <= max_y {
            self.camera.position.y = self.camera.position.y.clamp(min_y, max_y);
        } else if self.following {
            self.camera.position.y = self
                .camera
                .position
                .y
                .clamp(extent.min_y as f32, extent.max_y as f32);
        } else {
            self.camera.position.y = centre_y;
        }
    }

    /// How many missing tiles may be handed to the worker in one frame.
    ///
    /// The cap existed because a request was a commitment: the worker built the
    /// whole batch before reporting anything, so over-requesting meant the next
    /// view waited behind a screenful of tiles it no longer needed. Requests are
    /// now cancellable per tile and results stream back as they finish, so the
    /// budget can be the whole screen — the work that matters starts sooner and
    /// the work that stops mattering is dropped.
    /// Kept modest because each candidate costs a chart-coverage query against
    /// the whole catalogue on the main thread before it is requested; raising
    /// this to a screenful made the frame itself the bottleneck.
    const MAX_TILES_PER_FRAME: usize = 32;

    /// Confine the following draws to a tile's area, or release the scissor.
    /// Draw own ship and the AIS traffic, over everything.
    ///
    /// After the tiles, and only after: `draw_tiles` leaves the scissor set to
    /// whichever tile it drew last, so anything that follows is clipped to
    /// that one rectangle. Releasing it first is not tidiness — without it the
    /// mariner symbols appear only when the boat happens to be inside the last
    /// tile drawn, which looks exactly like a bug in the AIS layer.
    fn draw_mariner_symbols<'a>(&'a self, render_pass: &mut wgpu::RenderPass<'a>) {
        if self.mariner_count == 0 || debug_render_mode() == 3 {
            return;
        }
        let (Some(renderer), Some(bind_group), Some(buffer)) = (
            &self.symbol_renderer,
            &self.symbol_camera_bind_group,
            &self.mariner_buffer,
        ) else {
            return;
        };
        self.apply_scissor(render_pass, None);
        renderer.set_state(render_pass, bind_group);
        renderer.draw_range(render_pass, buffer, 0, self.mariner_count);
    }

    fn apply_scissor<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        scissor: Option<[u32; 4]>,
    ) {
        match scissor {
            Some([x, y, w, h]) => render_pass.set_scissor_rect(x, y, w, h),
            None => render_pass.set_scissor_rect(0, 0, self.size.width, self.size.height),
        }
    }

    /// The scissor rectangle covering a tile's area on screen, in physical
    /// pixels, or None if it is entirely off-screen.
    fn tile_scissor(&self, tile_id: &TileId) -> Option<[u32; 4]> {
        let b = tile_id.bounds();
        // Tilted, a tile is a trapezium on screen, so take the box around all
        // four projected corners rather than two opposite ones.
        let mut x0 = f32::MAX;
        let mut y0 = f32::MAX;
        let mut x1 = f32::MIN;
        let mut y1 = f32::MIN;
        for (wx, wy) in [
            (b.min_x, b.min_y),
            (b.max_x, b.min_y),
            (b.max_x, b.max_y),
            (b.min_x, b.max_y),
        ] {
            let p = self.camera.world_to_screen(wx as f32, wy as f32);
            if !p.x.is_finite() || !p.y.is_finite() {
                return None;
            }
            x0 = x0.min(p.x);
            y0 = y0.min(p.y);
            x1 = x1.max(p.x);
            y1 = y1.max(p.y);
        }
        let x0 = x0.floor().max(0.0);
        let y0 = y0.floor().max(0.0);
        let x1 = x1.ceil().min(self.size.width as f32);
        let y1 = y1.ceil().min(self.size.height as f32);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some([x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32])
    }

    fn build_visible_tiles(&mut self) {
        let t_upload = profile_enabled().then(Instant::now);
        // 1. Upload deferred tiles from last frame first
        let mut uploads_this_frame = 0usize;
        const MAX_UPLOADS_PER_FRAME: usize = 64;
        let deferred = std::mem::take(&mut self.deferred_tile_results);
        for response in deferred {
            if uploads_this_frame >= MAX_UPLOADS_PER_FRAME {
                self.deferred_tile_results.push(response);
                continue;
            }
            match response.result {
                Ok(packet) => {
                    self.tile_cache.upload(&self.device, packet, self.style_hash);
                    uploads_this_frame += 1;
                    self.needs_redraw = true;
                }
                Err(e) => eprintln!("Tile {:?} failed: {}", response.tile_id, e),
            }
        }

        // If there are still deferred tiles, request another frame to drain them
        if !self.deferred_tile_results.is_empty() {
            self.needs_redraw = true;
        }

        // 2. Poll worker for completed tile packets and upload to GPU (capped)
        if let Some(ref worker) = self.tile_worker {
            let results = worker.poll_results();
            for response in results {
                self.pending_tiles.remove(&response.tile_id);
                if uploads_this_frame >= MAX_UPLOADS_PER_FRAME {
                    // Defer excess results to next frame (avoid re-building)
                    self.deferred_tile_results.push(response);
                    continue;
                }
                match response.result {
                    Ok(packet) => {
                        self.tile_cache.upload(
                            &self.device,
                            packet,
                            self.style_hash,
                        );
                        uploads_this_frame += 1;
                        self.needs_redraw = true;
                    }
                    Err(e) => eprintln!("Tile {:?} failed: {}", response.tile_id, e),
                }
            }
        }

        let ms_upload = t_upload.map(|t| t.elapsed().as_micros()).unwrap_or(0);
        let t_select = profile_enabled().then(Instant::now);

        if let Some(ref worker) = self.tile_worker {
            if let Some(answer) = worker.poll_pick() {
                // Only the query still outstanding may open the bubble; an
                // answer to one the user has already dismissed is discarded.
                if self.pick_pending == Some(answer.seq) {
                    self.pick_pending = None;
                    self.pick_anchor =
                        Some([answer.position[0] as f32, answer.position[1] as f32]);
                    self.pick_objects = answer.objects;
                    self.needs_redraw = true;
                }
            }
        }

        // 2. Compute zoom level with hysteresis (prevents z-flip on small zoom changes)
        let raw_z = zoom_from_camera_raw(self.camera.zoom);
        let z = if self.current_z == 0 {
            raw_z.round().clamp(6.0, 18.0) as u8
        } else {
            let diff = raw_z - self.current_z as f64;
            if diff > 0.6 {
                self.current_z + 1
            } else if diff < -0.6 {
                self.current_z.saturating_sub(1).max(6)
            } else {
                self.current_z
            }
        };
        let prev_z = self.current_z;
        self.current_z = z;

        // If z changed, discard previous-z tiles instead of drawing them as fallback.
        // The fallback rectangles create visible tile-aligned artifacts that diverge
        // badly from OpenCPN.
        if z != prev_z && prev_z != 0 {
            self.previous_visible_tiles.clear();
            self.previous_style_hash = 0;
        }

        let view_bounds = self.camera.visible_bounds_tile();
        self.visible_tiles_cache = if self.camera.tilt > 0.0 {
            // Tilted, one level over the whole footprint would either starve
            // the foreground or build the far half at a detail no pixel can
            // show. Refine by distance instead: `z` stays the level the centre
            // of the screen deserves, and the walk coarsens outward from it.
            let cam = self.camera.clone();
            let footprint = cam.ground_footprint();
            let eye = cam.eye();
            crate::tiles::visible_tiles_lod(
                &view_bounds,
                z.saturating_sub(4).max(6),
                z,
                &|x, y| cam.ground_mpp_at(x, y),
                &|b| {
                    // Behind the eye is not "far away", it is off the chart:
                    // it would project back onto the screen mirrored. The
                    // view direction is (0, sin t, -cos t) from an eye at
                    // height d cos t, so a ground point is in front when
                    // (y - eye.y) sin t + d cos^2 t > 0.
                    let (st, ct) = (cam.tilt.sin(), cam.tilt.cos());
                    let d = cam.eye_distance();
                    if (b.max_y as f32 - eye.y) * st + d * ct * ct <= 0.0 {
                        return false;
                    }
                    tile_meets_footprint(b, &footprint)
                },
            )
        } else {
            visible_tiles(&view_bounds, z, 1.2)
        };

        // Diagnostic logging (first 5 frames only)
        use std::sync::atomic::{AtomicU32, Ordering};
        static FRAME_COUNT: AtomicU32 = AtomicU32::new(0);
        let frame = FRAME_COUNT.fetch_add(1, Ordering::Relaxed);
        if log::log_enabled!(log::Level::Debug) && frame < 5 {
            log::debug!("=== FRAME {} BUILD_VISIBLE_TILES ===", frame);
            log::debug!(
                "  Camera: pos=({:.0},{:.0}), zoom={:.1} m/px",
                self.camera.position.x,
                self.camera.position.y,
                self.camera.zoom
            );
        }

        // Sort tiles by distance to camera center (closest first for faster first-paint)
        let cam_x = self.camera.position.x as f64;
        let cam_y = self.camera.position.y as f64;
        self.visible_tiles_cache.sort_by(|a, b| {
            let a_bounds = a.bounds();
            let b_bounds = b.bounds();
            let a_cx = (a_bounds.min_x + a_bounds.max_x) / 2.0;
            let a_cy = (a_bounds.min_y + a_bounds.max_y) / 2.0;
            let b_cx = (b_bounds.min_x + b_bounds.max_x) / 2.0;
            let b_cy = (b_bounds.min_y + b_bounds.max_y) / 2.0;
            let a_dist = (a_cx - cam_x).powi(2) + (a_cy - cam_y).powi(2);
            let b_dist = (b_cx - cam_x).powi(2) + (b_cy - cam_y).powi(2);
            a_dist.partial_cmp(&b_dist).unwrap_or(std::cmp::Ordering::Equal)
        });

        let ms_select = t_select.map(|t| t.elapsed().as_micros()).unwrap_or(0);
        let t_draw = profile_enabled().then(Instant::now);

        // Build the draw list: the tile itself when it is resident, otherwise
        // the nearest resident ancestor clipped to this tile's screen rect.
        self.tile_draw_list.clear();
        let mut with_scissor = Vec::new();
        for tile_id in &self.visible_tiles_cache {
            let key = TileCacheKey::new(*tile_id, self.style_hash);
            if self.tile_cache.contains(&key) {
                self.tile_draw_list.push((key, None));
                continue;
            }
            if let Some(ancestor) =
                nearest_resident_ancestor(*tile_id, |t| {
                    self.tile_cache.contains(&TileCacheKey::new(t, self.style_hash))
                })
            {
                if let Some(rect) = self.tile_scissor(tile_id) {
                    with_scissor.push((TileCacheKey::new(ancestor, self.style_hash), Some(rect)));
                }
            }
        }
        // Unclipped first: the scissor is render-pass state, so grouping the
        // draws that do not need it avoids setting and resetting it per tile.
        self.tile_draw_list.extend(with_scissor);

        let ms_draw = t_draw.map(|t| t.elapsed().as_micros()).unwrap_or(0);
        let t_missing = profile_enabled().then(Instant::now);

        // 3. Find missing tiles not in cache AND not already requested from worker
        let mut missing = Vec::new();
        for tile_id in &self.visible_tiles_cache {
            let key = TileCacheKey::new(*tile_id, self.style_hash);
            if !self.tile_cache.contains(&key)
                && !self.tile_cache.is_known_empty(&key)
                && !self.pending_tiles.contains(tile_id)
            {
                // Quick pre-filter: skip tiles with no scale-appropriate charts.
                // Must match the builder's selection (charts_for_tile_scaled).
                if let Some(ref catalog) = self.catalog {
                    let tile_bounds = tile_id.bounds();
                    let tile_scale_denom = crate::tiles::meters_per_pixel(tile_id.z)
                        * (self.effective_ppmm as f64)
                        * 1000.0;
                    if catalog
                        .charts_for_tile_scaled(&tile_bounds, tile_scale_denom)
                        .is_empty()
                    {
                        self.tile_cache.mark_empty(&key);
                        continue;
                    }
                }
                missing.push(*tile_id);
                if missing.len() >= Self::MAX_TILES_PER_FRAME {
                    break;
                }
            }
        }

        let ms_missing = t_missing.map(|t| t.elapsed().as_micros()).unwrap_or(0);
        if profile_enabled() && (ms_upload + ms_select + ms_draw + ms_missing) > 2000 {
            log::info!(
                "profile.bvt_breakdown: upload={}us select={}us drawlist={}us missing={}us candidates={}",
                ms_upload, ms_select, ms_draw, ms_missing, self.visible_tiles_cache.len()
            );
        }

        // 4. Send missing tiles to background worker
        if !missing.is_empty() {
            if let Some(ref worker) = self.tile_worker {
                let view_params = crate::tiles::worker::ViewParams {
                    meters_per_pixel: self.camera.zoom,
                    ppmm: self.effective_ppmm,
                    width_px: self.size.width as f32,
                    height_px: self.size.height as f32,
                    center_x: self.camera.position.x,
                    center_y: self.camera.position.y,
                };
                // A new request supersedes the last, and the worker abandons
                // whatever of it has not started. Those tiles never report
                // back, so anything still marked pending from the previous
                // generation has to be released or it would never be requested
                // again — a permanent hole in the map.
                self.pending_tiles.clear();
                for tid in &missing {
                    self.pending_tiles.insert(*tid);
                }
                worker.request_tiles(missing, view_params);
            }
        }

        if tile_debug_enabled() {
            let (mut res, mut pend, mut empty, mut other) = (0, 0, 0, 0);
            let mut empties = Vec::new();
            for t in &self.visible_tiles_cache {
                let k = TileCacheKey::new(*t, self.style_hash);
                if self.tile_cache.contains(&k) {
                    res += 1;
                } else if self.pending_tiles.contains(t) {
                    pend += 1;
                } else if self.tile_cache.is_known_empty(&k) {
                    empty += 1;
                    empties.push(format!("z{}/{}/{}", t.z, t.x, t.y));
                } else {
                    other += 1;
                }
            }
            eprintln!(
                "TILES visible={} resident={} pending={} empty={} unknown={} drawn={} empties=[{}]",
                self.visible_tiles_cache.len(),
                res, pend, empty, other,
                self.tile_draw_list.len(),
                empties.join(",")
            );
        }

        // 5. Evict tiles over budget
        self.tile_cache.evict_to_budget();
    }

    /// Draw tiles using cached visible_tiles from build_visible_tiles()
    fn build_global_text_buffers(&mut self) {
        use crate::render::{
            text_layout::{declutter_and_layout_labels_ex, declutter_soundings},
        };
        use wgpu::util::DeviceExt;

        let mut all_soundings = Vec::new();
        let mut all_labels = Vec::new();

        for tile_id in &self.visible_tiles_cache {
            let key = crate::tiles::cache::TileCacheKey::new(*tile_id, self.style_hash);
            if let Some(buffers) = self.tile_cache.get(&key) {
                all_soundings.extend_from_slice(&buffers.text_instances);
                all_labels.extend_from_slice(&buffers.label_candidates);
            }
        }

        declutter_soundings(&mut all_soundings, &self.camera);
        // S-52 portrays soundings with the presentation library's digit
        // symbols. navcore can, and does under NAVCORE_SOUNDING_SYMBOLS=1, but
        // the raster atlas is too coarse to magnify — see
        // `text_layout::sounding_symbols_enabled`.
        let use_symbols = crate::render::text_layout::sounding_symbols_enabled();
        let sounding_symbols = if use_symbols {
            crate::render::text_layout::layout_sounding_symbols(&all_soundings)
        } else {
            Vec::new()
        };
        let sounding_glyphs = if use_symbols {
            Vec::new()
        } else {
            crate::render::text_layout::layout_sounding_labels(&all_soundings)
        };
        // Scale-aware ShowImportantTextOnly: suppress dis >= 20 labels (place
        // names, light descriptions) only at overview zooms. Once zoomed into
        // harbour/approach detail (z >= 14) show them, matching OpenCPN. Mirrors
        // the builder-side TileBuilder::TEXT_DETAIL_ZOOM gate.
        let important_only = self
            .s52_engine
            .as_ref()
            .map(|e| e.settings.show_important_text_only)
            .unwrap_or(false)
            && crate::tiles::zoom_from_camera(self.camera.zoom) < 14;
        let mut decluttered_labels =
            declutter_and_layout_labels_ex(&all_labels, &self.camera, important_only);
        decluttered_labels.extend(sounding_glyphs);

        self.global_text_buffer = None;
        self.global_text_count = 0;

        self.global_sounding_buffer = None;
        self.global_sounding_count = sounding_symbols.len() as u32;
        if self.global_sounding_count > 0 {
            self.global_sounding_buffer =
                Some(self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("Global Sounding Symbol Buffer"),
                    contents: bytemuck::cast_slice(&sounding_symbols),
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                }));
        }

        self.global_label_buffer = None;
        self.global_label_count = decluttered_labels.len() as u32;
        if self.global_label_count > 0 {
            self.global_label_buffer = Some(self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("Global Label Buffer"),
                contents: bytemuck::cast_slice(&decluttered_labels),
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            }));
        }
    }

    /// Record what the draw pass actually issued for a tile.
    ///
    /// The scene dump (`navcore --dump-scene`) proves what the *builder*
    /// decided; this proves what the *renderer* did with it. Between them sits
    /// upload, cache residency and per-priority slicing — where geometry that
    /// exists in a tile packet can still never reach the screen.
    fn log_draw(&self, rec: serde_json::Value) {
        if let Some(log) = &self.draw_log {
            log.borrow_mut().push(rec);
        }
    }

    fn draw_tiles<'a>(&'a self, render_pass: &mut wgpu::RenderPass<'a>, line_style_offsets: &HashMap<LineStyleKey, u32>) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static DRAW_FRAME_COUNT: AtomicU32 = AtomicU32::new(0);
        let draw_frame = DRAW_FRAME_COUNT.fetch_add(1, Ordering::Relaxed);
        let debug_mode = debug_render_mode();

        // === Priority-ordered drawing ===
        // S-52 requires interleaved draw order: for each priority level (0-9),
        // draw areas → patterns → lines → symbols, then text on top.

        // Pre-collect and sort all line batches by priority
        let mut legacy_draws: Vec<(&LineBatchGpu, LineStyleKey, Option<[u32; 4]>)> = Vec::new();
        for (key, scissor) in &self.tile_draw_list {
            if let Some(buffers) = self.tile_cache.get(key) {
                for batch in &buffers.line_batches {
                    if batch.index_count > 0 {
                        legacy_draws.push((batch, batch.key.style.clone(), *scissor));
                    }
                }
            }
        }
        legacy_draws.sort_by(|a, b| a.0.key.cmp(&b.0.key));

        let mut drew_tiles = 0usize;
        let mut drew_line_batches = 0usize;
        let mut uniform_updates = 0usize;
        let mut drew_symbols = 0u32;
        let mut symbol_draw_calls = 0u32;
        let mut line_index_count = 0u32;
        let mut line_vertex_count = 0u32;
        let mut last_style: Option<LineStyleKey> = None;

        // Pre-compute line batch index ranges per priority
        let mut legacy_prio_ranges: [(usize, usize); 10] = [(0, 0); 10];
        {
            let mut start = 0;
            for prio in 0..10u8 {
                let end = legacy_draws.partition_point(|(b, _, _)| b.key.disp_prio <= prio);
                legacy_prio_ranges[prio as usize] = (start, end);
                start = end;
            }
        }

        if self.draw_log.is_some() {
            for tile_id in &self.visible_tiles_cache {
                let key = TileCacheKey::new(*tile_id, self.style_hash);
                match self.tile_cache.get(&key) {
                    Some(b) => self.log_draw(serde_json::json!({
                        "record": "tile",
                        "tile": [tile_id.z, tile_id.x, tile_id.y],
                        "resident": true,
                        "area_vertex_offsets": b.area_priority_offsets,
                        "bg_area_vertex_offsets": b.bg_area_priority_offsets,
                        "line_batches": b.line_batches.len(),
                    })),
                    None => self.log_draw(serde_json::json!({
                        "record": "tile",
                        "tile": [tile_id.z, tile_id.x, tile_id.y],
                        "resident": false,
                        "note": "visible but not in the GPU tile cache — nothing of it is drawn",
                    })),
                }
            }
        }

        // NAVCORE_MAX_PRIO=<n> stops the draw loop after priority n. Bisecting
        // the priority stack is the fastest way to find which layer is painting
        // over another: the scene dump says what *should* be there, this says
        // which pass removed it.
        let max_prio: u8 = std::env::var("NAVCORE_MAX_PRIO")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(9);

        for priority in 0..10u8 {
            if priority > max_prio {
                break;
            }
            let p = priority as usize;

            // --- Background areas at this priority (stencil test: pass when stencil==0) ---
            render_pass.set_pipeline(&self.bg_pipeline);
            render_pass.set_bind_group(0, &self.uniform_bind_group, &[]);
            render_pass.set_stencil_reference(0);
            for (key, scissor) in &self.tile_draw_list {
                let _ = &key.tile_id;
                if let Some(buffers) = self.tile_cache.get(key) {
                    self.apply_scissor(render_pass, *scissor);
                    if let Some(ref bg_buf) = buffers.bg_area_buffer {
                        let start = buffers.bg_area_priority_offsets[p];
                        let end = buffers.bg_area_priority_offsets[p + 1];
                        if end > start {
                            render_pass.set_vertex_buffer(0, bg_buf.slice(..));
                            render_pass.draw(start..end, 0..1);
                            drew_tiles += 1;
                        }
                    }
                }
            }

            // --- Foreground areas at this priority (no stencil test) ---
            render_pass.set_pipeline(&self.pipeline);
            render_pass.set_bind_group(0, &self.uniform_bind_group, &[]);
            for (key, scissor) in &self.tile_draw_list {
                let _ = &key.tile_id;
                if let Some(buffers) = self.tile_cache.get(key) {
                    self.apply_scissor(render_pass, *scissor);
                    let start = buffers.area_priority_offsets[p];
                    let end = buffers.area_priority_offsets[p + 1];
                    if end > start {
                        render_pass.set_vertex_buffer(0, buffers.area_buffer.slice(..));
                        render_pass.draw(start..end, 0..1);
                        drew_tiles += 1;
                        self.log_draw(serde_json::json!({
                            "record": "draw", "kind": "area-fg",
                            "tile": [key.tile_id.z, key.tile_id.x, key.tile_id.y],
                            "fallback": scissor.is_some(),
                            "priority": priority, "vertices": end - start,
                        }));
                    }
                }
            }

            // --- Patterns at this priority (bg then fg) ---
            if let (Some(ref pattern_renderer), Some(ref pattern_bind_group)) =
                (&self.pattern_renderer, &self.pattern_camera_bind_group)
            {
                let mut bg_pattern_state_set = false;
                let mut fg_pattern_state_set = false;
                for (key, scissor) in &self.tile_draw_list {
                    let _ = &key.tile_id;
                    if let Some(buffers) = self.tile_cache.get(key) {
                        self.apply_scissor(render_pass, *scissor);
                        // Draw bg patterns
                        if let Some(ref bg_pat_buf) = buffers.bg_pattern_buffer {
                            let start = buffers.bg_pattern_priority_offsets[p];
                            let end = buffers.bg_pattern_priority_offsets[p + 1];
                            if end > start {
                                if !bg_pattern_state_set {
                                    pattern_renderer
                                        .set_background_state(render_pass, pattern_bind_group);
                                    render_pass.set_stencil_reference(0);
                                    bg_pattern_state_set = true;
                                }
                                pattern_renderer.draw_range(
                                    render_pass,
                                    bg_pat_buf,
                                    start,
                                    end - start,
                                );
                            }
                        }
                        // Draw fg patterns
                        if let Some(ref pattern_buffer) = buffers.pattern_buffer {
                            let start = buffers.pattern_priority_offsets[p];
                            let end = buffers.pattern_priority_offsets[p + 1];
                            if end > start {
                                if !fg_pattern_state_set {
                                    pattern_renderer.set_state(render_pass, pattern_bind_group);
                                    fg_pattern_state_set = true;
                                }
                                pattern_renderer.draw_range(
                                    render_pass,
                                    pattern_buffer,
                                    start,
                                    end - start,
                                );
                            }
                        }
                    }
                }
            }

            // --- Lines at this priority ---
            // Legacy draws are sorted by LineBatchKey which puts bg before fg.
            // Split the range into bg and fg sub-ranges.
            let (l_start, l_end) = legacy_prio_ranges[p];

            // Find the split point: bg batches (is_background=true) sort first
            let bg_end = legacy_draws[l_start..l_end]
                .partition_point(|(b, _, _)| b.key.is_background)
                + l_start;

            // Background lines (stencil test: pass when stencil==0)
            if bg_end > l_start {
                render_pass.set_pipeline(&self.bg_line_pipeline);
                render_pass.set_bind_group(0, &self.uniform_bind_group, &[]);
                render_pass.set_stencil_reference(0);
                for (batch, style_key, scissor) in &legacy_draws[l_start..bg_end] {
                    if last_style.as_ref() != Some(style_key) {
                        last_style = Some(style_key.clone());
                        uniform_updates += 1;
                    }
                    let offset = line_style_offsets.get(style_key).copied().unwrap_or(0);
                    self.apply_scissor(render_pass, *scissor);
                    render_pass.set_bind_group(1, &self.line_bind_group, &[offset]);
                    render_pass.set_vertex_buffer(0, batch.vertex_buffer.slice(..));
                    render_pass.set_index_buffer(batch.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                    render_pass.draw_indexed(0..batch.index_count, 0, 0..1);
                    drew_line_batches += 1;
                    line_index_count += batch.index_count;
                    line_vertex_count += batch.vertex_count;
                }
            }

            // Foreground lines (no stencil test)
            if l_end > bg_end {
                render_pass.set_pipeline(&self.line_pipeline);
                render_pass.set_bind_group(0, &self.uniform_bind_group, &[]);
                for (batch, style_key, scissor) in &legacy_draws[bg_end..l_end] {
                    if last_style.as_ref() != Some(style_key) {
                        last_style = Some(style_key.clone());
                        uniform_updates += 1;
                    }
                    let offset = line_style_offsets.get(style_key).copied().unwrap_or(0);
                    self.apply_scissor(render_pass, *scissor);
                    render_pass.set_bind_group(1, &self.line_bind_group, &[offset]);
                    render_pass.set_vertex_buffer(0, batch.vertex_buffer.slice(..));
                    render_pass.set_index_buffer(batch.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                    render_pass.draw_indexed(0..batch.index_count, 0, 0..1);
                    drew_line_batches += 1;
                    line_index_count += batch.index_count;
                    line_vertex_count += batch.vertex_count;
                }
            }

            // --- Symbols at this priority ---
            if let (Some(ref symbol_renderer), Some(ref symbol_bind_group)) =
                (&self.symbol_renderer, &self.symbol_camera_bind_group)
            {
                if debug_mode == 3 && priority == 0 {
                    symbol_renderer.render_atlas_debug(render_pass, symbol_bind_group);
                    symbol_draw_calls = 1;
                } else if debug_mode != 3 {
                    let mut symbol_state_set = false;
                    for (key, scissor) in &self.tile_draw_list {
                        if let Some(buffers) = self.tile_cache.get(key) {
                            self.apply_scissor(render_pass, *scissor);
                            if let Some(ref symbol_buffer) = buffers.symbol_buffer {
                                let start = buffers.symbol_priority_offsets[p];
                                let end = buffers.symbol_priority_offsets[p + 1];
                                if end > start {
                                    if !symbol_state_set {
                                        symbol_renderer.set_state(render_pass, symbol_bind_group);
                                        symbol_state_set = true;
                                    }
                                    symbol_renderer.draw_range(
                                        render_pass,
                                        symbol_buffer,
                                        start,
                                        end - start,
                                    );
                                    drew_symbols += end - start;
                                    symbol_draw_calls += 1;
                                }
                            }
                        }
                    }
                }
            }
        } // end priority loop

        if log::log_enabled!(log::Level::Debug) && (draw_frame < 5 || draw_frame % 120 == 0) {
            let cached = self.visible_tiles_cache.iter().filter(|t| {
                self.tile_cache.contains(&TileCacheKey::new(**t, self.style_hash))
            }).count();
            log::debug!(
                "draw_tiles: visible={}, cached={}, area_draws={}, line_batches={} uniform_updates={} symbols={}",
                self.visible_tiles_cache.len(),
                cached,
                drew_tiles,
                drew_line_batches,
                uniform_updates,
                drew_symbols,
            );
        }
        if debug_mode != 0 {
            log::info!(
                "debug_render: tile lines vertices={} indices={} batches={} symbols={} draws={} mode={}",
                line_vertex_count,
                line_index_count,
                drew_line_batches,
                drew_symbols,
                symbol_draw_calls,
                debug_mode
            );
        }

        // 4) Text (soundings)
        if let (Some(ref text_renderer), Some(ref text_bind_group)) =
            (&self.text_renderer, &self.text_camera_bind_group)
        {
            if let Some(ref text_buffer) = self.global_text_buffer {
                if self.global_text_count > 0 {
                    text_renderer.render_with_buffer(
                        render_pass,
                        text_bind_group,
                        text_buffer,
                        self.global_text_count,
                    );
                    if log::log_enabled!(log::Level::Debug) && draw_frame < 5 {
                        log::debug!("draw_tiles: text_drew={}", self.global_text_count);
                    }
                }
            }
        }

        // The global passes below are not per-tile; release any scissor the
        // hole-filling draws left set.
        self.apply_scissor(render_pass, None);

        // 4b) Soundings, as S-52 digit symbols
        if let (Some(ref symbol_renderer), Some(ref symbol_bind_group), Some(ref buf)) = (
            &self.symbol_renderer,
            &self.symbol_camera_bind_group,
            &self.global_sounding_buffer,
        ) {
            if self.global_sounding_count > 0 {
                symbol_renderer.render_with_buffer(
                    render_pass,
                    symbol_bind_group,
                    buf,
                    self.global_sounding_count,
                );
            }
        }

        // 5) Labels (non-numeric text)
        if let (Some(ref label_renderer), Some(ref label_bind_group)) =
            (&self.label_renderer, &self.label_camera_bind_group)
        {
            if let Some(ref label_buffer) = self.global_label_buffer {
                if self.global_label_count > 0 {
                    label_renderer.render_with_buffer(
                        render_pass,
                        label_bind_group,
                        label_buffer,
                        self.global_label_count,
                    );
                    if log::log_enabled!(log::Level::Debug) && draw_frame < 5 {
                        log::debug!("draw_tiles: labels_drew={}", self.global_label_count);
                    }
                }
            }
        }

    }

    /// Create the UI. Separate from `new` because it needs the window.
    ///
    /// `NAVCORE_UI=0` leaves it out entirely, which is how the parity captures
    /// get a frame of chart with no interface over it.
    pub fn init_ui(&mut self) {
        if std::env::var("NAVCORE_UI").is_ok_and(|v| v == "0") {
            return;
        }
        if self.ui.is_none() {
            self.ui = Some(crate::render::ui::Ui::new(
                &self.device,
                &self.window,
                self.config.format,
            ));
            // `NAVCORE_SHOP=1` opens the chart shop at startup, so it can be
            // captured without a click.
            if let Some(ui) = self.ui.as_mut() {
                if std::env::var("NAVCORE_SHOP").is_ok_and(|v| v != "0") {
                    ui.shop.open = true;
                    ui.shop.email = std::env::var("NAVCORE_SHOP_EMAIL").unwrap_or_default();
                }
                // `NAVCORE_SIGNALK=<address>` overrides the saved server, for
                // testing against a particular one without touching settings.
                if let Ok(url) = std::env::var("NAVCORE_SIGNALK") {
                    if !url.is_empty() {
                        ui.instruments.url = url;
                    }
                }
            }
            // Open the stream to the server this plotter was last using. A
            // chart plotter that has to be told to reconnect every time the
            // boat's power cycles is not a chart plotter.
            let url = self
                .ui
                .as_ref()
                .map(|u| u.instruments.url.clone())
                .unwrap_or_default();
            let stream = crate::signalk::service::normalise_url(&url);
            if !stream.is_empty() {
                log::info!("signalk: reconnecting to {stream}");
                self.signalk
                    .get_or_insert_with(crate::signalk::SignalKService::new)
                    .connect(stream);
            }
        }
    }

    /// Offer a window event to the UI first. `true` means it was consumed and
    /// the chart must ignore it.
    ///
    /// Also honours egui's request for a repaint, which is not optional. egui
    /// asks for one on every pointer move, and its idea of what the pointer is
    /// over is whatever the last frame computed. Skip the repaints and that
    /// answer goes stale: the cursor sits on a text field while egui still
    /// believes it is over the chart, so the click falls through and drops a
    /// pin. Hover highlights, tooltips and cursor shapes all die the same way.
    pub fn ui_on_window_event(&mut self, event: &winit::event::WindowEvent) -> bool {
        let window = self.window.clone();
        match self.ui {
            Some(ref mut ui) => {
                let response = ui.on_window_event(&window, event);
                if response.repaint {
                    self.needs_redraw = true;
                }
                response.consumed
            }
            None => false,
        }
    }

    /// Is the pointer over the interface rather than the chart?
    pub fn ui_pointer_over(&self) -> bool {
        self.ui.as_ref().is_some_and(|u| u.pointer_over_ui())
    }

    /// Ask what chart objects are under a screen position, for the info bubble.
    ///
    /// The answer comes back from the tile worker, which already holds the
    /// parsed cells. The tolerance is a fixed number of screen pixels turned
    /// into metres, so clicking near a buoy finds it at any zoom.
    pub fn pick_at_screen(&mut self, x: f32, y: f32) {
        let world = self.camera.screen_to_world(x, y);
        self.pick_at_world(world.x, world.y);
    }

    /// As [`pick_at_screen`](Self::pick_at_screen), for a position already in
    /// Mercator metres. Used by `NAVCORE_PICK` so a capture can show the bubble.
    pub fn pick_at_world(&mut self, x: f32, y: f32) {
        // A fingertip is about 9 mm across; a mouse pointer is exact. Sizing
        // the tolerance in millimetres rather than pixels makes the same tap
        // work on a plotter's touchscreen and on a desktop, at any density.
        let radius_px = 2.0 * self.effective_ppmm.max(1.0);
        self.pick_anchor = None;
        self.pick_objects.clear();
        if let Some(ref worker) = self.tile_worker {
            self.pick_pending = Some(worker.request_pick(
                [x as f64, y as f64],
                (radius_px * self.camera.zoom) as f64,
            ));
        }
        self.needs_redraw = true;
    }

    /// Is a query still with the worker?
    pub fn pick_in_flight(&self) -> bool {
        self.pick_pending.is_some()
    }

    /// Close the info bubble.
    pub fn dismiss_pick(&mut self) {
        // Also drops any answer still in flight, or a query the user cancelled
        // would re-open the bubble when it landed.
        self.pick_pending = None;
        if self.pick_anchor.is_some() {
            self.pick_anchor = None;
            self.pick_objects.clear();
            self.needs_redraw = true;
        }
    }

    /// Is the info bubble showing?
    pub fn pick_active(&self) -> bool {
        self.pick_anchor.is_some() || self.pick_pending.is_some()
    }

    /// Where the instrument layout is kept between runs.
    ///
    /// A dashboard someone arranged at the chart table must still be there
    /// when they start the engine.
    fn settings_path() -> Option<std::path::PathBuf> {
        Some(dirs::config_dir()?.join("navcore").join("settings.json"))
    }

    fn save_settings(&self) {
        let (Some(ui), Some(path)) = (self.ui.as_ref(), Self::settings_path()) else {
            return;
        };
        let Ok(json) = serde_json::to_string_pretty(&ui.instruments) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&path, json) {
            // Not fatal: the session works, it just will not be remembered.
            log::warn!("could not save settings to {}: {e}", path.display());
        }
    }

    pub(crate) fn load_settings() -> Option<crate::render::ui::InstrumentView> {
        let path = Self::settings_path()?;
        let text = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str(&text) {
            Ok(v) => Some(v),
            Err(e) => {
                // A settings file from an older build should cost the user
                // their layout, not their session.
                log::warn!("ignoring unreadable settings at {}: {e}", path.display());
                None
            }
        }
    }

    /// Project the boat onto the screen for the overlay.
    ///
    /// The renderer does this rather than the UI because the camera lives
    /// here, and because the UI should not learn about Mercator to draw a
    /// triangle.
    fn own_ship_view(&self) -> Option<crate::render::ui_ownship::OwnShip> {
        let (lat, lon) = self.fleet.own.position()?;
        let (wx, wy) = crate::render::projection::Projection::to_mercator(lat, lon);
        let screen = self.camera.world_to_screen(wx as f32, wy as f32);
        let ppp = self.scale_factor.max(0.01);

        // The fix, not the heading, decides staleness: a boat that has lost
        // its GPS must not keep drawing itself somewhere plausible.
        let stale = self
            .fleet
            .own
            .get("navigation.position")
            .map(|r| r.is_stale())
            .unwrap_or(true);

        Some(crate::render::ui_ownship::OwnShip {
            screen: [screen.x / ppp, screen.y / ppp],
            heading: self.fleet.own.heading_true().map(|h| h as f32),
            cog: self
                .fleet
                .own
                .number("navigation.courseOverGroundTrue")
                .map(|c| c as f32),
            sog: self
                .fleet
                .own
                .number("navigation.speedOverGround")
                .map(|s| s as f32),
            // Metres per *logical point*, so the course vector is a real
            // distance whatever the display's pixel density.
            mpp: self.camera.zoom * ppp,
            stale,
        })
    }

    /// Rebuild the mariner symbol buffer from where the boats now are.
    ///
    /// Called each frame the fleet changed. The buffer only grows: a harbour
    /// that briefly shows forty targets should not reallocate every frame
    /// afterwards as they come and go.
    fn update_mariner_symbols(&mut self) {
        let Some(symbols) = self.mariner_symbols else {
            return;
        };
        let stale_own = self
            .fleet
            .own
            .get("navigation.position")
            .map(|r| r.is_stale())
            .unwrap_or(true);
        let instances = crate::render::mariner::instances(&self.fleet, &symbols, stale_own);
        self.mariner_count = instances.len() as u32;
        if instances.is_empty() {
            return;
        }

        let bytes: &[u8] = bytemuck::cast_slice(&instances);
        let needed = bytes.len() as u64;
        let big_enough = self
            .mariner_buffer
            .as_ref()
            .is_some_and(|b| b.size() >= needed);
        if !big_enough {
            use wgpu::util::DeviceExt;
            self.mariner_buffer = Some(self.device.create_buffer_init(
                &wgpu::util::BufferInitDescriptor {
                    label: Some("mariner symbols"),
                    contents: bytes,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                },
            ));
        } else if let Some(ref buffer) = self.mariner_buffer {
            self.queue.write_buffer(buffer, 0, bytes);
        }
    }

    /// Project the AIS traffic onto the screen, with its closest approaches.
    ///
    /// CPA is computed geodesically rather than in the Mercator plane: at 56°
    /// north a Mercator northing is stretched by nearly a factor of two, so a
    /// range measured there would be wrong in exactly the situation the number
    /// exists for.
    fn ais_targets(&self) -> Vec<crate::render::ui_ais::AisTarget> {
        use crate::geo::{cpa, LatLon, Motion};

        let ppp = self.scale_factor.max(0.01);
        let own = self.fleet.own.position().map(|(lat, lon)| Motion {
            at: LatLon::new(lat, lon),
            course: self
                .fleet
                .own
                .number("navigation.courseOverGroundTrue")
                .or_else(|| self.fleet.own.heading_true())
                .unwrap_or(0.0),
            speed: self
                .fleet
                .own
                .number("navigation.speedOverGround")
                .unwrap_or(0.0),
        });

        self.fleet
            .targets()
            .filter_map(|target| {
                let (lat, lon) = target.position()?;
                let (wx, wy) = crate::render::projection::Projection::to_mercator(lat, lon);
                let screen = self.camera.world_to_screen(wx as f32, wy as f32);

                let approach = own.and_then(|own| {
                    let theirs = Motion {
                        at: LatLon::new(lat, lon),
                        course: target.course()?,
                        speed: target.speed()?,
                    };
                    let c = cpa(own, theirs)?;
                    (!c.past).then_some((c.distance_m as f32, c.seconds as f32))
                });

                Some(crate::render::ui_ais::AisTarget {
                    screen: [screen.x / ppp, screen.y / ppp],
                    heading: target.heading().map(|h| h as f32),
                    cog: target.course().map(|c| c as f32),
                    sog: target.speed().map(|s| s as f32),
                    label: target.label().map(str::to_string),
                    under_way: target.under_way(),
                    lost: target.is_lost(),
                    cpa: approach,
                })
            })
            .collect()
    }

    /// Fold whatever the boat has said into the vessel state.
    fn poll_signalk(&mut self) {
        let Some(ref sk) = self.signalk else { return };
        let events = sk.poll();
        if events.is_empty() {
            return;
        }
        for event in events {
            match event {
                crate::signalk::service::Event::Status(s) => {
                    if let Some(ref mut ui) = self.ui {
                        ui.instruments.connected = s.is_connected();
                        ui.instruments.status = s.summary();
                    }
                }
                crate::signalk::service::Event::Delta(d) => self.fleet.apply(&d),
                crate::signalk::service::Event::SelfContext(ctx) => {
                    self.fleet.set_self_context(ctx)
                }
            }
        }
        // Drop targets nothing has been heard from in a long time. Done here
        // rather than on receipt: a fleet that only shrinks when a different
        // vessel reports would keep a lone ghost for ever on a quiet sea.
        self.fleet.forget_stale();

        let mut follow = false;
        if let Some(ref mut ui) = self.ui {
            // The picker offers what the boat actually sends, which is the
            // only list worth showing: a menu of every path Signal K defines
            // would be hundreds long and mostly absent.
            if ui.instruments.available.len() != self.fleet.own.len() {
                ui.instruments.available = self.fleet.own.paths().map(str::to_string).collect();
            }
            follow = ui.instruments.follow;
        }

        self.following = false;
        // Keep the boat centred, if asked. Only while the fix is fresh: a
        // chart that keeps recentring on a dead position is worse than one
        // that stays put and lets the stale marker drift off.
        if follow {
            if let Some((lat, lon)) = self.fleet.own.position() {
                let fresh = self
                    .fleet
                    .own
                    .get("navigation.position")
                    .is_some_and(|r| !r.is_stale());
                if fresh {
                    let (x, y) =
                        crate::render::projection::Projection::to_mercator(lat, lon);
                    self.camera.position = glam::Vec2::new(x as f32, y as f32);
                    self.following = true;
                }
            }
        }
        self.needs_redraw = true;
    }

    fn shop_send(&mut self, request: crate::shop::service::Request) {
        let shop = self
            .shop
            .get_or_insert_with(crate::shop::service::ShopService::new);
        shop.send(request);
        if let Some(ref mut ui) = self.ui {
            ui.shop.busy = true;
        }
        self.needs_redraw = true;
    }

    /// This machine's o-charts fingerprint, if it has a licence at all.
    /// Work out which edition a lapsed licence may still ask for, and put it
    /// to the user.
    ///
    /// Two sources, best first: the shop's own record of what this machine's
    /// slot last received, then the edition installed here. Either is an
    /// edition the licence demonstrably covered; the shop's current one is
    /// not, which is why asking for it comes back refused.
    fn prepare_lapsed_download(&mut self, chart_id: &str) {
        let Some(ref mut ui) = self.ui else { return };
        let Some(chart) = ui.shop.charts.iter().find(|c| c.id == chart_id) else {
            return;
        };

        let from_slot = ui
            .shop
            .system_name
            .as_deref()
            .and_then(|name| chart.slot_for(name))
            .map(|(_, s)| s.last_requested.trim().to_string())
            .filter(|v| !v.is_empty());
        let from_disk = ui.shop.installed.get(chart_id).map(|e| e.to_string());

        let (edition, because) = match (from_slot, from_disk) {
            (Some(slot), _) => (
                Some(slot.clone()),
                format!(
                    "The shop has {}, published after your licence expired — it will not \
                     grant that. Edition {slot} is the last one the shop recorded for this \
                     machine, and your licence covered it.",
                    chart.edition
                ),
            ),
            (None, Some(disk)) => (
                Some(disk.clone()),
                format!(
                    "The shop has {}, published after your licence expired — it will not \
                     grant that. Edition {disk} is what is installed here, so your licence \
                     covered it.",
                    chart.edition
                ),
            ),
            (None, None) => (
                None,
                format!(
                    "The shop has {}, published after your licence expired. navcore cannot \
                     tell which edition you last held — nothing for this set is installed \
                     here and the shop recorded no earlier request.",
                    chart.edition
                ),
            ),
        };

        ui.shop.pending = Some(crate::render::ui::PendingDownload {
            chart_id: chart_id.to_string(),
            chart_name: chart.name.clone(),
            edition,
            because,
        });
    }

    fn machine_fingerprint() -> Option<crate::shop::Fingerprint> {
        crate::decrypt::ChartDecryptor::new("license")
            .ok()
            .and_then(|d| crate::shop::Fingerprint::load(d.fpr_path()).ok())
    }

    /// Sign in, sending this machine's fingerprint so the listing can say which
    /// charts are usable here rather than merely owned.
    fn shop_sign_in(&mut self, email: String, password: String) {
        let fingerprint = Self::machine_fingerprint();
        if fingerprint.is_none() {
            if let Some(ref mut ui) = self.ui {
                ui.shop.status =
                    "No chart licence found on this machine; charts can be listed but not \
                     assigned here."
                        .into();
            }
        }
        self.shop_send(crate::shop::service::Request::SignIn {
            email,
            password,
            fingerprint,
        });
    }

    /// Drain the shop worker into the panel.
    fn poll_shop(&mut self) {
        use crate::shop::service::Event;
        let (Some(shop), Some(ui)) = (self.shop.as_ref(), self.ui.as_mut()) else {
            return;
        };
        for event in shop.poll() {
            match event {
                Event::Status(text) => ui.shop.status = text,
                Event::SignedIn { system_name } => {
                    ui.shop.signed_in = true;
                    ui.shop.system_name = system_name;
                    ui.shop.password.clear();
                }
                Event::Charts { charts, systems } => {
                    // Key each listed chart to what is on disk. Without this
                    // the panel cannot say which edition is held, and a lapsed
                    // subscription has nothing to ask the shop for except the
                    // current edition — which is the one it will not grant.
                    let root = self.chart_root.clone().unwrap_or_else(|| "charts".into());
                    let on_disk = crate::shop::installed::scan(&root);
                    let mut installed: std::collections::HashMap<_, _> = charts
                        .iter()
                        .filter_map(|c| {
                            let stem = c.set_stem()?;
                            let edition =
                                crate::shop::installed::newest_for(&on_disk, &stem)?;
                            Some((c.id.clone(), edition))
                        })
                        .collect();
                    // The stem comes from the shop's ChartList links, and not
                    // every reply carries them. One chart set owned and one
                    // installed is unambiguous without any key at all — and
                    // it is much the commonest case, one region per boat.
                    if installed.is_empty() && charts.len() == 1 && on_disk.len() == 1 {
                        installed.insert(charts[0].id.clone(), on_disk[0].edition);
                    }
                    log::info!(
                        "chart shop: {} set(s) on disk, {} matched to the listing",
                        on_disk.len(),
                        installed.len()
                    );
                    ui.shop.installed = installed;
                    ui.shop.charts = charts;
                    ui.shop.systems = systems;
                    ui.shop.busy = false;
                    ui.shop.status = String::new();
                }
                Event::SignedOut => {
                    ui.shop.signed_in = false;
                    ui.shop.charts.clear();
                    ui.shop.system_name = None;
                    ui.shop.busy = false;
                    ui.shop.status = String::new();
                }
                Event::Progress {
                    chart_id,
                    done,
                    total,
                } => {
                    ui.shop.grants.insert(
                        chart_id,
                        if total > 0 {
                            format!("{} of {} MB", done / 1_000_000, total / 1_000_000)
                        } else {
                            format!("{} MB", done / 1_000_000)
                        },
                    );
                }
                Event::Installed { chart_id, summary } => {
                    ui.shop.grants.insert(chart_id, format!("installed: {summary}"));
                    ui.shop.busy = false;
                    ui.shop.status =
                        "Installed. Restart navcore to draw the new charts.".into();
                }
                Event::Grant { chart_id, summary } => {
                    ui.shop.grants.insert(chart_id, summary);
                    ui.shop.busy = false;
                    ui.shop.status = String::new();
                }
                Event::Failed(text) => {
                    ui.shop.busy = false;
                    ui.shop.status = text;
                }
            }
            self.needs_redraw = true;
        }
    }

    /// Does the interface still have a frame it wants to draw?
    pub fn ui_wants_repaint(&self) -> bool {
        self.ui.as_ref().is_some_and(|u| u.wants_repaint())
    }

    /// Get window reference for redraw requests
    pub fn window(&self) -> &Window {
        &self.window
    }

    /// Check if a redraw is needed
    pub fn needs_redraw(&self) -> bool {
        self.needs_redraw
    }

    /// Check if there are tiles still pending from the worker
    pub fn pending_tiles_empty(&self) -> bool {
        self.pending_tiles.is_empty()
            && self.deferred_tile_results.is_empty()
            && self.previous_visible_tiles.is_empty()
    }

    /// Mark that a redraw is needed (called on camera move, etc.)
    pub fn mark_dirty(&mut self) {
        self.needs_redraw = true;
    }

    /// Request that the next rendered frame be saved to `path` as a PNG (headless capture).
    pub fn request_capture(&mut self, path: String) {
        self.pending_capture = Some(path);
        self.needs_redraw = true;
    }

    /// True while a capture has been requested but not yet written.
    pub fn capture_pending(&self) -> bool {
        self.pending_capture.is_some()
    }

    /// Copy a rendered surface texture to CPU and write it as a PNG. Used by NAVCORE_SHOT.
    fn save_capture(&self, texture: &wgpu::Texture, path: &str) {
        let width = self.config.width;
        let height = self.config.height;
        let bpp = 4u32;
        let unpadded = width * bpp;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded = ((unpadded + align - 1) / align) * align;

        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("capture-readback"),
            size: (padded * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("capture") });
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &buffer,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        self.queue.submit(std::iter::once(encoder.finish()));

        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device.poll(wgpu::Maintain::Wait);
        if rx.recv().map(|r| r.is_err()).unwrap_or(true) {
            eprintln!("NAVCORE_SHOT: failed to map readback buffer");
            return;
        }

        let data = slice.get_mapped_range();
        let is_bgra = matches!(
            self.config.format,
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
        );
        let mut pixels = Vec::with_capacity((unpadded * height) as usize);
        for row in 0..height {
            let start = (row * padded) as usize;
            let line = &data[start..start + unpadded as usize];
            for px in line.chunks_exact(4) {
                if is_bgra {
                    pixels.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                } else {
                    pixels.extend_from_slice(&[px[0], px[1], px[2], px[3]]);
                }
            }
        }
        drop(data);
        buffer.unmap();

        match image::RgbaImage::from_raw(width, height, pixels) {
            Some(img) => match img.save(path) {
                Ok(()) => {
                    eprintln!("NAVCORE_SHOT: wrote {} ({}x{})", path, width, height);
                    if let (Ok(dump), Some(log)) =
                        (std::env::var("NAVCORE_DUMP_DRAW"), &self.draw_log)
                    {
                        let lines: Vec<String> = log
                            .borrow()
                            .iter()
                            .map(|v| v.to_string())
                            .collect();
                        if std::fs::write(&dump, lines.join("\n") + "\n").is_ok() {
                            eprintln!(
                                "NAVCORE_DUMP_DRAW: wrote {} draw records to {}",
                                lines.len(),
                                dump
                            );
                        }
                    }
                }
                Err(e) => eprintln!("NAVCORE_SHOT: save failed: {}", e),
            },
            None => eprintln!("NAVCORE_SHOT: bad capture buffer dimensions"),
        }
    }
}

/// S-57 type codes for area features (used by legacy load_chart path)
const BUAARE: u16 = 13;  // Built-up area
const LAKARE: u16 = 69;  // Lake

// Old tessellate_line_with_max and tessellate_polyline_continuous functions
// have been DELETED as part of migration to shader-based polyline rendering.
// See line.wgsl and build_line_vertices() for the new implementation.

impl Drop for RenderState {
    fn drop(&mut self) {
        // Shut down background tile worker thread cleanly
        if let Some(worker) = self.tile_worker.take() {
            worker.shutdown();
        }
    }
}


/// Does this tile's box overlap the camera's ground footprint?
///
/// The footprint is a convex quad — a rectangle untilted, a trapezium tilted —
/// so the separating-axis test over both shapes' edge normals is exact, and
/// cheap enough to run per candidate tile during the quadtree walk. Testing the
/// footprint's *bounding box* instead would pull in the two large wedges either
/// side of the trapezium, which at a 45 degree tilt is most of the tiles.
fn tile_meets_footprint(b: &crate::tiles::TileBounds, quad: &[glam::Vec2; 4]) -> bool {
    let rect = [
        glam::Vec2::new(b.min_x as f32, b.min_y as f32),
        glam::Vec2::new(b.max_x as f32, b.min_y as f32),
        glam::Vec2::new(b.max_x as f32, b.max_y as f32),
        glam::Vec2::new(b.min_x as f32, b.max_y as f32),
    ];
    let axes = |p: &[glam::Vec2; 4]| {
        let mut out = [glam::Vec2::ZERO; 4];
        for i in 0..4 {
            let e = p[(i + 1) % 4] - p[i];
            out[i] = glam::Vec2::new(-e.y, e.x);
        }
        out
    };
    for axis in axes(&rect).into_iter().chain(axes(quad)) {
        if axis.length_squared() < 1e-12 {
            continue;
        }
        let proj = |p: &[glam::Vec2; 4]| {
            let mut lo = f32::MAX;
            let mut hi = f32::MIN;
            for v in p {
                let d = axis.dot(*v);
                lo = lo.min(d);
                hi = hi.max(d);
            }
            (lo, hi)
        };
        let (a0, a1) = proj(&rect);
        let (b0, b1) = proj(quad);
        if a1 < b0 || b1 < a0 {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod footprint_tests {
    use super::*;
    use crate::tiles::TileBounds;

    fn quad(pts: [(f32, f32); 4]) -> [glam::Vec2; 4] {
        pts.map(|(x, y)| glam::Vec2::new(x, y))
    }

    #[test]
    fn overlapping_and_disjoint_boxes() {
        let f = quad([(0.0, 0.0), (100.0, 0.0), (100.0, 100.0), (0.0, 100.0)]);
        assert!(tile_meets_footprint(&TileBounds::new(50.0, 150.0, 50.0, 150.0), &f));
        assert!(tile_meets_footprint(&TileBounds::new(-50.0, 500.0, -50.0, 500.0), &f));
        assert!(!tile_meets_footprint(&TileBounds::new(200.0, 300.0, 0.0, 100.0), &f));
    }

    #[test]
    fn corner_outside_a_trapezium_is_rejected() {
        // The tilted footprint: narrow near edge, wide far edge. A tile off to
        // the side of the near edge is inside the bounding box but outside the
        // trapezium, and is exactly what the axis test has to exclude.
        let f = quad([(40.0, 0.0), (60.0, 0.0), (200.0, 200.0), (-100.0, 200.0)]);
        assert!(!tile_meets_footprint(&TileBounds::new(-90.0, -70.0, 0.0, 20.0), &f));
        assert!(tile_meets_footprint(&TileBounds::new(-90.0, -70.0, 180.0, 200.0), &f));
    }
}

#[cfg(test)]
mod fallback_tests {
    use super::*;

    /// A zoom change must not blank the map: until the new level is built, each
    /// missing tile is covered by the nearest coarser one that exists.
    #[test]
    fn nearest_ancestor_walks_up_the_pyramid() {
        let tile = TileId { z: 15, x: 17514, y: 10264 };
        let parent = TileId { z: 14, x: 8757, y: 5132 };
        let grandparent = TileId { z: 13, x: 4378, y: 2566 };

        assert_eq!(nearest_resident_ancestor(tile, |t| t == parent), Some(parent));
        // The parent is missing too, so it keeps climbing.
        assert_eq!(
            nearest_resident_ancestor(tile, |t| t == grandparent),
            Some(grandparent)
        );
        // Nothing cached anywhere above it.
        assert_eq!(nearest_resident_ancestor(tile, |_| false), None);
        // Four levels is the limit; a 5x-removed ancestor is too coarse to use.
        let far = TileId { z: 10, x: 547, y: 320 };
        assert_eq!(nearest_resident_ancestor(tile, |t| t == far), None);
        // It never offers the tile itself — the caller has already established
        // that one is missing.
        assert_eq!(nearest_resident_ancestor(tile, |t| t == tile), None);
    }
}
