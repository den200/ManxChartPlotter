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

/// Camera uniforms for one draw origin (96 bytes).
///
/// The uniform buffer holds an array of these, one per *slot*, each at a
/// `min_uniform_buffer_offset_alignment` stride, bound with a dynamic offset
/// (wgpu ref notes §3, "Dynamic Bindings vs. Separate Bindings"). Every tile
/// is drawn with its own slot, whose `view_proj` maps positions relative to
/// the tile's centre — see `Camera::view_projection_relative` for why.
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct Uniforms {
    /// View-projection for positions relative to this slot's origin.
    pub view_proj: [[f32; 4]; 4],
    /// View size in pixels [width, height]
    pub view_size: [f32; 2],
    /// Pixels per meter at current zoom
    pub pixels_per_meter: f32,
    /// Physical pixels per logical point (the display's scale factor).
    /// Symbols and area patterns are sized in pixels, so without it they
    /// were twice their S-52 size on a standard-density screen relative to
    /// the lines and text, which already scale with the display.
    pub px_per_point: f32,
    /// This slot's origin minus the frame's pattern anchor (the camera), in
    /// metres. Area patterns are anchored to the world grid through it.
    pub anchor_offset: [f32; 2],
    pub _pad: [f32; 2],
}

/// Camera slots in the uniform buffer. The first few are fixed; tiles follow.
///
/// 2048 slots at a 256-byte stride is 512 KiB — small next to one tile's
/// vertices, and several times the largest draw list a tilted view makes.
pub const CAMERA_SLOTS: usize = 2048;
/// Slot for data still in global Mercator (the single-chart debug path).
pub const SLOT_GLOBAL: usize = 0;
/// Slot for the globally decluttered soundings and labels.
pub const SLOT_TEXT: usize = 1;
/// Slot for own ship and AIS targets.
pub const SLOT_MARINER: usize = 2;
/// First tile slot; entry `i` of the tile draw list uses `SLOT_FIRST_TILE + i`.
pub const SLOT_FIRST_TILE: usize = 3;

/// Bind group layout entry for the camera slot: a uniform bound with a
/// dynamic offset, one `Uniforms` wide.
pub fn camera_layout_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: true,
            min_binding_size: std::num::NonZeroU64::new(std::mem::size_of::<Uniforms>() as u64),
        },
        count: None,
    }
}

/// The binding for [`camera_layout_entry`]: one slot's worth of the buffer,
/// moved along it by the dynamic offset.
pub fn camera_binding(buffer: &wgpu::Buffer) -> wgpu::BindingResource<'_> {
    wgpu::BindingResource::Buffer(wgpu::BufferBinding {
        buffer,
        offset: 0,
        size: std::num::NonZeroU64::new(std::mem::size_of::<Uniforms>() as u64),
    })
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

/// Per-style uniforms for the line shader (48 bytes).
///
/// The camera comes from the per-draw camera slot, so nothing here changes
/// when the view moves.
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct LineUniforms {
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
    /// Display priority for depth sorting
    pub disp_prio: f32,
    /// Dot on length in screen pixels (>0 for DASD dash-dot pattern)
    pub dot_on_px: f32,
    /// LC() symbol repeat distance in pixels; 0 for an ordinary line. When
    /// set, the stroke is a ribbon `line_width_px` wide and the symbol's
    /// segments are drawn in it (see `render::lc_pattern`).
    pub lc_advance_px: f32,
    /// First segment of the LC() symbol in the segment buffer.
    pub lc_first: u32,
    /// Number of segments of the LC() symbol.
    pub lc_count: u32,
    /// How far before / after its place on the line the LC() symbol reaches,
    /// pixels.
    pub lc_x_range_px: [f32; 2],
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
/// What a route network job (Signal K resources) reports back.
pub(crate) enum RouteNetEvent {
    Status(String),
    Fetched(Vec<(crate::nav::Route, Vec<crate::nav::model::Waypoint>)>),
    /// A weather-routing job finished.
    WxPlanned {
        route: crate::nav::Route,
        waypoints: Vec<crate::nav::model::Waypoint>,
        summary: String,
    },
    WxFailed(String),
    WxProgress(String),
    /// An ORC certificate search finished.
    OrcResults(Vec<crate::nav::orc::OrcHit>),
    OrcFailed(String),
    OrcProgress(String),
    /// A wind field for the overlay arrived.
    WindLoaded(Box<crate::nav::grib::GribForecast>),
    WindFailed(String),
    WindProgress(String),
    /// A surface-current field for the overlay arrived.
    CurrentLoaded(Box<crate::nav::currents::CurrentForecast>),
    CurrentFailed(String),
    /// The point forecast behind the weather sheet arrived.
    PointLoaded(Box<crate::nav::pointfx::PointForecast>),
    PointFailed(String),
    /// A worker thread has ended, however it ended. Sent by a drop guard, so
    /// it arrives even if the worker panicked — which is what keeps a busy
    /// flag from sticking on for the rest of the session.
    JobDone,
}

/// Sends [`RouteNetEvent::JobDone`] when a worker thread ends, on every path
/// including a panic unwind.
struct JobGuard(std::sync::mpsc::Sender<RouteNetEvent>);

impl Drop for JobGuard {
    fn drop(&mut self) {
        let _ = self.0.send(RouteNetEvent::JobDone);
    }
}

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
    /// Camera slots (see [`Uniforms`]), bound with a dynamic offset.
    pub uniform_buffer: wgpu::Buffer,
    /// Stride between camera slots in `uniform_buffer`.
    camera_slot_align: u32,
    /// World origin the global text buffers (soundings, labels) are relative to.
    text_origin: glam::DVec2,
    /// World origin the mariner symbol instances are relative to.
    mariner_origin: glam::DVec2,
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
    /// A chart folder loading on a worker thread.
    chart_load: Option<ChartLoad>,
    /// Answers from the free-chart (NOAA) workers.
    free_rx: std::sync::mpsc::Receiver<FreeEvent>,
    free_tx: std::sync::mpsc::Sender<FreeEvent>,
    /// Set to stop the NOAA download in progress.
    free_cancel: Arc<std::sync::atomic::AtomicBool>,
    /// NOAA workers still running.
    free_jobs: usize,
    /// The colour table in use, so a re-apply does not re-upload it.
    palette_name: Option<String>,
    /// Bumped with every change of mariner settings; tiles built under an
    /// older one are discarded on arrival.
    settings_generation: u64,
    /// What the frame is cleared to: deep water in the active palette, so
    /// the sea beyond the charts (and a tile still loading) dims with the
    /// rest of the chart at night instead of flashing day blue.
    clear_colour: Color,
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
    /// The user's waypoints and routes, loaded on first use.
    route_store: Option<crate::nav::RouteStore>,
    /// Active-route following state, when a route is activated.
    route_follow: Option<crate::nav::Following>,
    /// Results coming back from route network jobs (Signal K PUT/GET).
    route_net_rx: Option<std::sync::mpsc::Receiver<RouteNetEvent>>,
    route_net_tx: Option<std::sync::mpsc::Sender<RouteNetEvent>>,
    /// S-52 mariner symbols — own ship and AIS — rebuilt whenever the boats
    /// move, which is why they need an instance buffer of their own rather
    /// than sharing the tiles'.
    mariner_buffer: Option<wgpu::Buffer>,
    mariner_count: u32,
    mariner_symbols: Option<crate::render::mariner::MarinerSymbols>,
    /// Where chart sets live — the directory navcore was pointed at, which is
    /// also where a downloaded set is unpacked so it sits beside the others.
    chart_root: Option<std::path::PathBuf>,
    /// The NAVCORE_PLAN capture hook has run (it must fire exactly once).
    env_plan_fired: bool,
    /// The address the user configured, while `NAVCORE_SIGNALK` overrides it
    /// for this run. Written back on save so a test address cannot become
    /// the boat's.
    signalk_override_saved: Option<String>,
    /// The wind field the overlay draws, when one is loaded.
    wind_forecast: Option<crate::nav::grib::GribForecast>,
    /// The surface current the overlay draws, likewise. Fetched separately
    /// from the wind — it is a different service and a much smaller answer,
    /// and waiting for the GRIB to arrive before showing the tide's set would
    /// be the tail wagging the dog.
    current_forecast: Option<crate::nav::currents::CurrentForecast>,
    /// Worker threads reporting to the route-network channel that have not
    /// yet ended. The event loop keeps polling while any are outstanding —
    /// nothing else would wake it to collect their results.
    route_net_jobs: usize,
    /// The world position the bubble is pinned to, so it tracks the object as
    /// the chart pans rather than sitting still on the glass.
    pick_anchor: Option<[f64; 2]>,
    pick_objects: Vec<crate::pick::PickedObject>,
    /// The query the open bubble is answering.
    pick_shown: u64,
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
    /// Light sector arcs and legs, sized on screen.
    sector_renderer: super::sectors::SectorRenderer,
    /// LC() lines: instanced segments, drawn by the line shader's LC entry
    /// points with the line pipelines' bind groups.
    lc_pipeline: wgpu::RenderPipeline,
    /// LC() symbol segments in pixels, bound to the line shader.
    lc_segment_buffer: wgpu::Buffer,
    /// Where each LC() symbol sits in `lc_segment_buffer`, and its size.
    lc_symbols: Vec<super::lc_pattern::LcSymbolGpu>,
    /// The density `lc_segment_buffer` was laid out at.
    lc_atlas_ppmm: f32,
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

/// A lattice of sample points that belongs to the sea rather than to the
/// screen.
///
/// The weather overlays used to step across the window from its own corner and
/// sample whatever water happened to lie under each point. That pins the
/// pattern to the glass: pan, and the chart slides beneath a field that does
/// not move; zoom, and it stays exactly as coarse as it was. A wind field has
/// to belong to the water, so the lattice is laid out in Mercator and then
/// projected, and every barb keeps its place on the sea as the chart moves.
///
/// The spacing is quantised to octaves of world distance. Derived straight
/// from the zoom it would be re-cut on every wheel click and the whole field
/// would crawl; rounded to the nearest power of two it holds still through a
/// factor of two of zoom and then doubles cleanly.
struct SampleGrid {
    /// Where each point landed on screen, logical points, row-major.
    screen: Vec<[f32; 2]>,
    /// Where each point is on the earth, in the same order.
    geo: Vec<(f64, f64)>,
    cols: usize,
    rows: usize,
}

impl SampleGrid {
    fn is_empty(&self) -> bool {
        self.cols == 0 || self.rows == 0
    }
}

impl RenderState {
    /// Lay a world-anchored lattice over `area`, about `spacing` points apart.
    fn sample_grid(&self, spacing: f32, area: egui::Rect) -> SampleGrid {
        sample_grid_for(&self.camera, self.scale_factor, spacing, area)
    }
}

/// The lattice, as arithmetic on a camera alone.
///
/// Split out from the renderer so it can be tested without a GPU — and the
/// thing worth testing is precisely the bug this replaced: pan the camera and
/// every sample must travel with the chart.
///
/// `area` is in logical points. The lattice is generous by one cell on every
/// side so a wash reaches the edges of the chart rather than stopping short of
/// them, and so a barb whose feathers cross the edge is still drawn.
fn sample_grid_for(
    camera: &Camera,
    scale_factor: f32,
    spacing: f32,
    area: egui::Rect,
) -> SampleGrid {
    {
        let empty = SampleGrid {
            screen: Vec::new(),
            geo: Vec::new(),
            cols: 0,
            rows: 0,
        };
        let ppp = scale_factor.max(0.01);
        if area.width() <= 0.0 || area.height() <= 0.0 {
            return empty;
        }
        // World units per logical point, and from that the world distance the
        // caller asked for, rounded to the nearest octave.
        let per_point = camera.zoom * ppp;
        let wanted = spacing * per_point;
        if !wanted.is_finite() || wanted <= 0.0 {
            return empty;
        }
        let step = 2f32.powf(wanted.log2().round());
        if !step.is_finite() || step <= 0.0 {
            return empty;
        }

        // The world rectangle the visible area covers. Taken from the four
        // corners because a tilted camera does not map a screen rectangle to a
        // world one, and the bounding box of the corners is the honest
        // conservative answer.
        let (mut lo_x, mut lo_y) = (f64::INFINITY, f64::INFINITY);
        let (mut hi_x, mut hi_y) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        for (x, y) in [
            (area.left(), area.top()),
            (area.right(), area.top()),
            (area.left(), area.bottom()),
            (area.right(), area.bottom()),
        ] {
            let w = camera.screen_to_world(x * ppp, y * ppp);
            lo_x = lo_x.min(w.x);
            hi_x = hi_x.max(w.x);
            lo_y = lo_y.min(w.y);
            hi_y = hi_y.max(w.y);
        }
        if !(lo_x.is_finite() && lo_y.is_finite() && hi_x.is_finite() && hi_y.is_finite()) {
            return empty;
        }

        let step = step as f64;
        let i0 = (lo_x / step).floor() as i64 - 1;
        let i1 = (hi_x / step).ceil() as i64 + 1;
        let j0 = (lo_y / step).floor() as i64 - 1;
        let j1 = (hi_y / step).ceil() as i64 + 1;
        let cols = (i1 - i0 + 1).max(0) as usize;
        let rows = (j1 - j0 + 1).max(0) as usize;
        // A camera looking at the whole earth through a tilted lens can ask
        // for a lattice with no end to it. Refusing is better than freezing.
        const MOST: usize = 20_000;
        if cols < 2 || rows < 2 || cols.saturating_mul(rows) > MOST {
            return empty;
        }

        let mut screen = Vec::with_capacity(cols * rows);
        let mut geo = Vec::with_capacity(cols * rows);
        // Rows run north to south so the lattice comes out in reading order on
        // screen, which is what the fill's mesh expects.
        for j in (j0..=j1).rev() {
            for i in i0..=i1 {
                let (wx, wy) = (i as f64 * step, j as f64 * step);
                let s = camera.world_to_screen(wx, wy);
                screen.push([s.x / ppp, s.y / ppp]);
                let (lat, lon) = crate::render::projection::Projection::to_wgs84(wx, wy);
                geo.push((lat, lon));
            }
        }
        SampleGrid {
            screen,
            geo,
            cols,
            rows,
        }
    }
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
        log::debug!(
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

        log::debug!("Surface format: {:?}", surface_format);
        log::debug!("Available formats: {:?}", surface_caps.formats);

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

        let (free_tx, free_rx) = std::sync::mpsc::channel();

        // Camera slots: one `Uniforms` per draw origin, at the device's
        // dynamic-offset alignment. Written every frame by write_camera_slots.
        let camera_slot_align = device
            .limits()
            .min_uniform_buffer_offset_alignment
            .max(std::mem::size_of::<Uniforms>() as u32);
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Camera Slot Buffer"),
            size: camera_slot_align as u64 * CAMERA_SLOTS as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
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
                    camera_layout_entry(0),
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
                    resource: camera_binding(&uniform_buffer),
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
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: std::num::NonZeroU64::new(std::mem::size_of::<LineUniforms>() as u64),
                    },
                    count: None,
                },
                // LC() symbol segments, in pixels (render::lc_pattern::LcAtlas)
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

        // The LC symbols at this display's density. The segment count does not
        // depend on the density, so a density change rewrites it in place.
        let lc_atlas_ppmm = effective_ppmm.max(1.0);
        let lc_atlas = super::lc_pattern::LcAtlas::shared(lc_atlas_ppmm);
        let lc_segment_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("LC Segment Buffer"),
            contents: bytemuck::cast_slice(&lc_atlas.segments),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
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
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &line_uniform_buffer,
                        offset: 0,
                        size: std::num::NonZeroU64::new(std::mem::size_of::<LineUniforms>() as u64),
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: lc_segment_buffer.as_entire_binding(),
                },
            ],
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

        let sector_renderer =
            super::sectors::SectorRenderer::new(&device, config.format, &uniform_bind_group_layout);

        // LC() lines: each segment an instanced quad (see render::lc_pattern).
        let lc_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("LC Line Pipeline"),
            layout: Some(&line_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &line_shader,
                entry_point: "vs_lc",
                buffers: &[super::lc_pattern::LcSegment::desc()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &line_shader,
                entry_point: "fs_lc",
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
            // As the line pipelines: ordered on the CPU, no depth test.
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

        // Symbol renderer (optional - may fail if assets not found)
        let (symbol_renderer, symbol_camera_bind_group) = match SymbolRenderer::new(
            &device,
            &queue,
            surface_format,
            &uniform_bind_group_layout,
        ) {
            Ok(renderer) => {
                let bind_group = renderer.create_camera_bind_group(&device, &uniform_buffer);
                log::debug!("Symbol renderer initialized");
                (Some(renderer), Some(bind_group))
            }
            Err(e) => {
                log::warn!("Failed to initialize symbol renderer: {}", e);
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
                        resource: camera_binding(&uniform_buffer),
                    }],
                });
                log::debug!("Pattern renderer initialized");
                (Some(renderer), Some(bind_group))
            }
            Err(e) => {
                log::warn!("Failed to initialize pattern renderer: {}", e);
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
                log::debug!("Text renderer initialized");
                (Some(renderer), Some(bind_group))
            }
            Err(e) => {
                log::warn!("Failed to initialize text renderer: {}", e);
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
                log::debug!("Label renderer initialized");
                (Some(renderer), Some(bind_group))
            }
            Err(e) => {
                log::warn!("Failed to initialize label renderer: {}", e);
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
            camera_slot_align,
            text_origin: glam::DVec2::ZERO,
            mariner_origin: glam::DVec2::ZERO,
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
            clear_colour: OCEAN_BACKGROUND,
            chart_load: None,
            free_rx,
            free_tx,
            free_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            free_jobs: 0,
            palette_name: None,
            settings_generation: 0,
            pattern_camera_bind_group,
            text_renderer,
            text_camera_bind_group,
            label_renderer,
            ui: None,
            shop: None,
            signalk: None,
            fleet: Default::default(),
            following: false,
            route_store: None,
            route_follow: None,
            route_net_rx: None,
            route_net_tx: None,
            mariner_buffer: None,
            mariner_count: 0,
            mariner_symbols: crate::render::mariner::MarinerSymbols::resolve(),
            chart_root: None,
            env_plan_fired: false,
            signalk_override_saved: None,
            wind_forecast: None,
            current_forecast: None,
            route_net_jobs: 0,
            pick_anchor: None,
            pick_objects: Vec::new(),
            pick_shown: 0,
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
            sector_renderer,
            lc_pipeline,
            lc_segment_buffer,
            lc_symbols: lc_atlas.symbols,
            lc_atlas_ppmm,
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
        log::debug!("Area feature type breakdown:");
        let mut sorted_types: Vec<_> = area_type_counts.iter().collect();
        sorted_types.sort_by_key(|(code, _)| *code);
        for (type_code, count) in sorted_types {
            log::debug!("  type_code={}: {} features", type_code, count);
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
        log::debug!("Actually rendered area types: {:?}", rendered_area_types);
        log::debug!("Primitive stats - plain: {}, strip: {}, fan: {} triangles",
            total_plain_triangles, total_strip_triangles, total_fan_triangles);
        if total_skinny_triangles > 0 || total_degenerate_triangles > 0 {
            log::debug!("Triangle quality - skinny: {}, degenerate: {}",
                total_skinny_triangles, total_degenerate_triangles);
        }

        // Debug: Check which recognized features have geometry
        let land_with_geom = chart.land_areas().filter(|f| f.area_geometry.is_some()).count();
        let depth_with_geom = chart.depth_areas().filter(|f| f.area_geometry.is_some()).count();
        log::debug!("Land areas with geometry: {}, Depth areas with geometry: {}", land_with_geom, depth_with_geom);

        let area_vertex_count = vertices.len();

        // Debug: show ALL line feature type codes
        let mut line_type_counts: HashMap<u16, usize> = HashMap::new();
        for feature in chart.lines() {
            *line_type_counts.entry(feature.type_code).or_insert(0) += 1;
        }
        log::debug!("Line feature type breakdown:");
        let mut sorted_line_types: Vec<_> = line_type_counts.iter().collect();
        sorted_line_types.sort_by_key(|(code, _)| *code);
        for (type_code, count) in sorted_line_types {
            log::debug!("  type_code={}: {} features", type_code, count);
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

        log::debug!("Lines: coastlines={} polylines, contours={} polylines",
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
                log::debug!("Coastline batch: {} vertices, {} indices", coastline_verts.len(), coastline_indices.len());
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
                log::debug!("Contour batch: {} vertices, {} indices", contour_verts.len(), contour_indices.len());
            }
        }

        // Total line index count for logging
        let total_line_indices: u32 = self.line_batches.iter().map(|b| b.index_count).sum();
        log::debug!("Loaded {} area vertices, {} line indices in {} batches",
            area_vertex_count, total_line_indices, self.line_batches.len());

        log::debug!("Loaded {} total area vertices from chart", vertices.len());

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
            log::debug!("No geometry to render");
            return;
        }

        log::debug!("Combined bounds: X=[{:.0}, {:.0}], Y=[{:.0}, {:.0}]", min_x, max_x, min_y, max_y);
        if !vertices.is_empty() {
            log::debug!("First 3 area vertices: {:?}", &vertices[..3.min(vertices.len())]);
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
        self.camera = Camera::new(center_x as f64, center_y as f64, zoom, screen_w, screen_h);
        log::debug!("Camera at ({:.0}, {:.0}), zoom: {:.2} m/px (SM bounds: {:.0}x{:.0})", center_x, center_y, zoom, width, height);

        // Log view_proj matrix to verify it's sensible
        let vp = self.camera.view_projection_matrix();
        log::debug!("View-proj matrix:\n  {:?}\n  {:?}\n  {:?}\n  {:?}",
            vp.row(0), vp.row(1), vp.row(2), vp.row(3));

        // Update symbol instances
        if let Some(ref mut symbol_renderer) = self.symbol_renderer {
            let ref_lat = chart.header.ref_lat;
            let ref_lon = chart.header.ref_lon;
            let instances = features_to_instances(&chart.features, ref_lat, ref_lon);
            log::debug!("Created {} symbol instances", instances.len());
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
        log::debug!("Loading test triangle to verify pipeline...");

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
        log::debug!("Test triangle view_proj matrix:");
        log::debug!("  {:?}", vp.row(0));
        log::debug!("  {:?}", vp.row(1));
        log::debug!("  {:?}", vp.row(2));
        log::debug!("  {:?}", vp.row(3));
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
                    resource: camera_binding(&self.uniform_buffer),
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
        self.palette_name = Some(palette_name.to_string());
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
                if let Some(c) = engine.get_color("DEPDW") {
                    self.clear_colour = c;
                }
                if let Some(ref patterns) = self.pattern_renderer {
                    let tables = &engine.tables;
                    let colour_of = |token: &str| {
                        tables
                            .get_color_f32(token)
                            .map(|c| [0, 1, 2].map(|i| (c[i].clamp(0.0, 1.0) * 255.0).round() as u8))
                    };
                    let day = matches!(palette_name, "DAY" | "DAY_BRIGHT");
                    patterns.switch_palette(
                        &self.queue,
                        (!day).then_some(&colour_of as &dyn Fn(&str) -> Option<[u8; 3]>),
                    );
                }
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
    /// Remember where chart sets live, so a download lands beside them —
    /// and so the next session opens the same charts without an argument.
    pub fn set_chart_root(&mut self, root: std::path::PathBuf) {
        if let Some(ref mut ui) = self.ui {
            ui.charts.chosen = root.display().to_string();
            // What is installed depends on which folder is in use.
            for row in ui.free.rows.iter_mut() {
                row.installed = crate::shop::noaa::installed(&root, row.code);
            }
        }
        self.chart_root = Some(root);
    }

    /// Load a folder of charts now, replacing whatever is on screen.
    ///
    /// The startup path in `main` does the same three steps but is allowed to
    /// take `Arc::get_mut` on a key store nothing else holds yet. By the time
    /// the menu can call this, that store is shared with the tile worker, so
    /// the keys are built fresh here rather than mutated in place — the
    /// alternative is a panic the first time somebody changes folder.
    ///
    /// Returns how many cells the catalogue found, or a sentence fit for the
    /// window: a folder of holiday photographs is a thing a user will pick by
    /// accident, and it must say so rather than empty the chart.
    /// Start loading a folder of charts on a worker thread.
    ///
    /// Building the catalogue decrypts every cell's header, which takes the
    /// better part of twenty seconds on a real set — done on the UI thread,
    /// the window froze with no spinner and no way to tell it was working.
    /// `keep_view` holds the camera where it is (a reload after an install);
    /// otherwise the view frames the new charts.
    pub fn start_chart_folder_load(&mut self, dir: std::path::PathBuf, keep_view: bool) {
        if let Some(ref mut ui) = self.ui {
            ui.charts.loading = true;
            ui.charts.status = "Loading charts…".into();
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_dir = dir.clone();
        std::thread::spawn(move || {
            let _ = tx.send(load_chart_folder(&worker_dir));
        });
        self.chart_load = Some(ChartLoad { dir, keep_view, rx });
        self.needs_redraw = true;
    }

    /// Take a finished chart load, if one has arrived.
    fn poll_chart_load(&mut self) {
        let Some(ref load) = self.chart_load else { return };
        let result = match load.rx.try_recv() {
            Ok(r) => r,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err("the chart loader stopped unexpectedly".into())
            }
        };
        let ChartLoad { dir, keep_view, .. } = self.chart_load.take().unwrap();
        match result {
            Ok((catalog, keys, decryptor)) => {
                let found = catalog.charts.len();
                log::info!("chart folder {}: {found} cells", dir.display());
                let view = keep_view.then(|| self.camera.clone());
                self.set_chart_root(dir.clone());
                self.load_catalog(catalog, keys, decryptor);
                if let Some(camera) = view {
                    self.camera = camera;
                }
                if let Some(ref mut ui) = self.ui {
                    ui.charts.loading = false;
                    ui.charts.chosen = dir.display().to_string();
                    ui.charts.status = format!("{found} cells loaded");
                }
                self.refresh_installed();
                // Only a folder that actually loaded is worth coming back to
                // next time.
                self.save_settings();
            }
            Err(e) => {
                if let Some(ref mut ui) = self.ui {
                    ui.charts.loading = false;
                    ui.charts.status = e;
                }
            }
        }
        self.needs_redraw = true;
    }

    /// Where free charts go: the chart folder in use, or — before there is
    /// one — navcore's own, which then becomes the chart folder.
    fn free_root(&self) -> std::path::PathBuf {
        self.chart_root.clone().unwrap_or_else(|| {
            dirs::data_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("navcore")
                .join("charts")
        })
    }

    /// List the packages with what is installed, and ask NOAA for sizes
    /// and dates in the background.
    fn free_refresh(&mut self) {
        let root = self.free_root();
        if let Some(ref mut ui) = self.ui {
            ui.free.probed = true;
            ui.free.rows = crate::shop::noaa::REGIONS
                .iter()
                .map(|r| crate::render::ui::FreeRow {
                    code: r.code,
                    name: r.name,
                    remote: None,
                    installed: crate::shop::noaa::installed(&root, r.code),
                })
                .collect();
        }
        let tx = self.free_tx.clone();
        self.free_jobs += 1;
        std::thread::spawn(move || {
            for r in crate::shop::noaa::REGIONS {
                let answer = crate::shop::noaa::probe(r.code);
                if tx.send(FreeEvent::Probed(r.code, answer)).is_err() {
                    return;
                }
            }
            let _ = tx.send(FreeEvent::Done);
        });
    }

    fn free_download(&mut self, code: String) {
        if self.ui.as_ref().is_some_and(|u| u.free.active.is_some()) {
            return;
        }
        let root = self.free_root();
        let name = crate::shop::noaa::REGIONS
            .iter()
            .find(|r| r.code == code)
            .map(|r| r.name)
            .unwrap_or("charts");
        if let Some(ref mut ui) = self.ui {
            ui.free.active = Some((code.clone(), 0, 0));
            ui.free.status = format!("Downloading {name}…");
        }
        self.free_cancel.store(false, std::sync::atomic::Ordering::Relaxed);
        let cancel = Arc::clone(&self.free_cancel);
        let tx = self.free_tx.clone();
        self.free_jobs += 1;
        std::thread::spawn(move || {
            let mut last = std::time::Instant::now();
            let progress_tx = tx.clone();
            let progress_code = code.clone();
            let result = crate::shop::noaa::install(
                &root,
                &code,
                |done, total| {
                    // Four updates a second is plenty for a progress bar.
                    if last.elapsed() >= std::time::Duration::from_millis(250) || done == total {
                        last = std::time::Instant::now();
                        let _ = progress_tx.send(FreeEvent::Progress(progress_code.clone(), done, total));
                    }
                },
                &cancel,
            );
            let _ = tx.send(match result {
                Ok(inst) => FreeEvent::Installed(code, root, inst),
                Err(e) => FreeEvent::Failed(code, e),
            });
            let _ = tx.send(FreeEvent::Done);
        });
    }

    fn free_remove(&mut self, code: &str) {
        let root = self.free_root();
        let result = crate::shop::noaa::remove(&root, code);
        if let Some(ref mut ui) = self.ui {
            match result {
                Ok(()) => {
                    if let Some(row) = ui.free.rows.iter_mut().find(|r| r.code == code) {
                        row.installed = None;
                    }
                    ui.free.status = "Removed.".into();
                }
                Err(e) => ui.free.status = format!("Could not remove: {e}"),
            }
        }
        // Take the removed cells off the map.
        if self.chart_root.is_some() && self.chart_load.is_none() {
            self.start_chart_folder_load(root, true);
        }
    }

    /// Fold in what the NOAA workers have to say.
    fn poll_free_charts(&mut self) {
        let events: Vec<FreeEvent> = self.free_rx.try_iter().collect();
        if events.is_empty() {
            return;
        }
        let mut reload: Option<std::path::PathBuf> = None;
        for event in events {
            let Some(ref mut ui) = self.ui else { continue };
            match event {
                FreeEvent::Probed(code, answer) => {
                    if let Some(row) = ui.free.rows.iter_mut().find(|r| r.code == code) {
                        row.remote = answer.ok();
                    }
                }
                FreeEvent::Progress(code, done, total) => {
                    ui.free.active = Some((code, done, total));
                }
                FreeEvent::Installed(code, root, inst) => {
                    let name = ui.free.rows.iter().find(|r| r.code == code).map(|r| r.name).unwrap_or("");
                    ui.free.status = format!("{name} installed — loading the charts…");
                    if let Some(row) = ui.free.rows.iter_mut().find(|r| r.code == code) {
                        row.installed = Some(inst);
                    }
                    ui.free.active = None;
                    reload = Some(root);
                }
                FreeEvent::Failed(_, e) => {
                    ui.free.active = None;
                    ui.free.status = if e == "cancelled" {
                        "Download cancelled.".into()
                    } else {
                        format!("Download failed: {e}")
                    };
                }
                FreeEvent::Done => self.free_jobs = self.free_jobs.saturating_sub(1),
            }
        }
        if let Some(root) = reload {
            // Keep the view where it is when adding to charts already on
            // screen; frame the new ones when they are the first.
            let keep_view = self.chart_root.is_some();
            if self.chart_load.is_none() {
                self.start_chart_folder_load(root, keep_view);
            }
        }
        self.needs_redraw = true;
    }

    /// Whether charts are being loaded in the background.
    pub fn chart_load_pending(&self) -> bool {
        self.chart_load.is_some()
    }

    /// Re-key the shop's listing to what is on disk now. After an install
    /// the table must stop saying "Not installed" about what it just put there.
    fn refresh_installed(&mut self) {
        let root = self.chart_root.clone().unwrap_or_else(|| "charts".into());
        let Some(ref mut ui) = self.ui else { return };
        if ui.shop.charts.is_empty() {
            return;
        }
        ui.shop.installed = match_installed(&ui.shop.charts, &root);
    }

    /// The folder the last session was using, if it is still there.
    ///
    /// Read straight from the settings file rather than through the UI, because
    /// `main` needs it before there is a window to hold a UI at all.
    pub fn remembered_chart_folder() -> Option<std::path::PathBuf> {
        let path = Self::settings_path()?;
        let text = std::fs::read_to_string(path).ok()?;
        #[derive(serde::Deserialize)]
        struct Persisted {
            charts: crate::render::ui::ChartFolderView,
        }
        let p: Persisted = serde_json::from_str(&text).ok()?;
        let dir = std::path::PathBuf::from(p.charts.chosen.trim());
        // A folder on a memory stick that is no longer plugged in must not
        // stop the plotter starting.
        dir.is_dir().then_some(dir)
    }

    /// `NAVCORE_PLAN="from;to[;motor]"` types the passage planner's fields
    /// and presses the button, so the whole flow can be captured without a
    /// keyboard. An empty `from` means the boat. Called from both init_ui
    /// and set_chart_root because the plan needs the pair of them and their
    /// order is an accident of startup; fires once.
    fn maybe_env_plan(&mut self) {
        if self.env_plan_fired || self.ui.is_none() || self.chart_root.is_none() {
            return;
        }
        self.env_plan_fired = true;

        // `NAVCORE_WIND=1` opens the weather sheet and fetches for what is on
        // screen. Like the plan hook it has to wait for a view: at init_ui the
        // camera is still looking at 0°N 0°E, and the fetch would ask for the
        // wind over the Gulf of Guinea.
        if std::env::var("NAVCORE_WIND").is_ok() {
            if let Some(ref mut ui) = self.ui {
                ui.wind.show = true;
                ui.sheet.show = true;
            }
            self.spawn_wind_fetch();
            self.spawn_point_fetch();
        }

        let Ok(spec) = std::env::var("NAVCORE_PLAN") else { return };
        let parts: Vec<String> = spec.split(';').map(str::to_string).collect();
        if parts.len() >= 2 {
            let sail = parts.get(2).map(|m| m != "motor").unwrap_or(true);
            if let Some(ref mut ui) = self.ui {
                ui.plan.from = parts[0].clone();
                ui.plan.to = parts[1].clone();
            }
            self.plan_passage(&parts[0], &parts[1], sail);
        } else {
            log::warn!("NAVCORE_PLAN must be 'from;to[;motor]'; ignoring '{spec}'");
        }
    }

    pub fn load_catalog(
        &mut self,
        mut catalog: ChartCatalog,
        keys: Arc<KeyStore>,
        decryptor: CachedDecryptor,
    ) {
        // Camera position must be in GLOBAL MERCATOR meters
        // catalog.combined_extent is already in global Mercator (computed during catalog build)
        let extent = &catalog.combined_extent;
        let center_x = (extent.min_x + extent.max_x) / 2.0;
        let center_y = (extent.min_y + extent.max_y) / 2.0;
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
            let parts: Vec<f64> = view
                .split(',')
                .filter_map(|s| s.trim().parse::<f64>().ok())
                .collect();
            if parts.len() == 3 {
                let (mx, my) = crate::tiles::latlon_to_mercator(parts[0], parts[1]);
                self.camera = Camera::new(
                    mx, my, parts[2] as f32,
                    self.size.width as f32, self.size.height as f32,
                );
                log::debug!(
                    "NAVCORE_VIEW override: lat={} lon={} mpp={} -> Mercator ({:.0},{:.0})",
                    parts[0], parts[1], parts[2], mx, my
                );
            } else {
                log::warn!("NAVCORE_VIEW must be 'lat,lon,mpp'; ignoring '{}'", view);
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
                    log::debug!("NAVCORE_TILT: {:.1}°", self.camera.tilt.to_degrees());
                }
                Err(_) => log::warn!("NAVCORE_TILT must be a number of degrees; ignoring '{}'", t),
            }
        }

        // Only now does the camera look at the chart rather than at 0°N 0°E,
        // and a hook that asks "what is on screen?" — the wind fetch does —
        // must not be answered before that.
        self.maybe_env_plan();

        log::debug!("Catalog: {} charts, camera at ({:.0}, {:.0}) Mercator, zoom {:.0} m/px",
            catalog.charts.len(), center_x, center_y, zoom);

        // After the camera has framed the charts themselves: the basemap
        // widens the catalogue to the whole world.
        catalog.add_world_basemap();
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
                log::debug!("{}", engine.summary());
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
                log::warn!("Could not load S-52 engine: {}", e);
                log::warn!("Display category filtering will be disabled.");
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
                    log::warn!("NAVCORE_PALETTE: unknown value {:?}", other);
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
        // The saved Display choices — palette, safety depth from the draft —
        // now that there is an engine and a worker to give them to.
        self.settings_generation = 0;
        self.apply_display();
        self.pending_tiles.clear();
        self.deferred_tile_results.clear();
        log::debug!("Background tile worker spawned");

        // NAVCORE_PICK=lat,lon opens the info bubble at a position, so a
        // headless capture can show it. This is the second firing of the
        // hook (init_ui runs before the tile worker exists); when the pick
        // was aimed at a planner pin, that firing already consumed it.
        // Any hook that claims the tap has already consumed this pick during
        // init_ui; firing it again here would place a second waypoint or
        // refill a planner field.
        let pick_env = std::env::var("NAVCORE_PICK").ok().filter(|_| {
            std::env::var("NAVCORE_PLAN_PICK").is_err()
                && std::env::var("NAVCORE_ROUTE_NEW").is_err()
        });
        if let Some(spec) = pick_env {
            let parts: Vec<f64> = spec.split(',').filter_map(|v| v.trim().parse().ok()).collect();
            if parts.len() == 2 {
                let (mx, my) = crate::tiles::latlon_to_mercator(parts[0], parts[1]);
                self.pick_at_world(mx, my);
            } else {
                log::warn!("NAVCORE_PICK must be 'lat,lon'; ignoring {:?}", spec);
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
        let ppmm = 4.0 * scale_factor; // 96 DPI base = 4.0 ppmm
        let changed = ppmm != self.effective_ppmm;
        self.scale_factor = scale_factor;
        self.effective_ppmm = ppmm;
        // Tiles carry sizes baked at the old density — labels, light arcs,
        // line-symbol spacing, even which charts were chosen for the scale —
        // and the known-empty set was worked out with it too. Moving the
        // window to a display of another density starts them over.
        if changed && self.tile_mode {
            self.tile_cache.clear();
            self.pending_tiles.clear();
            self.deferred_tile_results.clear();
            self.needs_redraw = true;
        }
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
        // Style only: the camera reaches the line shader through the camera
        // slots, so a pan or zoom no longer rewrites every style.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        styles_key.hash(&mut hasher);
        self.effective_ppmm.to_bits().hash(&mut hasher);
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
                    if batch.vertex_count > 0 {
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
        self.poll_free_charts();
        self.poll_chart_load();
        self.poll_signalk();
        self.update_route_following();
        self.drain_route_net();
        self.update_mariner_symbols();

        // In tile mode, keep the camera near the loaded chart extent.
        // This prevents panning/gesture glitches from throwing the view outside the
        // valid Mercator range, which would yield empty tiles (ocean-only).
        if self.tile_mode {
            self.clamp_camera_to_catalog();
        }

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

        // Camera slots, now that the draw list and the text origin are known.
        self.write_camera_slots();

        // Pre-write line uniform styles to dynamic UBO BEFORE the render pass.
        // Each unique style gets its own aligned slot; draw calls reference by dynamic offset.
        let line_style_offsets: HashMap<LineStyleKey, u32> = if self.tile_mode {
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
            if self.lc_atlas_ppmm != self.line_style_ppmm() {
                let atlas = super::lc_pattern::LcAtlas::shared(self.line_style_ppmm());
                self.queue.write_buffer(
                    &self.lc_segment_buffer,
                    0,
                    bytemuck::cast_slice(&atlas.segments),
                );
                self.lc_symbols = atlas.symbols;
                self.lc_atlas_ppmm = self.line_style_ppmm();
            }
            if self.last_line_uniforms_key != line_uniforms_key {
                let uniform_start = profile_enabled().then(Instant::now);
                let tables = self.s52_engine.as_ref().map(|e| &e.tables);
                for (slot, style_key) in self.cached_visible_line_styles.iter().enumerate() {
                    let offset = (slot as u32) * align;
                    let style = style_for_key(style_key, self.line_style_ppmm(), tables);
                    let line_uniforms = LineUniforms {
                        line_width_px: style.width_px,
                        join_limit: 4.0,
                        color_index: style.color_index,
                        dash_on_px: style.dash_on_px,
                        dash_off_px: style.dash_off_px,
                        disp_prio: 0.0,
                        dot_on_px: style.dot_on_px,
                        lc_advance_px: 0.0,
                        lc_first: 0,
                        lc_count: 0,
                        lc_x_range_px: [0.0; 2],
                    };
                    // An LC() line: segments drawn as quads wide enough for its
                    // symbol, which the fragment shader repeats along them.
                    let line_uniforms = match style_key.lc {
                        Some(lc) => {
                            let sym = self.lc_symbols.get(lc as usize).copied().unwrap_or_default();
                            LineUniforms {
                                line_width_px: 2.0 * sym.half_extent_px,
                                dash_on_px: 0.0,
                                dash_off_px: 0.0,
                                dot_on_px: 0.0,
                                // Never zero: the shader divides by it.
                                lc_advance_px: sym.advance_px.max(1.0),
                                lc_first: sym.first,
                                lc_count: sym.count,
                                lc_x_range_px: sym.x_range_px,
                                ..line_uniforms
                            }
                        }
                        None => line_uniforms,
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
            let align = self.line_uniform_align;

            for (i, batch) in self.line_batches.iter().enumerate() {
                let offset = (i as u32) * align;
                let line_uniforms = LineUniforms {
                    line_width_px: batch.style.width_px,
                    join_limit: 4.0,
                    color_index: batch.style.color_index,
                    dash_on_px: batch.style.dash_on_px,
                    dash_off_px: batch.style.dash_off_px,
                    disp_prio: 4.0,
                    dot_on_px: batch.style.dot_on_px,
                    lc_advance_px: 0.0,
                    lc_first: 0,
                    lc_count: 0,
                    lc_x_range_px: [0.0; 2],
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
                            r: self.clear_colour[0] as f64,
                            g: self.clear_colour[1] as f64,
                            b: self.clear_colour[2] as f64,
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
                let cam = self.camera_slot_offset(SLOT_GLOBAL);
                render_pass.set_pipeline(&self.pipeline);
                render_pass.set_bind_group(0, &self.uniform_bind_group, &[cam]);

                if let Some(ref buffer) = self.vertex_buffer {
                    render_pass.set_vertex_buffer(0, buffer.slice(..));
                    render_pass.draw(0..self.vertex_count, 0..1);
                }

                // 2. Render lines (coastlines + depth contours) with shader-based pipeline
                render_pass.set_pipeline(&self.line_pipeline);
                render_pass.set_bind_group(0, &self.uniform_bind_group, &[cam]);
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
                        symbol_renderer.render_atlas_debug(&mut render_pass, symbol_bind_group, cam);
                    } else {
                        symbol_renderer.render(&mut render_pass, symbol_bind_group, cam);
                    }
                }

                // 4. Render text/soundings on top of everything
                if let (Some(ref text_renderer), Some(ref text_bind_group)) =
                    (&self.text_renderer, &self.text_camera_bind_group)
                {
                    text_renderer.render(&mut render_pass, text_bind_group, cam);
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
        // Projected before the take: routes_display reads ui.routes for the
        // visible set, and inside the block self.ui is temporarily None.
        // Projected before the take, like routes_display: inside the block
        // self.ui is temporarily None and the pins would silently vanish.
        let routes_display = self.routes_display();
        let plan_pins = self.plan_pins();
        let wind_barbs = self.wind_barbs();
        // Projected before the take, like the routes and the pins: inside the
        // block self.ui is None and both would silently vanish.
        let current_arrows = self.current_arrows();
        let wind_fill = self.wind_fill();
        // Either layer drawing something means the field reaches the view.
        let fill_drawn = wind_fill.kt.iter().filter(|k| k.is_some()).count();
        self.refresh_wind_coverage(wind_barbs.len() + fill_drawn);
        let weather_anchor = self.weather_anchor_screen();
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
                    pick_id: self.pick_shown,
                    own_ship,
                    ais,
                    mpp,
                    routes: routes_display,
                    plan_pins,
                    wind: wind_barbs,
                    wind_fill,
                    current: current_arrows,
                    weather_anchor,
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
                        if let Some(ref mut ui) = self.ui {
                            ui.instruments.active = true;
                        }
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
                        ui.instruments.active = false;
                        ui.instruments.status = "Not connected".into();
                        ui.instruments.available.clear();
                    }
                    self.needs_redraw = true;
                }
                crate::render::ui::UiAction::SettingsChanged => {
                    // The boat's draft feeds the chart's safety depth.
                    self.apply_display();
                    self.save_settings();
                }
                crate::render::ui::UiAction::FreeChartsRefresh => self.free_refresh(),
                crate::render::ui::UiAction::FreeChartsDownload { code } => self.free_download(code),
                crate::render::ui::UiAction::FreeChartsCancel => {
                    self.free_cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                crate::render::ui::UiAction::FreeChartsRemove { code } => self.free_remove(&code),
                crate::render::ui::UiAction::DisplayChanged => {
                    self.apply_display();
                    self.save_settings();
                }
                crate::render::ui::UiAction::RoutesOpen => {
                    self.ensure_route_store();
                    self.refresh_route_rows();
                    if let Some(ref mut ui) = self.ui {
                        ui.routes.open = !ui.routes.open;
                    }
                    if !self.ui.as_ref().is_some_and(|u| u.routes.open) {
                        self.finish_route_edit();
                    }
                }
                crate::render::ui::UiAction::RouteActivate { route_id } => {
                    self.activate_route(route_id);
                }
                crate::render::ui::UiAction::RouteDeactivate => {
                    self.deactivate_route();
                    if let Some(ref mut ui) = self.ui {
                        ui.routes.status = "Following stopped".into();
                    }
                }
                crate::render::ui::UiAction::RoutePublish { route_id } => {
                    self.ensure_route_store();
                    let base = self
                        .ui
                        .as_ref()
                        .and_then(|u| crate::signalk::resources::http_base(&u.instruments.url));
                    let job = self.route_store.as_ref().and_then(|s| {
                        s.route(route_id)
                            .map(|r| (r.clone(), s.waypoints.clone()))
                    });
                    match (base, job) {
                        (Some(base), Some((route, set))) => {
                            if let Some(ref mut ui) = self.ui {
                                ui.routes.net_busy = true;
                                ui.routes.status = format!("Publishing \"{}\"…", route.name);
                            }
                            let tx = self.route_net_sender();
                            std::thread::spawn(move || {
                                let _done = JobGuard(tx.clone());
                                let client =
                                    crate::signalk::resources::ResourcesClient::new(base);
                                let msg = match client.put_route(&route, &set) {
                                    Ok(()) => format!("Published \"{}\" to Signal K", route.name),
                                    Err(e) => format!("Signal K refused the route: {e}"),
                                };
                                let _ = tx.send(RouteNetEvent::Status(msg));
                            });
                        }
                        (None, _) => {
                            if let Some(ref mut ui) = self.ui {
                                ui.routes.status =
                                    "Set a Signal K server under Instruments first".into();
                            }
                        }
                        (_, None) => {}
                    }
                }
                crate::render::ui::UiAction::WeatherRoute { route_id } => {
                    self.ensure_route_store();
                    let endpoints = self.route_store.as_ref().and_then(|s| {
                        let r = s.route(route_id)?;
                        let a = s.waypoints.get(*r.waypoints.first()?)?.position;
                        let b = s.waypoints.get(*r.waypoints.last()?)?.position;
                        Some((r.name.clone(), a, b))
                    });
                    match endpoints {
                        Some((name, a, b)) => self.spawn_wx_plan(a, b, format!("{name} (wx)")),
                        None => {
                            if let Some(ref mut ui) = self.ui {
                                ui.weather.status = "that route has no endpoints".into();
                            }
                        }
                    }
                }
                crate::render::ui::UiAction::PlanRoute { from, to, sail } => {
                    self.plan_passage(&from, &to, sail);
                }
                crate::render::ui::UiAction::PlanCancel => {
                    // The workers notice at their next ring (or simply stay
                    // quiet when they finish); the UI is free at once.
                    crate::nav::isochrone::cancel_plans();
                    if let Some(ref mut ui) = self.ui {
                        ui.weather.busy = false;
                        ui.weather.status = "planning cancelled".into();
                    }
                    self.needs_redraw = true;
                }
                crate::render::ui::UiAction::FollowSet { on } => {
                    self.set_follow(on);
                }
                crate::render::ui::UiAction::RouteCreate => {
                    self.finish_route_edit();
                    self.ensure_route_store();
                    let route = crate::nav::Route::new(unique_route_name(
                        self.route_store.as_ref(),
                    ));
                    let id = route.id;
                    let saved = self
                        .route_store
                        .as_mut()
                        .map(|s| s.upsert_route(route))
                        .transpose();
                    match saved {
                        Ok(_) => {
                            if let Some(ref mut ui) = self.ui {
                                ui.routes.editing = Some(id);
                                ui.routes.visible.insert(id);
                                ui.routes.status =
                                    "tap the chart to place the first waypoint".into();
                            }
                            self.refresh_route_rows();
                            self.needs_redraw = true;
                        }
                        Err(e) => {
                            if let Some(ref mut ui) = self.ui {
                                ui.routes.status = format!("could not create the route: {e}");
                            }
                        }
                    }
                }
                crate::render::ui::UiAction::RouteDelete { route_id } => {
                    self.ensure_route_store();
                    // Following a route that is about to stop existing would
                    // leave the strip guiding towards nothing.
                    if self.ui.as_ref().and_then(|u| u.routes.active) == Some(route_id) {
                        self.deactivate_route();
                    }
                    let result = self
                        .route_store
                        .as_mut()
                        .map(|s| s.delete_route(route_id))
                        .transpose();
                    if let Some(ref mut ui) = self.ui {
                        match result {
                            Ok(_) => {
                                ui.routes.status = "route deleted".into();
                                ui.routes.visible.remove(&route_id);
                                if ui.routes.editing == Some(route_id) {
                                    ui.routes.editing = None;
                                }
                            }
                            Err(e) => ui.routes.status = format!("could not delete: {e}"),
                        }
                    }
                    self.refresh_route_rows();
                    self.needs_redraw = true;
                }
                crate::render::ui::UiAction::RouteRename { route_id, name } => {
                    self.edit_route(route_id, |route| {
                        route.name = name;
                        true
                    });
                }
                crate::render::ui::UiAction::RouteEdit { route_id: None } => {
                    self.finish_route_edit();
                }
                crate::render::ui::UiAction::RouteEdit { route_id } => {
                    // Switching straight from one route to another still
                    // finishes the first, so an empty one does not linger.
                    if self.ui.as_ref().and_then(|u| u.routes.editing).is_some() {
                        self.finish_route_edit();
                    }
                    if let Some(ref mut ui) = self.ui {
                        ui.routes.editing = route_id;
                        ui.routes.status = match route_id {
                            Some(_) => "tap the chart to add a waypoint".into(),
                            None => String::new(),
                        };
                        if let Some(id) = route_id {
                            ui.routes.visible.insert(id);
                        }
                    }
                    self.needs_redraw = true;
                }
                crate::render::ui::UiAction::RouteWaypointDelete {
                    route_id,
                    index,
                    waypoint,
                } => {
                    let mut removed_at = None;
                    self.edit_route(route_id, |route| {
                        let Some(at) = resolve_waypoint(route, index, waypoint) else {
                            return false;
                        };
                        route.waypoints.remove(at);
                        removed_at = Some(at);
                        true
                    });
                    if let Some(at) = removed_at {
                        let name = self
                            .route_store
                            .as_ref()
                            .and_then(|s| s.waypoints.get(waypoint))
                            .map(|w| w.name.clone())
                            .unwrap_or_default();
                        if let Some(ref mut ui) = self.ui {
                            ui.routes.undo = Some((route_id, at, waypoint, name));
                        }
                    }
                }
                crate::render::ui::UiAction::RouteWaypointUndo => {
                    let undo = self.ui.as_mut().and_then(|u| u.routes.undo.take());
                    if let Some((route_id, at, waypoint, _)) = undo {
                        // The mark itself was never deleted — a route
                        // dropping a waypoint does not unmake the place —
                        // so putting it back is only a matter of order.
                        let exists = self
                            .route_store
                            .as_ref()
                            .is_some_and(|s| s.waypoints.get(waypoint).is_some());
                        if exists {
                            self.edit_route(route_id, |route| {
                                let at = at.min(route.waypoints.len());
                                route.waypoints.insert(at, waypoint);
                                true
                            });
                        }
                    }
                }
                crate::render::ui::UiAction::RouteWaypointMove {
                    route_id,
                    index,
                    waypoint,
                    delta,
                } => {
                    self.edit_route(route_id, |route| {
                        let Some(at) = resolve_waypoint(route, index, waypoint) else {
                            return false;
                        };
                        let Some(to) = at.checked_add_signed(delta as isize) else {
                            return false;
                        };
                        if to >= route.waypoints.len() {
                            return false;
                        }
                        route.waypoints.swap(at, to);
                        true
                    });
                }
                crate::render::ui::UiAction::RouteWaypointRename {
                    route_id,
                    index,
                    waypoint,
                    name,
                } => {
                    self.ensure_route_store();
                    let wp = self.route_store.as_ref().and_then(|s| {
                        let route = s.route(route_id)?;
                        let at = resolve_waypoint(route, index, waypoint)?;
                        let mut wp = s.waypoints.get(route.waypoints[at])?.clone();
                        wp.name = name;
                        Some(wp)
                    });
                    if let (Some(store), Some(wp)) = (self.route_store.as_mut(), wp) {
                        // Each route's GPX inlines its marks by value, so a
                        // mark renamed here is stale in every sibling route
                        // that shares it until they are written too. The
                        // store knows which those are.
                        if let Err(e) = store.update_waypoint(wp) {
                            log::warn!("waypoint rename not saved: {e}");
                        }
                    }
                    self.refresh_route_rows();
                    self.needs_redraw = true;
                }
                crate::render::ui::UiAction::RouteReverse { route_id } => {
                    self.edit_route(route_id, |route| {
                        route.waypoints.reverse();
                        true
                    });
                }
                crate::render::ui::UiAction::WindToggle => {
                    let showing = self
                        .ui
                        .as_mut()
                        .map(|u| {
                            u.wind.show = !u.wind.show;
                            u.wind.show
                        })
                        .unwrap_or(false);
                    // Turning it on with nothing loaded fetches for what is
                    // on screen — one click, not two.
                    let empty = self.wind_forecast.is_none();
                    if showing && empty {
                        self.spawn_wind_fetch();
                    }
                    self.needs_redraw = true;
                }
                crate::render::ui::UiAction::ChartFolderOpen { path } => {
                    if self.chart_load.is_none() {
                        self.start_chart_folder_load(std::path::PathBuf::from(&path), false);
                    }
                }
                crate::render::ui::UiAction::WeatherSheetToggle => {
                    let (showing, empty) = match self.ui.as_mut() {
                        Some(u) => {
                            u.sheet.show = !u.sheet.show;
                            (u.sheet.show, u.sheet.data.is_none())
                        }
                        None => (false, true),
                    };
                    // Opening it with nothing in it fetches for where the
                    // chart is looking — one click, not two, same as the
                    // barbs have always done.
                    if showing && empty {
                        self.spawn_point_fetch();
                    }
                    self.needs_redraw = true;
                }
                crate::render::ui::UiAction::WeatherRefreshHere => {
                    self.spawn_point_fetch();
                    let (barbs, set) = self
                        .ui
                        .as_ref()
                        .map(|u| (u.wind.show || u.wind.fill, u.sheet.current_field))
                        .unwrap_or((false, false));
                    if barbs {
                        self.spawn_wind_fetch();
                    }
                    if set {
                        self.spawn_current_fetch();
                    }
                }
                crate::render::ui::UiAction::WindFillToggle => {
                    let showing = self
                        .ui
                        .as_mut()
                        .map(|u| {
                            u.wind.fill = !u.wind.fill;
                            u.wind.fill
                        })
                        .unwrap_or(false);
                    // The wash and the barbs read the same field, so turning
                    // it on with nothing loaded fetches once, exactly as the
                    // barbs do.
                    if showing && self.wind_forecast.is_none() {
                        self.spawn_wind_fetch();
                    }
                    self.needs_redraw = true;
                }
                crate::render::ui::UiAction::CurrentFieldToggle => {
                    let showing = self
                        .ui
                        .as_mut()
                        .map(|u| {
                            u.sheet.current_field = !u.sheet.current_field;
                            u.sheet.current_field
                        })
                        .unwrap_or(false);
                    if showing && self.current_forecast.is_none() {
                        self.spawn_current_fetch();
                    }
                    self.needs_redraw = true;
                }
                crate::render::ui::UiAction::BoatSearch { query, country } => {
                    self.spawn_orc_search(query, country);
                }
                crate::render::ui::UiAction::BoatUsePolar { index } => {
                    self.adopt_orc_polar(index);
                    self.save_settings();
                }
                crate::render::ui::UiAction::RoutesFetchSignalK => {
                    self.ensure_route_store();
                    match self
                        .ui
                        .as_ref()
                        .and_then(|u| crate::signalk::resources::http_base(&u.instruments.url))
                    {
                        Some(base) => {
                            if let Some(ref mut ui) = self.ui {
                                ui.routes.net_busy = true;
                                ui.routes.status = "Fetching routes from Signal K…".into();
                            }
                            let tx = self.route_net_sender();
                            std::thread::spawn(move || {
                                let _done = JobGuard(tx.clone());
                                let client =
                                    crate::signalk::resources::ResourcesClient::new(base);
                                match client.list_routes() {
                                    Ok(entries) => {
                                        let routes: Vec<_> = entries
                                            .iter()
                                            .filter_map(|(id, v)| {
                                                crate::signalk::resources::resource_to_route(
                                                    id, v,
                                                )
                                            })
                                            .collect();
                                        let _ = tx.send(RouteNetEvent::Fetched(routes));
                                    }
                                    Err(e) => {
                                        let _ = tx.send(RouteNetEvent::Status(format!(
                                            "Could not list Signal K routes: {e}"
                                        )));
                                    }
                                }
                            });
                        }
                        None => {
                            if let Some(ref mut ui) = self.ui {
                                ui.routes.status =
                                    "Set a Signal K server under Instruments first".into();
                            }
                        }
                    }
                }
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

        let half_w = (self.camera.viewport_width * self.camera.zoom / 2.0) as f64;
        let half_h = (self.camera.viewport_height * self.camera.zoom / 2.0) as f64;

        let min_x = extent.min_x + half_w;
        let max_x = extent.max_x - half_w;
        let min_y = extent.min_y + half_h;
        let max_y = extent.max_y - half_h;

        // If the extent is smaller than the viewport at the current zoom there
        // is no range that keeps the screen full of chart. The centre of the
        // screen is then only kept over the charts, not pinned to their
        // middle: pinning it — as this once did, every frame — undid every
        // pan and every zoom-at-the-cursor at the zoom navcore opens at, and
        // snapped the boat away from the middle while following her.
        if min_x.is_finite() && max_x.is_finite() && min_x <= max_x {
            self.camera.position.x = self.camera.position.x.clamp(min_x, max_x);
        } else {
            self.camera.position.x = self
                .camera
                .position
                .x
                .clamp(extent.min_x, extent.max_x);
        }

        if min_y.is_finite() && max_y.is_finite() && min_y <= max_y {
            self.camera.position.y = self.camera.position.y.clamp(min_y, max_y);
        } else {
            self.camera.position.y = self
                .camera
                .position
                .y
                .clamp(extent.min_y, extent.max_y);
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
        renderer.set_state(render_pass, bind_group, self.camera_slot_offset(SLOT_MARINER));
        renderer.draw_range(render_pass, buffer, 0, self.mariner_count);
    }

    /// Draw one line batch: an ordinary stroke (an indexed strip through
    /// `stroke`) or an LC() line (instanced segments through the LC
    /// pipeline). `lc_bound` remembers which of the two is set.
    fn draw_line_batch<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        batch: &'a LineBatchGpu,
        stroke: &'a wgpu::RenderPipeline,
        lc_bound: &mut Option<bool>,
    ) {
        render_pass.set_vertex_buffer(0, batch.vertex_buffer.slice(..));
        match &batch.index_buffer {
            Some(indices) => {
                if *lc_bound != Some(false) {
                    render_pass.set_pipeline(stroke);
                    *lc_bound = Some(false);
                }
                render_pass.set_index_buffer(indices.slice(..), wgpu::IndexFormat::Uint32);
                render_pass.draw_indexed(0..batch.index_count, 0, 0..1);
            }
            None => {
                if *lc_bound != Some(true) {
                    render_pass.set_pipeline(&self.lc_pipeline);
                    *lc_bound = Some(true);
                }
                render_pass.draw(0..6, 0..batch.vertex_count);
            }
        }
    }

    /// Byte offset of a camera slot in the uniform buffer.
    fn camera_slot_offset(&self, slot: usize) -> u32 {
        slot as u32 * self.camera_slot_align
    }

    /// Camera slot offset for entry `i` of the tile draw list, if it got one.
    fn tile_cam(&self, i: usize) -> Option<u32> {
        let slot = SLOT_FIRST_TILE + i;
        (slot < CAMERA_SLOTS).then(|| self.camera_slot_offset(slot))
    }

    /// Fill the camera slots for this frame.
    ///
    /// One slot per draw origin: the fixed ones (global, text, mariner) and
    /// then one per entry of the tile draw list, each re-basing the camera on
    /// that tile's centre. The matrices are made in f64 and only then cast,
    /// which is the whole point — see `Camera::view_projection_relative`.
    fn write_camera_slots(&mut self) {
        let ppm = 1.0 / self.camera.zoom;
        let px_per_point = self.scale_factor.max(0.5);
        let view_size = [self.size.width as f32, self.size.height as f32];
        let anchor = self.camera.position;

        let mut origins = vec![glam::DVec2::ZERO; SLOT_FIRST_TILE];
        origins[SLOT_GLOBAL] = glam::DVec2::ZERO;
        origins[SLOT_TEXT] = self.text_origin;
        origins[SLOT_MARINER] = self.mariner_origin;
        let tiles = self.tile_draw_list.len().min(CAMERA_SLOTS - SLOT_FIRST_TILE);
        if tiles < self.tile_draw_list.len() {
            log::warn!(
                "{} tiles to draw but only {} camera slots; the rest are skipped",
                self.tile_draw_list.len(),
                tiles
            );
        }
        origins.extend(
            self.tile_draw_list[..tiles]
                .iter()
                .map(|(key, _)| key.tile_id.origin()),
        );

        let stride = self.camera_slot_align as usize;
        let size = std::mem::size_of::<Uniforms>();
        let mut bytes = vec![0u8; origins.len() * stride];
        for (i, origin) in origins.iter().enumerate() {
            let u = Uniforms {
                view_proj: self.camera.view_projection_relative(*origin).to_cols_array_2d(),
                view_size,
                pixels_per_meter: ppm,
                px_per_point,
                anchor_offset: [
                    (origin.x - anchor.x) as f32,
                    (origin.y - anchor.y) as f32,
                ],
                _pad: [0.0; 2],
            };
            bytes[i * stride..i * stride + size].copy_from_slice(bytemuck::bytes_of(&u));
        }
        self.queue.write_buffer(&self.uniform_buffer, 0, &bytes);

        if let Some(ref patterns) = self.pattern_renderer {
            patterns.update_phase(&self.queue, [anchor.x, anchor.y], ppm as f64, px_per_point);
        }
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
            let p = self.camera.world_to_screen(wx, wy);
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
            if response.ppmm != self.effective_ppmm
                || response.settings_generation != self.settings_generation
            {
                continue; // built for a display or settings since changed
            }
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
                Err(e) => log::warn!("Tile {:?} failed: {}", response.tile_id, e),
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
                if response.ppmm != self.effective_ppmm
                    || response.settings_generation != self.settings_generation
                {
                    continue; // built for a display or settings since changed
                }
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
                    Err(e) => log::warn!("Tile {:?} failed: {}", response.tile_id, e),
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
                    self.pick_shown = answer.seq;
                    self.pick_anchor = Some([answer.position[0], answer.position[1]]);
                    self.pick_objects = answer.objects;
                    // Vessels first. A tap that lands on both a ship and the
                    // depth area under it is asking about the ship: the chart
                    // will still be there in a minute and the ship will not.
                    let tolerance =
                        (2.0 * self.effective_ppmm.max(1.0) * self.camera.zoom) as f64;
                    let mut vessels = self.picked_vessels(answer.position, tolerance.max(1.0));
                    vessels.append(&mut self.pick_objects);
                    self.pick_objects = vessels;
                    self.needs_redraw = true;
                }
            }
        }

        // 2. Compute zoom level with hysteresis (prevents z-flip on small zoom changes)
        let raw_z = zoom_from_camera_raw(self.camera.zoom);
        let z = if self.current_z == 0 {
            raw_z.round().clamp(crate::tiles::MIN_TILE_Z as f64, 18.0) as u8
        } else {
            let diff = raw_z - self.current_z as f64;
            if diff > 0.6 {
                self.current_z + 1
            } else if diff < -0.6 {
                self.current_z.saturating_sub(1).max(crate::tiles::MIN_TILE_Z)
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
                z.saturating_sub(4).max(crate::tiles::MIN_TILE_Z),
                z,
                &|x, y| cam.ground_mpp_at(x, y),
                &|b| {
                    // Behind the eye is not "far away", it is off the chart:
                    // it would project back onto the screen mirrored. The
                    // view direction is (0, sin t, -cos t) from an eye at
                    // height d cos t, so a ground point is in front when
                    // (y - eye.y) sin t + d cos^2 t > 0.
                    let tilt = cam.tilt as f64;
                    let (st, ct) = (tilt.sin(), tilt.cos());
                    let d = cam.eye_distance();
                    if (b.max_y - eye.y) * st + d * ct * ct <= 0.0 {
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
                    center_x: self.camera.position.x as f32,
                    center_y: self.camera.position.y as f32,
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
            log::warn!(
                "TILES visible={} resident={} pending={} empty={} unknown={} drawn={} empties=[{}]",
                self.visible_tiles_cache.len(),
                res, pend, empty, other,
                self.tile_draw_list.len(),
                empties.join(",")
            );
        }

        // 5. Evict tiles over budget
        // Touch every tile this frame will draw before evicting, so the LRU
        // is ordered by use rather than by upload age — without this it was
        // pure FIFO, and the tiles most likely to be thrown away were the
        // ones on screen longest, i.e. exactly what the user is looking at.
        let drawing: Vec<TileCacheKey> = self.tile_draw_list.iter().map(|(k, _)| *k).collect();
        for key in &drawing {
            self.tile_cache.touch(key);
        }
        self.tile_cache.evict_to_budget_keeping(&drawing);
    }

    /// Draw tiles using cached visible_tiles from build_visible_tiles()
    fn build_global_text_buffers(&mut self) {
        use crate::render::{
            text_layout::{declutter_and_layout_labels_ex, declutter_soundings},
        };
        use wgpu::util::DeviceExt;

        let mut all_soundings = Vec::new();
        let mut all_labels = Vec::new();

        // Tile packets hold positions relative to their own tile's centre.
        // Gathered here into one buffer, they are re-based on a single text
        // origin — the camera position — with the shift taken in f64 so
        // nothing is lost; the text slot then draws them with the camera
        // re-based on the same origin.
        let origin = self.camera.position;
        self.text_origin = origin;
        for tile_id in &self.visible_tiles_cache {
            let key = crate::tiles::cache::TileCacheKey::new(*tile_id, self.style_hash);
            if let Some(buffers) = self.tile_cache.get(&key) {
                let shift = tile_id.origin() - origin;
                let rebase = |p: [f32; 2]| {
                    [(shift.x + p[0] as f64) as f32, (shift.y + p[1] as f64) as f32]
                };
                all_soundings.extend(buffers.text_instances.iter().map(|s| {
                    let mut s = *s;
                    s.position = rebase(s.position);
                    s
                }));
                all_labels.extend(buffers.label_candidates.iter().map(|l| {
                    let mut l = l.clone();
                    l.position = rebase(l.position);
                    l
                }));
            }
        }

        declutter_soundings(&mut all_soundings, &self.camera, origin);
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
            declutter_and_layout_labels_ex(&all_labels, &self.camera, origin, important_only);
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
        // The dump is a record of *this* frame. Keeping every frame's records
        // grew without bound in a live session, and buried the captured frame
        // under a hundred warm-up ones in a NAVCORE_SHOT dump.
        if let Some(log) = &self.draw_log {
            log.borrow_mut().clear();
        }
        static DRAW_FRAME_COUNT: AtomicU32 = AtomicU32::new(0);
        let draw_frame = DRAW_FRAME_COUNT.fetch_add(1, Ordering::Relaxed);
        let debug_mode = debug_render_mode();

        // === Priority-ordered drawing ===
        // S-52 requires interleaved draw order: for each priority level (0-9),
        // draw areas → patterns → lines → symbols, then text on top.

        // Pre-collect and sort all line batches by priority
        let mut legacy_draws: Vec<(&LineBatchGpu, LineStyleKey, Option<[u32; 4]>, u32)> = Vec::new();
        for (i, (key, scissor)) in self.tile_draw_list.iter().enumerate() {
            let Some(cam) = self.tile_cam(i) else { continue };
            if let Some(buffers) = self.tile_cache.get(key) {
                for batch in &buffers.line_batches {
                    if batch.vertex_count > 0 {
                        legacy_draws.push((batch, batch.key.style.clone(), *scissor, cam));
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
                let end = legacy_draws.partition_point(|(b, _, _, _)| b.key.disp_prio <= prio);
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
            render_pass.set_stencil_reference(0);
            for (i, (key, scissor)) in self.tile_draw_list.iter().enumerate() {
                let Some(cam) = self.tile_cam(i) else { continue };
                if let Some(buffers) = self.tile_cache.get(key) {
                    self.apply_scissor(render_pass, *scissor);
                    if let Some(ref bg_buf) = buffers.bg_area_buffer {
                        let start = buffers.bg_area_priority_offsets[p];
                        let end = buffers.bg_area_priority_offsets[p + 1];
                        if end > start {
                            render_pass.set_bind_group(0, &self.uniform_bind_group, &[cam]);
                            render_pass.set_vertex_buffer(0, bg_buf.slice(..));
                            render_pass.draw(start..end, 0..1);
                            drew_tiles += 1;
                        }
                    }
                }
            }

            // --- Foreground areas at this priority (no stencil test) ---
            render_pass.set_pipeline(&self.pipeline);
            for (i, (key, scissor)) in self.tile_draw_list.iter().enumerate() {
                let Some(cam) = self.tile_cam(i) else { continue };
                if let Some(buffers) = self.tile_cache.get(key) {
                    self.apply_scissor(render_pass, *scissor);
                    let start = buffers.area_priority_offsets[p];
                    let end = buffers.area_priority_offsets[p + 1];
                    if end > start {
                        render_pass.set_bind_group(0, &self.uniform_bind_group, &[cam]);
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
                for (i, (key, scissor)) in self.tile_draw_list.iter().enumerate() {
                    let Some(cam) = self.tile_cam(i) else { continue };
                    if let Some(buffers) = self.tile_cache.get(key) {
                        self.apply_scissor(render_pass, *scissor);
                        // Draw bg patterns
                        if let Some(ref bg_pat_buf) = buffers.bg_pattern_buffer {
                            let start = buffers.bg_pattern_priority_offsets[p];
                            let end = buffers.bg_pattern_priority_offsets[p + 1];
                            if end > start {
                                if !bg_pattern_state_set {
                                    pattern_renderer
                                        .set_background_state(render_pass, pattern_bind_group, cam);
                                    render_pass.set_stencil_reference(0);
                                    bg_pattern_state_set = true;
                                    fg_pattern_state_set = false;
                                }
                                render_pass.set_bind_group(0, pattern_bind_group, &[cam]);
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
                                    pattern_renderer.set_state(render_pass, pattern_bind_group, cam);
                                    fg_pattern_state_set = true;
                                    bg_pattern_state_set = false;
                                }
                                render_pass.set_bind_group(0, pattern_bind_group, &[cam]);
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
                .partition_point(|(b, _, _, _)| b.key.is_background)
                + l_start;

            // Background lines (stencil test: pass when stencil==0)
            if bg_end > l_start {
                let mut lc_bound = None;
                render_pass.set_stencil_reference(0);
                for (batch, style_key, scissor, cam) in &legacy_draws[l_start..bg_end] {
                    if last_style.as_ref() != Some(style_key) {
                        last_style = Some(style_key.clone());
                        uniform_updates += 1;
                    }
                    let offset = line_style_offsets.get(style_key).copied().unwrap_or(0);
                    self.apply_scissor(render_pass, *scissor);
                    render_pass.set_bind_group(0, &self.uniform_bind_group, &[*cam]);
                    render_pass.set_bind_group(1, &self.line_bind_group, &[offset]);
                    self.draw_line_batch(render_pass, batch, &self.bg_line_pipeline, &mut lc_bound);
                    drew_line_batches += 1;
                    line_index_count += batch.index_count;
                    line_vertex_count += batch.vertex_count;
                }
            }

            // Foreground lines (no stencil test)
            if l_end > bg_end {
                let mut lc_bound = None;
                for (batch, style_key, scissor, cam) in &legacy_draws[bg_end..l_end] {
                    if last_style.as_ref() != Some(style_key) {
                        last_style = Some(style_key.clone());
                        uniform_updates += 1;
                    }
                    let offset = line_style_offsets.get(style_key).copied().unwrap_or(0);
                    self.apply_scissor(render_pass, *scissor);
                    render_pass.set_bind_group(0, &self.uniform_bind_group, &[*cam]);
                    render_pass.set_bind_group(1, &self.line_bind_group, &[offset]);
                    self.draw_line_batch(render_pass, batch, &self.line_pipeline, &mut lc_bound);
                    drew_line_batches += 1;
                    line_index_count += batch.index_count;
                    line_vertex_count += batch.vertex_count;
                }
            }

            // --- Light sector arcs and legs at this priority ---
            // After the lines, as they were when they were line batches.
            let mut sector_pipeline_set = false;
            for (i, (key, scissor)) in self.tile_draw_list.iter().enumerate() {
                let Some(cam) = self.tile_cam(i) else { continue };
                let Some(buffers) = self.tile_cache.get(key) else { continue };
                let Some(ref buf) = buffers.sector_buffer else { continue };
                let start = buffers.sector_priority_offsets[p];
                let end = buffers.sector_priority_offsets[p + 1];
                if end > start {
                    if !sector_pipeline_set {
                        self.sector_renderer.set_pipeline(render_pass);
                        sector_pipeline_set = true;
                    }
                    self.apply_scissor(render_pass, *scissor);
                    render_pass.set_bind_group(0, &self.uniform_bind_group, &[cam]);
                    self.sector_renderer.draw_range(render_pass, buf, start, end - start);
                }
            }

            // --- Symbols at this priority ---
            if let (Some(ref symbol_renderer), Some(ref symbol_bind_group)) =
                (&self.symbol_renderer, &self.symbol_camera_bind_group)
            {
                if debug_mode == 3 && priority == 0 {
                    symbol_renderer.render_atlas_debug(
                        render_pass,
                        symbol_bind_group,
                        self.camera_slot_offset(SLOT_GLOBAL),
                    );
                    symbol_draw_calls = 1;
                } else if debug_mode != 3 {
                    let mut symbol_state_set = false;
                    for (i, (key, scissor)) in self.tile_draw_list.iter().enumerate() {
                        let Some(cam) = self.tile_cam(i) else { continue };
                        if let Some(buffers) = self.tile_cache.get(key) {
                            self.apply_scissor(render_pass, *scissor);
                            if let Some(ref symbol_buffer) = buffers.symbol_buffer {
                                let start = buffers.symbol_priority_offsets[p];
                                let end = buffers.symbol_priority_offsets[p + 1];
                                if end > start {
                                    if !symbol_state_set {
                                        symbol_renderer.set_state(render_pass, symbol_bind_group, cam);
                                        symbol_state_set = true;
                                    }
                                    render_pass.set_bind_group(0, symbol_bind_group, &[cam]);
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
                        self.camera_slot_offset(SLOT_TEXT),
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
                    self.camera_slot_offset(SLOT_TEXT),
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
                        self.camera_slot_offset(SLOT_TEXT),
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
            // Seed the remembered folder before anything can save over it.
            // A session started on the test triangle, or on a folder named
            // once on the command line, still saves its settings when the
            // user moves an instrument — and an empty field written then
            // would quietly forget the boat's own charts.
            if let (Some(ref mut ui), Some(dir)) =
                (self.ui.as_mut(), Self::remembered_chart_folder())
            {
                ui.charts.chosen = dir.display().to_string();
            }
            // `NAVCORE_ROUTE_NEW="lat,lon;lat,lon;…"` builds a route by the
            // same path a tap takes, so the editor can be captured without
            // a pointer.
            if let Ok(spec) = std::env::var("NAVCORE_ROUTE_NEW") {
                self.ensure_route_store();
                let route = crate::nav::Route::new(unique_route_name(self.route_store.as_ref()));
                let id = route.id;
                if let Some(store) = self.route_store.as_mut() {
                    let _ = store.upsert_route(route);
                }
                if let Some(ref mut ui) = self.ui {
                    ui.routes.open = true;
                    ui.routes.editing = Some(id);
                    ui.routes.visible.insert(id);
                }
                for point in spec.split(';').filter(|s| !s.trim().is_empty()) {
                    match crate::geo::parse_latlon(point) {
                        Some(p) => {
                            let (x, y) =
                                crate::render::projection::Projection::to_mercator(p.lat, p.lon);
                            self.route_edit_append(x, y);
                        }
                        None => log::warn!("NAVCORE_ROUTE_NEW: cannot read '{point}'"),
                    }
                }
                self.refresh_route_rows();
            }
            // `NAVCORE_BOAT="query[;country]"` opens the boat window and
            // fires the polar search, for captures. "1" just opens it.
            if let Ok(spec) = std::env::var("NAVCORE_BOAT") {
                if let Some(ref mut ui) = self.ui {
                    ui.boat.open = true;
                }
                if spec != "1" {
                    let (query, country) = match spec.split_once(';') {
                        Some((q, c)) => (q.to_string(), c.to_string()),
                        None => (spec.clone(), "DEN".to_string()),
                    };
                    if let Some(ref mut ui) = self.ui {
                        ui.boat.search = query.clone();
                        ui.boat.country = country.clone();
                    }
                    self.spawn_orc_search(query, country);
                }
            }
            // `NAVCORE_PLAN_PICK=from|to` arms a planner pin at startup, so
            // the tap-to-fill path can be driven by NAVCORE_PICK below.
            if let Ok(which) = std::env::var("NAVCORE_PLAN_PICK") {
                if let Some(ref mut ui) = self.ui {
                    ui.plan.picking = match which.as_str() {
                        "from" => Some(crate::render::ui::PlanPickTarget::From),
                        "to" => Some(crate::render::ui::PlanPickTarget::To),
                        _ => None,
                    };
                }
            }
            // `NAVCORE_PICK=lat,lon` taps the chart at startup, so the object
            // bubble can be captured without a click.
            if let Ok(at) = std::env::var("NAVCORE_PICK") {
                let parts: Vec<f64> = at.split(',').filter_map(|v| v.trim().parse().ok()).collect();
                if parts.len() == 2 {
                    let (x, y) = crate::render::projection::Projection::to_mercator(parts[0], parts[1]);
                    self.pick_at_world(x, y);
                }
            }
            // `NAVCORE_ROUTES_OPEN=1` opens the routes window at startup.
            if std::env::var("NAVCORE_ROUTES_OPEN").is_ok_and(|v| v != "0") {
                self.ensure_route_store();
                self.refresh_route_rows();
                if let Some(ref mut ui) = self.ui {
                    ui.routes.open = true;
                }
            }
            // `NAVCORE_FOLLOW=1` activates the first stored route at startup,
            // so the guidance strip can be captured without a click.
            if std::env::var("NAVCORE_FOLLOW").is_ok_and(|v| v != "0") {
                self.ensure_route_store();
                if let Some(id) = self
                    .route_store
                    .as_ref()
                    .and_then(|s| s.routes().next())
                    .map(|r| r.id)
                {
                    self.activate_route(id);
                }
            }
            // `NAVCORE_SHOP=1` opens the chart shop at startup, so it can be
            // captured without a click.
            let mut signalk_override_saved: Option<String> = None;
            if let Some(ui) = self.ui.as_mut() {
                if std::env::var("NAVCORE_SHOP").is_ok_and(|v| v != "0") {
                    ui.shop.open = true;
                    ui.shop.email = std::env::var("NAVCORE_SHOP_EMAIL").unwrap_or_default();
                    // `NAVCORE_SHOP=free` or `=folder` opens that tab.
                    ui.charts.tab = match std::env::var("NAVCORE_SHOP").as_deref() {
                        Ok("free") => crate::render::ui::ChartsTab::Free,
                        Ok("folder") => crate::render::ui::ChartsTab::Folder,
                        _ => crate::render::ui::ChartsTab::Shop,
                    };
                }
                // `NAVCORE_SIGNALK=<address>` overrides the saved server for
                // this run only. It used to claim it did not touch settings
                // while quietly doing exactly that: the override went into
                // the live view, and the next save — which any settings
                // change triggers — wrote it over the address the user had
                // actually configured. The real one is kept here and put
                // back at save time.
                if let Ok(url) = std::env::var("NAVCORE_SIGNALK") {
                    if !url.is_empty() {
                        signalk_override_saved = Some(ui.instruments.url.clone());
                        ui.instruments.url = url;
                    }
                }
            }
            self.signalk_override_saved = signalk_override_saved;
            self.maybe_env_plan();
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
                if let Some(ref mut ui) = self.ui {
                    ui.instruments.active = true;
                }
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

    /// Is this physical-pixel position covered by the interface? For touch,
    /// where there is no hovering pointer to ask about.
    pub fn ui_covers(&self, x: f32, y: f32) -> bool {
        self.ui.as_ref().is_some_and(|u| u.covers(x, y))
    }

    /// Esc: back out of whatever is most in the way — the info bubble, then
    /// an armed planner pin, then route editing, then an open window.
    /// Answers whether it did anything.
    pub fn escape(&mut self) -> bool {
        if self.pick_active() {
            self.dismiss_pick();
            return true;
        }
        if let Some(ref mut ui) = self.ui {
            if ui.plan.picking.take().is_some() {
                self.needs_redraw = true;
                return true;
            }
        }
        if self.ui.as_ref().and_then(|u| u.routes.editing).is_some() {
            self.finish_route_edit();
            return true;
        }
        // Then the windows, one per press, the last-opened kind first as far
        // as a fixed order can guess it: the object-level ones before the
        // settings-level ones.
        if let Some(ref mut ui) = self.ui {
            for open in [
                &mut ui.display.open,
                &mut ui.boat.open,
                &mut ui.routes.open,
                &mut ui.instruments.open,
                &mut ui.shop.open,
            ] {
                if *open {
                    *open = false;
                    self.needs_redraw = true;
                    return true;
                }
            }
        }
        false
    }

    /// A tap on the chart, from mouse or finger.
    ///
    /// An armed planner pin or an open route editor takes the tap even while
    /// the info bubble is up — the user asked for that tap deliberately, and
    /// losing it to "close the bubble" left the pin armed and the waypoint
    /// unplaced. Otherwise a tap with the bubble open closes it, the usual
    /// tap-away of a popover, and a tap with nothing open asks what is there.
    pub fn tap_at_screen(&mut self, x: f32, y: f32) {
        let world = self.camera.screen_to_world(x, y);
        let (wx, wy) = (world.x as f64, world.y as f64);
        if self.plan_pick_fill(wx, wy) || self.route_edit_append(wx, wy) {
            self.dismiss_pick();
            return;
        }
        if self.pick_active() {
            self.dismiss_pick();
        } else {
            self.pick_at_world(world.x, world.y);
        }
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
    pub fn pick_at_world(&mut self, x: f64, y: f64) {
        // An armed planner pin claims the tap first — it is a deliberate
        // one-shot — then an open route editor, which is a standing mode.
        // Only if neither wants it does the tap ask "what is that?".
        if self.plan_pick_fill(x, y) {
            return;
        }
        if self.route_edit_append(x, y) {
            return;
        }
        // A fingertip is about 9 mm across; a mouse pointer is exact. Sizing
        // the tolerance in millimetres rather than pixels makes the same tap
        // work on a plotter's touchscreen and on a desktop, at any density.
        let radius_px = 2.0 * self.effective_ppmm.max(1.0);
        self.pick_anchor = None;
        self.pick_objects.clear();
        if let Some(ref worker) = self.tile_worker {
            self.pick_pending = Some(worker.request_pick(
                [x, y],
                (radius_px * self.camera.zoom) as f64,
            ));
        }
        self.needs_redraw = true;
    }

    /// If a planner pin is armed, fill its field from this tap and disarm.
    /// Answers whether the tap was claimed.
    fn plan_pick_fill(&mut self, x: f64, y: f64) -> bool {
        let Some(ref mut ui) = self.ui else { return false };
        let Some(target) = ui.plan.picking.take() else { return false };
        let (lat, lon) = crate::render::projection::Projection::to_wgs84(x, y);
        let text = crate::geo::format_latlon(lat, lon);
        match target {
            crate::render::ui::PlanPickTarget::From => ui.plan.from = text,
            crate::render::ui::PlanPickTarget::To => ui.plan.to = text,
        }
        self.needs_redraw = true;
        true
    }

    /// The planner's endpoints, projected for the pins. Same discipline as
    /// [`routes_display`](Self::routes_display): runs before the frame takes
    /// `self.ui`, or it would see None and draw nothing.
    fn plan_pins(&self) -> Vec<crate::render::ui::PlanPin> {
        let Some(ref ui) = self.ui else { return Vec::new() };
        let ppp = self.scale_factor.max(0.01);
        let mut pins = Vec::new();
        for (text, is_start) in [(&ui.plan.from, true), (&ui.plan.to, false)] {
            let Some(p) = crate::geo::parse_latlon(text) else { continue };
            let (wx, wy) = crate::render::projection::Projection::to_mercator(p.lat, p.lon);
            let s = self.camera.world_to_screen(wx, wy);
            pins.push(crate::render::ui::PlanPin {
                screen: [s.x / ppp, s.y / ppp],
                is_start,
            });
        }
        pins
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
        #[derive(serde::Serialize)]
        struct Persisted<'a> {
            instruments: &'a crate::render::ui::InstrumentView,
            weather: &'a crate::render::ui::WeatherView,
            boat: &'a crate::render::ui::BoatView,
            charts: &'a crate::render::ui::ChartFolderView,
            display: &'a crate::render::ui::DisplayView,
        }
        // An env override is for this run; the file keeps what the user set.
        let mut instruments = ui.instruments.clone();
        if let Some(ref saved) = self.signalk_override_saved {
            instruments.url = saved.clone();
        }
        let Ok(json) = serde_json::to_string_pretty(&Persisted {
            instruments: &instruments,
            weather: &ui.weather,
            boat: &ui.boat,
            charts: &ui.charts,
            display: &ui.display,
        }) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Written aside and renamed into place: power on a boat goes when it
        // goes, and a settings file cut off mid-write would lose everything.
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, json).and_then(|_| std::fs::rename(&tmp, &path)) {
            // Not fatal: the session works, it just will not be remembered.
            log::warn!("could not save settings to {}: {e}", path.display());
        }
    }

    pub(crate) fn load_settings() -> Option<crate::render::ui::InstrumentView> {
        let path = Self::settings_path()?;
        let text = std::fs::read_to_string(&path).ok()?;
        #[derive(serde::Deserialize)]
        struct Persisted {
            instruments: crate::render::ui::InstrumentView,
        }
        // Current shape first, then the pre-weather file that was the
        // instruments alone — the user keeps their layout across the change.
        if let Ok(p) = serde_json::from_str::<Persisted>(&text) {
            return Some(p.instruments);
        }
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

    pub(crate) fn load_weather_settings() -> crate::render::ui::WeatherView {
        let Some(path) = Self::settings_path() else {
            return Default::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Default::default();
        };
        #[derive(serde::Deserialize)]
        struct Persisted {
            #[serde(default)]
            weather: crate::render::ui::WeatherView,
        }
        serde_json::from_str::<Persisted>(&text)
            .map(|p| p.weather)
            .unwrap_or_default()
    }

    pub(crate) fn load_display_settings() -> crate::render::ui::DisplayView {
        let Some(path) = Self::settings_path() else {
            return Default::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Default::default();
        };
        #[derive(serde::Deserialize)]
        struct Persisted {
            #[serde(default)]
            display: crate::render::ui::DisplayView,
        }
        serde_json::from_str::<Persisted>(&text)
            .map(|p| p.display)
            .unwrap_or_default()
    }

    /// Put the Display window's choices into effect: the palette at once,
    /// and the mariner's settings in both engines, starting the tiles over
    /// when they changed. Safe to call often — nothing happens when nothing
    /// changed.
    pub fn apply_display(&mut self) {
        let Some(ref ui) = self.ui else { return };
        let display = ui.display.clone();
        let draft = ui.boat.draft_m;
        if std::env::var("NAVCORE_PALETTE").is_err()
            && self.palette_name.as_deref() != Some(display.palette.table())
        {
            self.switch_palette(display.palette.table());
        }
        let Some(ref engine) = self.s52_engine else { return };
        let base = crate::s52::MarinerSettings::from_env();
        let settings = display.apply(&base, draft);
        if engine.settings == settings {
            return;
        }
        if let Some(ref mut engine) = self.s52_engine {
            engine.set_settings(settings.clone());
        }
        self.settings_generation += 1;
        if let Some(ref worker) = self.tile_worker {
            worker.set_settings(settings, self.settings_generation);
        }
        self.tile_cache.clear();
        self.pending_tiles.clear();
        self.deferred_tile_results.clear();
        self.needs_redraw = true;
    }

    pub(crate) fn load_boat_settings() -> crate::render::ui::BoatView {
        let Some(path) = Self::settings_path() else {
            return Default::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Default::default();
        };
        #[derive(serde::Deserialize)]
        struct Persisted {
            #[serde(default)]
            boat: crate::render::ui::BoatView,
        }
        serde_json::from_str::<Persisted>(&text)
            .map(|p| p.boat)
            .unwrap_or_default()
    }

    /// Project the boat onto the screen for the overlay.
    ///
    /// The renderer does this rather than the UI because the camera lives
    /// here, and because the UI should not learn about Mercator to draw a
    /// triangle.
    fn own_ship_view(&self) -> Option<crate::render::ui_ownship::OwnShip> {
        let (lat, lon) = self.fleet.own.position()?;
        let (wx, wy) = crate::render::projection::Projection::to_mercator(lat, lon);
        let screen = self.camera.world_to_screen(wx, wy);
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

    /// Vessels under a tap, as query results.
    ///
    /// Presented as [`PickedObject`]s so the bubble that answers "what is
    /// that?" for a buoy answers it for a ship too, in the same place, with
    /// the same gesture. A watch officer should not have to learn that chart
    /// objects are tapped and vessels are found some other way.
    fn picked_vessels(
        &self,
        world: [f64; 2],
        tolerance_m: f64,
    ) -> Vec<crate::pick::PickedObject> {
        use crate::geo::{cpa, to_nm, LatLon, Motion};

        let (lat, lon) = crate::render::projection::Projection::to_wgs84(world[0], world[1]);
        let at = LatLon::new(lat, lon);
        let prefs = self
            .ui
            .as_ref()
            .map(|u| u.instruments.units)
            .unwrap_or_default();
        let show_ais = self.ui.as_ref().map(|u| u.instruments.show_ais).unwrap_or(true);
        if !show_ais {
            return Vec::new();
        }

        let own = self.own_motion();

        let mut out = Vec::new();
        for target in self.fleet.targets() {
            let Some((tlat, tlon)) = target.position() else {
                continue;
            };
            let there = LatLon::new(tlat, tlon);
            let distance_m = crate::geo::distance_m(at, there);
            if distance_m > tolerance_m {
                continue;
            }

            let mut attributes: Vec<(String, String)> = Vec::new();
            let mut push = |k: &str, v: String| attributes.push((k.to_string(), v));
            if let Some(mmsi) = target.mmsi() {
                push("MMSI", mmsi.to_string());
            }
            if let Some(state) = target.vessel.text("navigation.state") {
                push("Status", state.to_string());
            }
            if let Some(sog) = target.speed() {
                let r = crate::signalk::Quantity::Speed.format(sog, &prefs);
                push("SOG", format!("{} {}", r.value, r.unit));
            }
            if let Some(cog) = target.course() {
                let r = crate::signalk::Quantity::Angle.format(cog, &prefs);
                push("COG", format!("{}{}", r.value, r.unit));
            }
            if let Some(hdg) = target.vessel.heading_true() {
                let r = crate::signalk::Quantity::Angle.format(hdg, &prefs);
                push("Heading", format!("{}{}", r.value, r.unit));
            }
            // Range and bearing *from us*, which is the first thing anyone
            // wants and the thing a chart cannot show.
            if let Some(own) = own {
                let (range, bearing) = crate::geo::range_bearing(own.at, there);
                push("Range", format!("{:.2} NM", to_nm(range)));
                push("Bearing", crate::geo::format_bearing(bearing.to_degrees()));

                if let (Some(course), Some(speed)) = (target.course(), target.speed()) {
                    if let Some(c) = cpa(own, Motion { at: there, course, speed }) {
                        if c.past {
                            push("CPA", "opening — closest approach is behind us".into());
                        } else {
                            push("CPA", format!("{:.2} NM", to_nm(c.distance_m)));
                            push("TCPA", format!("{:.0} min", c.seconds / 60.0));
                        }
                    }
                }
            }
            if target.is_lost() {
                push(
                    "Lost",
                    format!("no report for {:.0} min", target.last_report.elapsed().as_secs_f64() / 60.0),
                );
            }

            out.push(crate::pick::PickedObject {
                acronym: "AIS".into(),
                title: target
                    .label()
                    .map(str::to_string)
                    .unwrap_or_else(|| "AIS target".into()),
                chart: "Signal K".into(),
                chart_scale: 0,
                geometry: crate::senc::FeatureType::Point,
                attributes,
                notes: Vec::new(),
                distance_m,
                // A vessel's summary is its own line already: name, then the
                // numbers. Nothing for S-52 to compose.
                summary: None,
                duplicates: 0,
            });
        }
        out.sort_by(|a, b| a.distance_m.total_cmp(&b.distance_m));
        out
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
        let origin = self.camera.position;
        self.mariner_origin = origin;
        let instances = crate::render::mariner::instances(
            &self.fleet,
            &symbols,
            stale_own,
            [origin.x, origin.y],
        );
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

    /// Project the visible routes for the chart overlay.
    fn routes_display(&self) -> Vec<crate::render::ui_routes::RouteDisplay> {
        let Some(store) = self.route_store.as_ref() else {
            return Vec::new();
        };
        let Some(ui) = self.ui.as_ref() else {
            return Vec::new();
        };
        let ppp = self.scale_factor.max(0.01);
        let active = ui.routes.active;
        let active_leg = self.route_follow.as_ref().map(|f| f.leg);
        // Metres per logical point and the latitude stretch, for the arrival
        // circle's on-screen radius.
        let mpp = (self.camera.zoom * ppp) as f64;

        store
            .routes()
            .filter(|r| ui.routes.visible.contains(&r.id) || active == Some(r.id))
            .map(|r| {
                let is_active = active == Some(r.id);
                let to_wp = is_active
                    .then(|| {
                        active_leg
                            .and_then(|l| r.legs.get(l))
                            .map(|l| l.to)
                    })
                    .flatten();
                let points = r
                    .waypoints
                    .iter()
                    .enumerate()
                    .filter_map(|(i, id)| {
                        let wp = store.waypoints.get(*id)?;
                        let (x, y) = crate::render::projection::Projection::to_mercator(
                            wp.position.lat,
                            wp.position.lon,
                        );
                        let s = self.camera.world_to_screen(x, y);
                        // The plan's ETA lives on the leg ENDING here.
                        let eta = (i > 0)
                            .then(|| r.legs.get(i - 1))
                            .flatten()
                            .and_then(|l| l.plan.as_ref())
                            .map(|p| {
                                // Local, like the weather sheet's clock: the
                                // same instant must not read 12:00 beside the
                                // barbs and 14:00 on the sheet.
                                p.eta.with_timezone(&chrono::Local).format("%H:%M").to_string()
                            });
                        let arrival_radius_px = (to_wp == Some(*id)).then(|| {
                            let k = 1.0 / wp.position.lat.to_radians().cos();
                            let radius_nm = wp.arrival_radius_nm.unwrap_or(0.1);
                            ((radius_nm * crate::geo::METRES_PER_NM * k) / mpp) as f32
                        });
                        Some(crate::render::ui_routes::RoutePointDisplay {
                            screen: [s.x / ppp as f32, s.y / ppp as f32],
                            name: wp.name.clone(),
                            eta,
                            arrival_radius_px,
                        })
                    })
                    .collect();
                crate::render::ui_routes::RouteDisplay {
                    name: r.name.clone(),
                    is_active,
                    active_leg: is_active.then(|| active_leg).flatten(),
                    points,
                }
            })
            .collect()
    }

    /// Project the AIS traffic onto the screen, with its closest approaches.
    ///
    /// CPA is computed geodesically rather than in the Mercator plane: at 56°
    /// north a Mercator northing is stretched by nearly a factor of two, so a
    /// range measured there would be wrong in exactly the situation the number
    /// exists for.
    /// Own ship's position and motion, for CPA and range/bearing — `None`
    /// while the fix is stale. A CPA worked from where the boat was a minute
    /// ago is a confident wrong answer, the worst kind for collision work.
    fn own_motion(&self) -> Option<crate::geo::Motion> {
        let fresh = self
            .fleet
            .own
            .get("navigation.position")
            .is_some_and(|r| !r.is_stale());
        if !fresh {
            return None;
        }
        let (lat, lon) = self.fleet.own.position()?;
        Some(crate::geo::Motion {
            at: crate::geo::LatLon::new(lat, lon),
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
        })
    }

    fn ais_targets(&self) -> Vec<crate::render::ui_ais::AisTarget> {
        use crate::geo::{cpa, LatLon, Motion};

        let ppp = self.scale_factor.max(0.01);
        let own = self.own_motion();

        self.fleet
            .targets()
            .filter_map(|target| {
                let (lat, lon) = target.position()?;
                let (wx, wy) = crate::render::projection::Projection::to_mercator(lat, lon);
                let screen = self.camera.world_to_screen(wx, wy);

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
                        ui.instruments.active =
                            !matches!(s, crate::signalk::service::Status::Disconnected);
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
        if follow {
            self.recentre_on_boat();
        }
        self.needs_redraw = true;
    }

    /// Put the boat in the middle of the chart. Only while the fix is fresh:
    /// a chart that keeps recentring on a dead position is worse than one
    /// that stays put and lets the stale marker drift off.
    fn recentre_on_boat(&mut self) {
        let Some((lat, lon)) = self.fleet.own.position() else { return };
        let fresh = self
            .fleet
            .own
            .get("navigation.position")
            .is_some_and(|r| !r.is_stale());
        if fresh {
            let (x, y) = crate::render::projection::Projection::to_mercator(lat, lon);
            self.camera.position = glam::DVec2::new(x, y);
            self.following = true;
        }
    }

    /// Turn following on or off, from the button on the chart or the `C` key.
    /// On means now, not at the next position report.
    pub fn set_follow(&mut self, on: bool) {
        if let Some(ref mut ui) = self.ui {
            ui.instruments.follow = on;
        }
        self.following = false;
        if on {
            self.recentre_on_boat();
        }
        self.save_settings();
        self.needs_redraw = true;
    }

    /// Whether the chart is keeping the boat centred.
    pub fn follow_on(&self) -> bool {
        self.ui.as_ref().is_some_and(|u| u.instruments.follow)
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

        // A live subscription this machine holds no slot for: the download
        // spends one, for good. Said before, not discovered after.
        if !chart.expired {
            let machine = ui.shop.system_name.clone().unwrap_or_default();
            ui.shop.pending = Some(crate::render::ui::PendingDownload {
                chart_id: chart_id.to_string(),
                chart_name: chart.name.clone(),
                edition: None,
                because: format!(
                    "This assigns one of the {} free slot(s) on this licence to \"{machine}\". \
                     o-charts cannot move or cancel an assignment once a chart is requested.",
                    chart.free_slots()
                ),
                new_slot: true,
            });
            return;
        }

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
            new_slot: false,
        });
    }

    /// Load the route store on first use, reporting problems into the window.
    fn ensure_route_store(&mut self) {
        if self.route_store.is_some() {
            return;
        }
        // `NAVCORE_ROUTES=<dir>` points the store elsewhere — captures and
        // tests must not write into the user's real route folder.
        let dir = std::env::var("NAVCORE_ROUTES")
            .map(std::path::PathBuf::from)
            .ok()
            .or_else(crate::nav::RouteStore::default_dir)
            .unwrap_or_else(|| std::path::PathBuf::from("routes"));
        match crate::nav::RouteStore::open(dir) {
            Ok((store, problems)) => {
                if let (Some(ui), false) = (self.ui.as_mut(), problems.is_empty()) {
                    ui.routes.status = problems
                        .iter()
                        .map(|p| p.to_string())
                        .collect::<Vec<_>>()
                        .join("; ");
                }
                self.route_store = Some(store);
            }
            Err(e) => {
                if let Some(ref mut ui) = self.ui {
                    ui.routes.status = format!("Route store unavailable: {e}");
                }
            }
        }
    }

    /// Begin following a route: the action handler and the capture hook share
    /// this.
    fn activate_route(&mut self, route_id: uuid::Uuid) {
        self.ensure_route_store();
        let started = self
            .route_store
            .as_ref()
            .and_then(|s| s.route(route_id))
            .and_then(|r| {
                crate::nav::Following::start(r, crate::nav::FollowConfig::default())
                    .map(|f| (f, r.name.clone()))
            });
        match started {
            Some((f, name)) => {
                self.route_follow = Some(f);
                if let Some(ref mut ui) = self.ui {
                    ui.routes.active = Some(route_id);
                    ui.routes.visible.insert(route_id);
                    ui.routes.status = format!("Following {name}");
                }
            }
            None => {
                if let Some(ref mut ui) = self.ui {
                    ui.routes.status =
                        "That route cannot be followed (fewer than two waypoints)".into();
                }
            }
        }
        self.refresh_route_rows();
    }

    /// Rebuild the routes window's table from the store.
    fn refresh_route_rows(&mut self) {
        let Some(ref mut ui) = self.ui else { return };
        let active = ui.routes.active;
        let editing = ui.routes.editing;
        ui.routes.rows = self
            .route_store
            .as_ref()
            .map(|store| {
                store
                    .routes()
                    .map(|r| crate::render::ui_routes::RouteRow {
                        id: r.id,
                        name: r.name.clone(),
                        legs: r.legs.len(),
                        distance_nm: r.total_distance_nm(),
                        active: active == Some(r.id),
                        visible: ui.routes.visible.contains(&r.id) || active == Some(r.id),
                        // Only the route under the editor pays for its list.
                        waypoints: if editing == Some(r.id) {
                            r.waypoints
                                .iter()
                                .enumerate()
                                .filter_map(|(i, wid)| {
                                    let wp = store.waypoints.get(*wid)?;
                                    Some(crate::render::ui_routes::RouteWaypointRow {
                                        id: wp.id,
                                        name: wp.name.clone(),
                                        lat: wp.position.lat,
                                        lon: wp.position.lon,
                                        leg: i.checked_sub(1).and_then(|p| {
                                            let prev = store.waypoints.get(r.waypoints[p])?;
                                            let (m, brg) = crate::geo::range_bearing(
                                                prev.position,
                                                wp.position,
                                            );
                                            Some((
                                                m / crate::geo::METRES_PER_NM,
                                                brg.to_degrees().rem_euclid(360.0),
                                            ))
                                        }),
                                    })
                                })
                                .collect()
                        } else {
                            Vec::new()
                        },
                        provenance: r.generated.as_ref().map(|g| {
                            format!(
                                "wx: {} run {} · {}",
                                g.grib_source.as_deref().unwrap_or("?"),
                                g.grib_run
                                    .map(|t| t.format("%d %b %HZ").to_string())
                                    .unwrap_or_default(),
                                g.polar
                            )
                        }),
                    })
                    .collect()
            })
            .unwrap_or_default();
    }

    /// A sender for a worker about to be spawned. Taking one counts a job:
    /// its [`JobGuard`] reports the end, whatever the ending.
    fn route_net_sender(&mut self) -> std::sync::mpsc::Sender<RouteNetEvent> {
        self.route_net_jobs += 1;
        if self.route_net_tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            self.route_net_tx = Some(tx);
            self.route_net_rx = Some(rx);
        }
        self.route_net_tx.as_ref().unwrap().clone()
    }

    /// Take the guidance strip down.
    ///
    /// A strip of steering numbers that stopped updating is worse than no
    /// strip: XTE, bearing and time-to-go all still look live, and nothing
    /// on screen says the fix behind them is minutes old. The same reasoning
    /// as the instrument bar clearing itself when Signal K drops.
    fn clear_guidance(&mut self) {
        self.guidance_waiting(None);
    }

    /// No guidance this frame. `why`, when a route is still being followed,
    /// is what the strip says instead.
    fn guidance_waiting(&mut self, why: Option<String>) {
        if let Some(ref mut ui) = self.ui {
            if ui.routes.guidance.take().is_some() || ui.routes.guidance_waiting != why {
                self.needs_redraw = true;
            }
            ui.routes.guidance_waiting = why;
        }
    }

    /// Stop following, and forget the guidance the strip was drawing.
    fn deactivate_route(&mut self) {
        self.route_follow = None;
        if let Some(ref mut ui) = self.ui {
            ui.routes.active = None;
            ui.routes.guidance = None;
            ui.routes.guidance_waiting = None;
        }
        self.refresh_route_rows();
        self.needs_redraw = true;
    }

    /// Apply an edit to a stored route and write it back.
    ///
    /// Every manual edit goes through here so that saving, re-deriving the
    /// legs (which `upsert_route` does), refreshing the table and reporting
    /// a failure all happen in exactly one place. The closure sees the route
    /// by value; returning `false` abandons the edit untouched.
    fn edit_route(&mut self, id: uuid::Uuid, edit: impl FnOnce(&mut crate::nav::Route) -> bool) {
        self.ensure_route_store();
        // Which mark are we steering to? Reordering, reversing or removing a
        // waypoint renumbers the legs, and a follower holding a bare index
        // would silently start guiding somewhere the crew never chose. The
        // index is restored below from the identity of the target.
        let steering_to = self
            .route_follow
            .as_ref()
            .filter(|f| f.route_id == id)
            .and_then(|f| Some(f.marks_ahead(self.route_store.as_ref()?.route(id)?)));

        let Some(store) = self.route_store.as_mut() else { return };
        let Some(mut route) = store.route(id).cloned() else { return };
        if !edit(&mut route) {
            return;
        }
        let result = store.upsert_route(route);
        if let Err(e) = result {
            if let Some(ref mut ui) = self.ui {
                ui.routes.status = format!("could not save the route: {e}");
            }
            log::warn!("route edit not saved: {e}");
            return;
        }

        if let Some(target) = steering_to {
            let route = self.route_store.as_ref().and_then(|s| s.route(id)).cloned();
            if let (Some(follow), Some(route)) = (self.route_follow.as_mut(), route) {
                follow.retarget(&route, &target);
            }
        }
        // A route the user is editing is a route they want to watch.
        if let Some(ref mut ui) = self.ui {
            ui.routes.visible.insert(id);
        }
        self.refresh_route_rows();
        self.update_route_following();
        self.needs_redraw = true;
    }

    /// Append a waypoint at a tapped position to the route being edited.
    /// Answers whether the tap was claimed.
    /// Leave route editing, however the user got out: Done, closing the
    /// window, or starting on another route. A route left with no waypoints
    /// is discarded — pressing New route and changing your mind should not
    /// leave "Route 3 · 0 legs" behind to be cleaned up by hand.
    fn finish_route_edit(&mut self) {
        if let Some(ref mut ui) = self.ui {
            ui.routes.undo = None;
        }
        let Some(id) = self.ui.as_mut().and_then(|u| u.routes.editing.take()) else {
            return;
        };
        let empty = self
            .route_store
            .as_ref()
            .and_then(|s| s.route(id))
            .is_some_and(|r| r.waypoints.is_empty());
        let mut status = String::new();
        if empty {
            if self.ui.as_ref().and_then(|u| u.routes.active) == Some(id) {
                self.deactivate_route();
            }
            if let Some(Ok(_)) = self.route_store.as_mut().map(|s| s.delete_route(id)) {
                status = "empty route discarded".into();
            }
            if let Some(ref mut ui) = self.ui {
                ui.routes.visible.remove(&id);
            }
        }
        if let Some(ref mut ui) = self.ui {
            ui.routes.status = status;
        }
        self.refresh_route_rows();
        self.needs_redraw = true;
    }

    fn route_edit_append(&mut self, x: f64, y: f64) -> bool {
        let Some(id) = self.ui.as_ref().and_then(|u| u.routes.editing) else {
            return false;
        };
        let (lat, lon) = crate::render::projection::Projection::to_wgs84(x, y);
        self.ensure_route_store();
        let Some(store) = self.route_store.as_mut() else { return false };
        if store.route(id).is_none() {
            // The route went away under the editor; drop the mode rather
            // than swallowing taps forever.
            if let Some(ref mut ui) = self.ui {
                ui.routes.editing = None;
            }
            return false;
        }
        // One past the highest "WP n" already on the route, not the count:
        // after a deletion the count would hand out a name already in use.
        let n = store
            .route(id)
            .map(|r| {
                r.waypoints
                    .iter()
                    .filter_map(|w| store.waypoints.get(*w))
                    .filter_map(|w| w.name.strip_prefix("WP ")?.parse::<usize>().ok())
                    .max()
                    .unwrap_or(0)
                    .max(r.waypoints.len())
            })
            .unwrap_or(0);
        let wp = crate::nav::model::Waypoint::new(format!("WP {}", n + 1), lat, lon);
        let wp_id = wp.id;
        store.waypoints.insert(wp);
        self.edit_route(id, |route| {
            route.waypoints.push(wp_id);
            true
        });
        true
    }

    /// The lat/lon box now on screen, padded a little so a small pan does
    /// not immediately walk off the fetched field.
    fn visible_geo_box(&self) -> crate::nav::grib::GeoBox {
        let (w, h) = (self.config.width as f32, self.config.height as f32);
        let corners = [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h)];
        let points: Vec<crate::geo::LatLon> = corners
            .iter()
            .map(|&(x, y)| {
                let world = self.camera.screen_to_world(x, y);
                let (lat, lon) =
                    crate::render::projection::Projection::to_wgs84(world.x as f64, world.y as f64);
                crate::geo::LatLon::new(lat.clamp(-89.0, 89.0), lon)
            })
            .collect();
        crate::nav::grib::GeoBox::around(&points, 0.5)
    }

    /// Fetch the wind for the visible area on a worker thread.
    fn spawn_wind_fetch(&mut self) {
        let area = self.visible_geo_box();
        let hours = self
            .ui
            .as_ref()
            .map(|u| u.weather.hours.min(48))
            .unwrap_or(24);
        if let Some(ref mut ui) = self.ui {
            ui.wind.busy = true;
            ui.wind.status = "fetching…".into();
            ui.wind.source = crate::nav::grib::GribSource::NoaaGfs025.label().into();
        }
        let tx = self.route_net_sender();
        std::thread::spawn(move || {
            let _done = JobGuard(tx.clone());
            let cache = dirs::config_dir()
                .map(|d| d.join("navcore").join("grib"))
                .unwrap_or_else(|| "grib-cache".into());
            let progress_tx = tx.clone();
            let result = crate::nav::grib::GribForecast::fetch(
                crate::nav::grib::GribSource::NoaaGfs025,
                area,
                hours,
                &cache,
                move |m| {
                    let _ = progress_tx.send(RouteNetEvent::WindProgress(m));
                },
            );
            let _ = tx.send(match result {
                Ok(f) => RouteNetEvent::WindLoaded(Box::new(f)),
                Err(e) => RouteNetEvent::WindFailed(e.to_string()),
            });
        });
    }

    /// The instant every weather display on screen is showing.
    ///
    /// One reading of one cursor, used by the barbs, the current arrows and
    /// the sheet alike. Before there was a sheet each display carried its own
    /// step index, and it was possible — easy, in fact — to have the barbs on
    /// one hour and the numbers on another.
    fn weather_cursor_ms(&self) -> i64 {
        let now = chrono::Utc::now().timestamp_millis();
        match self.ui.as_ref() {
            // A closed sheet has no visible cursor, so nothing on screen says
            // which hour the overlays show — they must then show the present,
            // not the +30 h someone scrubbed to before closing it.
            Some(ui) if ui.sheet.show => ui.sheet.cursor(now),
            _ => now,
        }
    }

    /// The cursor, held inside a field's own span — or `None` when it lies
    /// well outside it. Clamping a little is honest (the steps are hours
    /// apart); clamping +72 h to a field that ends at +48 h would draw
    /// Tuesday's wind labelled as Thursday's.
    fn field_time_ms(&self, first: i64, last: i64) -> Option<i64> {
        const SLACK_MS: i64 = 3 * 3_600_000;
        let t = self.weather_cursor_ms();
        (t >= first - SLACK_MS && t <= last + SLACK_MS).then(|| t.clamp(first, last))
    }

    /// The wind field projected onto a screen grid, for the overlay.
    ///
    /// Runs before the frame takes `self.ui`, like every other projector
    /// here — inside that block the option is None and the overlay would
    /// silently draw nothing.
    fn wind_barbs(&self) -> Vec<crate::render::ui_wind::WindBarb> {
        let (Some(ui), Some(forecast)) = (self.ui.as_ref(), self.wind_forecast.as_ref()) else {
            return Vec::new();
        };
        if !ui.wind.show || ui.wind.steps.is_empty() {
            return Vec::new();
        }
        // The sheet's cursor, held inside what this field actually covers.
        // The two need not span the same hours — the GRIB reaches 48 h and
        // the point forecast further — and clamping is honest here, because
        // the barbs then simply stop moving at the edge of their own data.
        let Some((first, last)) = forecast.valid_span() else {
            return Vec::new();
        };
        let Some(time_ms) = self.field_time_ms(first, last) else {
            return Vec::new();
        };
        // One barb every ~64 points: dense enough to show a shift across the
        // screen, sparse enough that the feathers never collide. Anchored to
        // the water, so the field travels with the chart under a pan.
        let area = ui.chart_area();
        let grid = self.sample_grid(64.0, area);
        let mut barbs = Vec::new();
        for (screen, &(lat, lon)) in grid.screen.iter().zip(grid.geo.iter()) {
            // A barb whose centre is under a panel is a barb nobody sees.
            if !area.contains(egui::pos2(screen[0], screen[1])) {
                continue;
            }
            if let Some((from_deg, kt)) = forecast.sample(lat, lon, time_ms) {
                barbs.push(crate::render::ui_wind::WindBarb {
                    screen: *screen,
                    from_deg: from_deg as f32,
                    kt: kt as f32,
                });
            }
        }
        barbs
    }

    /// The wind field as a screen-space grid of speeds, for the colour wash.
    ///
    /// Coarser than a pixel and finer than the model: 26 points is close
    /// enough that the eye reads a smooth gradient once the GPU has
    /// interpolated between the corners, and sparse enough that a 1080p
    /// window costs about 3 000 samples rather than two million. The grid is
    /// anchored to the window like the barbs, so it does not crawl when a
    /// panel opens.
    fn wind_fill(&self) -> crate::render::ui_wind::WindFill {
        let empty = crate::render::ui_wind::WindFill::default();
        let (Some(ui), Some(forecast)) = (self.ui.as_ref(), self.wind_forecast.as_ref()) else {
            return empty;
        };
        if !ui.wind.fill || ui.wind.steps.is_empty() {
            return empty;
        }
        let Some((first, last)) = forecast.valid_span() else {
            return empty;
        };
        let Some(time_ms) = self.field_time_ms(first, last) else {
            return empty;
        };
        // Finer than the barbs because the GPU draws the gradient between the
        // corners, and anchored to the water for the same reason they are:
        // a wash that stands still under a pan reads as a smear on the glass.
        let grid = self.sample_grid(32.0, ui.chart_area());
        if grid.is_empty() {
            return empty;
        }
        let kt = grid
            .geo
            .iter()
            .map(|&(lat, lon)| forecast.sample(lat, lon, time_ms).map(|(_, kt)| kt as f32))
            .collect();
        crate::render::ui_wind::WindFill {
            screen: grid.screen,
            cols: grid.cols,
            rows: grid.rows,
            kt,
        }
    }

    /// Say so when the loaded field has nothing to show here, rather than
    /// drawing an empty sea and letting it read as calm.
    fn refresh_wind_coverage(&mut self, drawn: usize) {
        let loaded = self.wind_forecast.is_some();
        // Past the field's last step the overlay draws nothing on purpose;
        // say when it ends rather than blame the view.
        let ends = self
            .wind_forecast
            .as_ref()
            .and_then(|f| f.valid_span())
            .filter(|&(first, last)| self.field_time_ms(first, last).is_none())
            .map(|(_, last)| last);
        let Some(ref mut ui) = self.ui else { return };
        if !(ui.wind.show || ui.wind.fill) || ui.wind.busy || !loaded {
            return;
        }
        let uncovered = "the wind field does not reach this view — press Here";
        let past_end = "the wind field ends at";
        if let Some(last) = ends {
            let at = chrono::DateTime::from_timestamp_millis(last)
                .map(|t| t.with_timezone(&chrono::Local).format("%a %H:%M").to_string())
                .unwrap_or_default();
            ui.wind.status = format!("{past_end} {at} — nothing to draw for the cursor's hour");
        } else if drawn == 0 && !ui.wind.steps.is_empty() {
            ui.wind.status = uncovered.into();
        } else if ui.wind.status == uncovered || ui.wind.status.starts_with(past_end) {
            ui.wind.status.clear();
        }
    }

    /// Where the point forecast should be taken: the middle of the chart.
    ///
    /// The same place "Here" fetches the wind and current fields for, so
    /// the lanes and the barbs always describe the same water. It used to be
    /// the boat whenever there was a fix, which meant panning to tomorrow's
    /// destination and pressing Here moved the barbs but left the lanes at
    /// the berth. While following, the boat *is* the middle of the chart.
    fn weather_anchor_latlon(&self) -> crate::geo::LatLon {
        let (w, h) = (self.config.width as f32, self.config.height as f32);
        let world = self.camera.screen_to_world(w / 2.0, h / 2.0);
        let (lat, lon) =
            crate::render::projection::Projection::to_wgs84(world.x as f64, world.y as f64);
        crate::geo::LatLon::new(lat.clamp(-89.0, 89.0), lon)
    }

    /// Fetch the point forecast behind the sheet on a worker thread.
    fn spawn_point_fetch(&mut self) {
        let at = self.weather_anchor_latlon();
        let days = self
            .ui
            .as_ref()
            // At least the sheet's widest span (3 days), or a third of it
            // would be blank.
            .map(|u| u.weather.hours.div_ceil(24).clamp(3, 8))
            .unwrap_or(3);
        if let Some(ref mut ui) = self.ui {
            ui.sheet.busy = true;
            ui.sheet.status = "fetching…".into();
            // The place moves only when its forecast arrives: moved now, a
            // failed fetch would leave the old lanes under the new name.
            if ui.sheet.data.is_none() {
                ui.sheet.anchor = Some([at.lat, at.lon]);
                ui.sheet.place = crate::nav::pointfx::place_label(at.lat, at.lon);
                ui.sheet.source = crate::nav::pointfx::POINT_SOURCE_LABEL.into();
            }
            ui.sheet.show = true;
        }
        let tx = self.route_net_sender();
        std::thread::spawn(move || {
            let _done = JobGuard(tx.clone());
            let _ = tx.send(
                match crate::nav::pointfx::PointForecast::fetch(at.lat, at.lon, days) {
                    Ok(f) => RouteNetEvent::PointLoaded(Box::new(f)),
                    Err(e) => RouteNetEvent::PointFailed(e),
                },
            );
        });
    }

    /// Fetch the surface current for the visible area on a worker thread.
    fn spawn_current_fetch(&mut self) {
        let area = self.visible_geo_box();
        let hours = self
            .ui
            .as_ref()
            .map(|u| u.weather.hours.min(72))
            .unwrap_or(48);
        if let Some(ref mut ui) = self.ui {
            ui.sheet.status = "fetching the set…".into();
        }
        let tx = self.route_net_sender();
        std::thread::spawn(move || {
            let _done = JobGuard(tx.clone());
            let _ = tx.send(
                match crate::nav::currents::CurrentForecast::fetch(area, hours) {
                    Ok(f) => RouteNetEvent::CurrentLoaded(Box::new(f)),
                    Err(e) => RouteNetEvent::CurrentFailed(e),
                },
            );
        });
    }

    /// The current field projected onto a screen grid, for the overlay.
    ///
    /// Coarser than the barbs on purpose. Two fields of arrows at the same
    /// spacing would interleave into a thicket, and the current is the slower
    /// story of the two: a grid every ~96 points reads as a pattern the eye
    /// can follow round a headland.
    fn current_arrows(&self) -> Vec<crate::render::ui_weather::CurrentArrow> {
        let (Some(ui), Some(field)) = (self.ui.as_ref(), self.current_forecast.as_ref()) else {
            return Vec::new();
        };
        if !ui.sheet.current_field {
            return Vec::new();
        }
        let steps = field.steps();
        let (Some(&first), Some(&last)) = (steps.first(), steps.last()) else {
            return Vec::new();
        };
        let Some(time_ms) = self.field_time_ms(first, last) else {
            return Vec::new();
        };
        // Only over the chart the reader can see, and anchored to the water
        // like the barbs: an arrow that stands still while the tide it
        // describes slides past is worse than no arrow.
        let area = ui.chart_area();
        let grid = self.sample_grid(96.0, area);
        let mut out = Vec::new();
        for (screen, &(lat, lon)) in grid.screen.iter().zip(grid.geo.iter()) {
            if !area.contains(egui::pos2(screen[0], screen[1])) {
                continue;
            }
            if let Some((to_deg, kt)) = field.sample(lat, lon, time_ms) {
                out.push(crate::render::ui_weather::CurrentArrow {
                    screen: *screen,
                    to_deg: to_deg as f32,
                    kt: kt as f32,
                });
            }
        }
        out
    }

    /// Where the sheet's point forecast was taken, in logical points.
    /// `None` when there is none, or when it has panned off the screen —
    /// a marker clamped to the edge would claim the forecast was taken there.
    fn weather_anchor_screen(&self) -> Option<[f32; 2]> {
        let ui = self.ui.as_ref()?;
        if !ui.sheet.show {
            return None;
        }
        let [lat, lon] = ui.sheet.anchor?;
        let (mx, my) = crate::render::projection::Projection::to_mercator(lat, lon);
        let s = self.camera.world_to_screen(mx, my);
        let ppp = self.scale_factor.max(0.01);
        let (x, y) = (s.x / ppp, s.y / ppp);
        let (w, h) = (
            self.config.width as f32 / ppp,
            self.config.height as f32 / ppp,
        );
        (x >= -20.0 && y >= -20.0 && x <= w + 20.0 && y <= h + 20.0).then_some([x, y])
    }

    /// Search ORC certificates on a worker thread; results drain back into
    /// the boat window.
    fn spawn_orc_search(&mut self, query: String, country: String) {
        if let Some(ref mut ui) = self.ui {
            ui.boat.busy = true;
            ui.boat.status = "searching…".into();
        }
        let tx = self.route_net_sender();
        std::thread::spawn(move || {
            let _done = JobGuard(tx.clone());
            let cache = dirs::config_dir()
                .map(|d| d.join("navcore").join("orc"))
                .unwrap_or_else(|| "orc-cache".into());
            let progress_tx = tx.clone();
            let result = crate::nav::orc::search(&country, &query, &cache, move |m| {
                let _ = progress_tx.send(RouteNetEvent::OrcProgress(m));
            });
            let _ = tx.send(match result {
                Ok(hits) => RouteNetEvent::OrcResults(hits),
                Err(e) => RouteNetEvent::OrcFailed(e),
            });
        });
    }

    /// Install a search hit: write its polar next to the settings, point
    /// the weather router at it, and let the certificate fill in whichever
    /// specs the user has not typed — the certificate knows the class, the
    /// user knows their boat, and a typed number always wins.
    fn adopt_orc_polar(&mut self, index: usize) {
        let Some(ref mut ui) = self.ui else { return };
        let Some(hit) = ui.boat.results.get(index).cloned() else { return };
        let dir = dirs::config_dir()
            .map(|d| d.join("navcore").join("polars"))
            .unwrap_or_else(|| "polars".into());
        if let Err(e) = std::fs::create_dir_all(&dir) {
            ui.boat.status = format!("could not create {}: {e}", dir.display());
            return;
        }
        let slug = crate::nav::orc::slug(&hit.class);
        let path = dir.join(format!("{slug}.pol"));
        if let Err(e) = std::fs::write(&path, &hit.pol) {
            ui.boat.status = format!("could not write the polar: {e}");
            return;
        }
        ui.weather.polar_path = path.display().to_string();
        if ui.boat.boat_type.trim().is_empty() {
            ui.boat.boat_type = hit.class.clone();
        }
        if ui.boat.loa_m == 0.0 {
            ui.boat.loa_m = hit.loa_m;
        }
        if ui.boat.beam_m == 0.0 {
            ui.boat.beam_m = hit.beam_m;
        }
        if ui.boat.draft_m == 0.0 {
            ui.boat.draft_m = hit.draft_m;
        }
        ui.boat.status = format!("polar installed: {} ({})", hit.class, path.display());
        log::info!("orc: installed polar for {} at {}", hit.class, path.display());
    }

    /// The routers' safety envelope, from the boat's own numbers where the
    /// user has entered them; the cautious defaults where not.
    fn safety_from_boat(&self) -> crate::nav::autoroute::SafetyConfig {
        let mut s = crate::nav::autoroute::SafetyConfig::default();
        if let Some(ref ui) = self.ui {
            if ui.boat.draft_m > 0.0 {
                s.draft_m = ui.boat.draft_m;
            }
            if ui.boat.air_draft_m > 0.0 {
                s.air_draft_m = ui.boat.air_draft_m;
            }
        }
        s
    }

    /// The menu bar's two fields, resolved and dispatched: an empty start
    /// means the boat, both positions must parse, and the verb picks the
    /// engine. Every refusal lands in the status line the bar shows.
    fn plan_passage(&mut self, from: &str, to: &str, sail: bool) {
        let start = if from.trim().is_empty() {
            self.fleet
                .own
                .position()
                .map(|(lat, lon)| crate::geo::LatLon::new(lat, lon))
        } else {
            crate::geo::parse_latlon(from)
        };
        let finish = crate::geo::parse_latlon(to);
        let status = |s: &mut Self, msg: &str| {
            if let Some(ref mut ui) = s.ui {
                ui.weather.status = msg.into();
            }
        };
        match (start, finish) {
            (Some(a), Some(b)) => {
                let name = format!(
                    "{} {} to {}",
                    if sail { "Sail" } else { "Motor" },
                    crate::nav::pointfx::place_label(a.lat, a.lon),
                    crate::nav::pointfx::place_label(b.lat, b.lon),
                );
                if sail {
                    self.spawn_wx_plan(a, b, name);
                } else {
                    self.spawn_motor_plan(a, b, name);
                }
            }
            (None, _) if from.trim().is_empty() => {
                status(self, "no boat position yet — tap 📍 by \"from\" and tap the chart, \
                              or type a start");
            }
            (None, _) => status(
                self,
                "can't read the start — try 56°24.6'N 10°58.8'E or 56.41, 10.98",
            ),
            (_, None) if to.trim().is_empty() => status(
                self,
                "where to? Tap 📍 by \"to\" and tap the chart, or type a position",
            ),
            (_, None) => status(
                self,
                "can't read the destination — try 56°24.6'N 10°58.8'E or 56.41, 10.98",
            ),
        }
    }

    /// Weather-route between two positions on a worker thread, with the
    /// forecast, polar and sea-state settings the Routes window holds.
    fn spawn_wx_plan(&mut self, a: crate::geo::LatLon, b: crate::geo::LatLon, name: String) {
        self.ensure_route_store();
        self.save_settings();
        let Some(dir) = self.chart_root.clone() else {
            if let Some(ref mut ui) = self.ui {
                ui.weather.status = "no chart folder loaded — choose one under Charts".into();
            }
            return;
        };
        let (hours, polar_path, use_waves, use_currents, motor_kt) = self
            .ui
            .as_ref()
            .map(|u| {
                (
                    u.weather.hours,
                    u.weather.polar_path.clone(),
                    u.weather.use_waves,
                    u.weather.use_currents,
                    u.boat.motor_kt,
                )
            })
            .unwrap_or((48, String::new(), true, true, 0.0));
        // The engine joins the plan where sailing crawls — only if the boat
        // has one worth using.
        let config = crate::nav::RoutingConfig {
            motor_speed_kt: (motor_kt > 0.0).then_some(motor_kt),
            ..Default::default()
        };
        if let Some(ref mut ui) = self.ui {
            ui.weather.busy = true;
            ui.weather.status = "starting…".into();
        }
        let safety = self.safety_from_boat();
        let tx = self.route_net_sender();
        std::thread::spawn(move || {
            let _done = JobGuard(tx.clone());
            let cache = dirs::config_dir()
                .map(|d| d.join("navcore").join("grib"))
                .unwrap_or_else(|| "grib-cache".into());
            let generation = crate::nav::isochrone::begin_cancellable();
            let (polar_text, polar_name) = if polar_path.trim().is_empty() {
                (crate::nav::wxroute::DEFAULT_POLAR.to_string(),
                 "built-in cruiser".to_string())
            } else {
                match std::fs::read_to_string(polar_path.trim()) {
                    Ok(t) => (t, polar_path.trim().to_string()),
                    Err(e) => {
                        let _ = tx.send(RouteNetEvent::WxFailed(format!(
                            "polar unreadable: {e}"
                        )));
                        return;
                    }
                }
            };
            let progress_tx = tx.clone();
            let result = crate::nav::wxroute::plan_from_chart_dir(
                &dir,
                a,
                b,
                hours,
                &cache,
                &polar_text,
                &polar_name,
                &config,
                &safety,
                use_waves,
                use_currents,
                &name,
                move |msg| {
                    if !crate::nav::isochrone::plan_cancelled(generation) {
                        let _ = progress_tx.send(RouteNetEvent::WxProgress(msg));
                    }
                },
            );
            // A cancelled plan says nothing more: the UI has moved on, and
            // may already be waiting on the next one.
            if crate::nav::isochrone::plan_cancelled(generation) {
                return;
            }
            let _ = tx.send(match result {
                Ok(p) => RouteNetEvent::WxPlanned {
                    summary: format!(
                        "{}: arrival {}{}",
                        p.route.name,
                        p.arrival.with_timezone(&chrono::Local).format("%a %d %b %H:%M"),
                        warnings_suffix(&p.warnings)
                    ),
                    route: p.route,
                    waypoints: p.waypoints,
                },
                Err(e) => RouteNetEvent::WxFailed(e),
            });
        });
    }

    /// The shortest safe route between two positions — the M3 grid router on
    /// a worker thread, same charts, no weather. What you steer under engine.
    fn spawn_motor_plan(&mut self, a: crate::geo::LatLon, b: crate::geo::LatLon, name: String) {
        self.ensure_route_store();
        let Some(dir) = self.chart_root.clone() else {
            if let Some(ref mut ui) = self.ui {
                ui.weather.status = "no chart folder loaded — choose one under Charts".into();
            }
            return;
        };
        if let Some(ref mut ui) = self.ui {
            ui.weather.busy = true;
            ui.weather.status = "planning…".into();
        }
        let safety = self.safety_from_boat();
        let tx = self.route_net_sender();
        std::thread::spawn(move || {
            let _done = JobGuard(tx.clone());
            let generation = crate::nav::isochrone::begin_cancellable();
            let progress_tx = tx.clone();
            let result = crate::nav::wxroute::motor_plan_from_chart_dir(&dir, a, b, &safety, |msg| {
                if !crate::nav::isochrone::plan_cancelled(generation) {
                    let _ = progress_tx.send(RouteNetEvent::WxProgress(msg));
                }
            });
            if crate::nav::isochrone::plan_cancelled(generation) {
                return;
            }
            let _ = tx.send(match result {
                Ok(mut p) => {
                    p.route.name = name;
                    let nm: f64 = p.route.legs.iter().map(|l| l.distance_nm).sum();
                    RouteNetEvent::WxPlanned {
                        summary: format!(
                            "{}: {:.1} NM, {} waypoints{}",
                            p.route.name,
                            nm,
                            p.route.waypoints.len(),
                            warnings_suffix(&p.warnings)
                        ),
                        route: p.route,
                        waypoints: p.waypoints,
                    }
                }
                Err(e) => RouteNetEvent::WxFailed(e),
            });
        });
    }

    /// Fold in results from route network jobs.
    fn drain_route_net(&mut self) {
        let Some(rx) = self.route_net_rx.as_ref() else { return };
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        if events.is_empty() {
            return;
        }
        for event in events {
            match event {
                RouteNetEvent::Status(msg) => {
                    if let Some(ref mut ui) = self.ui {
                        ui.routes.status = msg;
                        ui.routes.net_busy = false;
                    }
                }
                RouteNetEvent::WxProgress(msg) => {
                    if let Some(ref mut ui) = self.ui {
                        ui.weather.status = msg;
                    }
                }
                RouteNetEvent::WindProgress(msg) => {
                    if let Some(ref mut ui) = self.ui {
                        ui.wind.status = msg;
                    }
                }
                RouteNetEvent::WindFailed(msg) => {
                    if let Some(ref mut ui) = self.ui {
                        ui.wind.busy = false;
                        ui.wind.status = msg;
                    }
                }
                RouteNetEvent::WindLoaded(forecast) => {
                    let steps = forecast.steps().to_vec();
                    let run = forecast.run;
                    let source = forecast.source.label().to_string();
                    self.wind_forecast = Some(*forecast);
                    if let Some(ref mut ui) = self.ui {
                        ui.wind.busy = false;
                        ui.wind.source = source;
                        ui.wind.steps = steps;
                        ui.wind.status =
                            format!("run {}", run.format("%d %b %HZ"));
                        // Show what was fetched, but in the layer asked for:
                        // someone who wanted only the colour wash should not
                        // get barbs switched on over it.
                        if !ui.wind.fill {
                            ui.wind.show = true;
                        }
                    }
                    self.needs_redraw = true;
                }
                RouteNetEvent::CurrentFailed(msg) => {
                    if let Some(ref mut ui) = self.ui {
                        ui.sheet.status = msg;
                    }
                }
                RouteNetEvent::CurrentLoaded(field) => {
                    self.current_forecast = Some(*field);
                    if let Some(ref mut ui) = self.ui {
                        ui.sheet.current_field = true;
                        if ui.sheet.status == "fetching the set…" {
                            ui.sheet.status.clear();
                        }
                    }
                    self.needs_redraw = true;
                }
                RouteNetEvent::PointFailed(msg) => {
                    if let Some(ref mut ui) = self.ui {
                        ui.sheet.busy = false;
                        ui.sheet.status = msg;
                    }
                }
                RouteNetEvent::PointLoaded(point) => {
                    if let Some(ref mut ui) = self.ui {
                        ui.sheet.busy = false;
                        ui.sheet.anchor = Some([point.lat, point.lon]);
                        ui.sheet.place =
                            crate::nav::pointfx::place_label(point.lat, point.lon);
                        ui.sheet.source = crate::nav::pointfx::POINT_SOURCE_LABEL.into();
                        ui.sheet.status = if point.has_sea {
                            String::new()
                        } else {
                            "no sea data here — waves, set and tide are ashore".into()
                        };
                        // A new place is a new picture: hand the cursor back
                        // to the present rather than leaving it parked on an
                        // hour the reader chose for somewhere else.
                        ui.sheet.cursor_ms = None;
                        ui.sheet.window_start_ms = None;
                        ui.sheet.data = Some(point);
                        ui.sheet.show = true;
                    }
                    self.needs_redraw = true;
                }
                RouteNetEvent::OrcProgress(msg) => {
                    if let Some(ref mut ui) = self.ui {
                        ui.boat.status = msg;
                    }
                }
                RouteNetEvent::JobDone => {
                    self.route_net_jobs = self.route_net_jobs.saturating_sub(1);
                    // With nothing left running, no busy flag can still be
                    // true honestly. A worker that panicked never sent its
                    // own result, and without this the spinner would turn
                    // for the rest of the session.
                    if self.route_net_jobs == 0 {
                        if let Some(ref mut ui) = self.ui {
                            ui.routes.net_busy = false;
                            ui.wind.busy = false;
                            if ui.weather.busy {
                                ui.weather.busy = false;
                                ui.weather.status = "the planner stopped unexpectedly".into();
                            }
                            if ui.boat.busy {
                                ui.boat.busy = false;
                                ui.boat.status = "the search stopped unexpectedly".into();
                            }
                            if ui.sheet.busy {
                                ui.sheet.busy = false;
                                ui.sheet.status =
                                    "the forecast stopped unexpectedly".into();
                            }
                        }
                    }
                }
                RouteNetEvent::OrcFailed(msg) => {
                    if let Some(ref mut ui) = self.ui {
                        ui.boat.busy = false;
                        ui.boat.status = msg;
                    }
                }
                RouteNetEvent::OrcResults(hits) => {
                    if let Some(ref mut ui) = self.ui {
                        ui.boat.busy = false;
                        ui.boat.status = if hits.is_empty() {
                            "no certificates match — try fewer letters or another country".into()
                        } else {
                            format!("{} boat(s) found", hits.len())
                        };
                        ui.boat.results = hits;
                    }
                }
                RouteNetEvent::WxFailed(msg) => {
                    if let Some(ref mut ui) = self.ui {
                        ui.weather.busy = false;
                        ui.weather.status = msg;
                    }
                }
                RouteNetEvent::WxPlanned {
                    route,
                    waypoints,
                    summary,
                } => {
                    self.ensure_route_store();
                    let planned_id = route.id;
                    if let Some(store) = self.route_store.as_mut() {
                        for wp in waypoints {
                            store.waypoints.insert(wp);
                        }
                        if let Err(e) = store.upsert_route(route) {
                            log::warn!("wx: could not save: {e}");
                        }
                    }
                    if let Some(ref mut ui) = self.ui {
                        ui.weather.busy = false;
                        ui.weather.status = summary;
                        // The plan someone just waited for belongs on the
                        // chart, not behind a checkbox.
                        ui.routes.visible.insert(planned_id);
                    }
                }
                RouteNetEvent::Fetched(routes) => {
                    self.ensure_route_store();
                    // A fetched route can replace the very route being
                    // followed — publish and fetch round-trip the same id, so
                    // another client's edit arrives here and renumbers the
                    // legs underneath the follower. Same reasoning as a
                    // manual edit: hold on to the mark, not the index.
                    let steering_to = self.route_follow.as_ref().and_then(|f| {
                        let route = self.route_store.as_ref()?.route(f.route_id)?;
                        Some((f.route_id, f.marks_ahead(route)))
                    });
                    let count = routes.len();
                    let mut kept_local: Vec<String> = Vec::new();
                    if let Some(store) = self.route_store.as_mut() {
                        for (mut route, mut wps) in routes {
                            // Publish and fetch round-trip the same id, so a
                            // route edited here since it was published would
                            // be silently replaced by the server's older copy.
                            // When the two differ, keep the local one and
                            // bring the server's in beside it.
                            type Mark<'a> = (crate::geo::LatLon, &'a str);
                            let fetched: Vec<Mark> = route
                                .waypoints
                                .iter()
                                .filter_map(|id| wps.iter().find(|w| w.id == *id))
                                .map(|w| (w.position, w.name.as_str()))
                                .collect();
                            if let Some(local) = store.route(route.id) {
                                let mine: Vec<Mark> = local
                                    .waypoints
                                    .iter()
                                    .filter_map(|id| store.waypoints.get(*id))
                                    .map(|w| (w.position, w.name.as_str()))
                                    .collect();
                                let same = local.name == route.name
                                    && mine.len() == fetched.len()
                                    && mine.iter().zip(&fetched).all(|((a, an), (b, bn))| {
                                        (a.lat - b.lat).abs() < 1e-7
                                            && (a.lon - b.lon).abs() < 1e-7
                                            && an == bn
                                    });
                                if !same {
                                    kept_local.push(local.name.clone());
                                    let copy = format!("{} (Signal K)", route.name);
                                    // A later fetch refreshes the same copy
                                    // rather than stacking up new ones.
                                    route.id = store
                                        .routes()
                                        .find(|r| r.name == copy)
                                        .map(|r| r.id)
                                        .unwrap_or_else(uuid::Uuid::new_v4);
                                    route.name = copy;
                                    // Its marks come back under the local
                                    // marks' ids; inserted as they are they
                                    // would move the kept route's waypoints.
                                    for wp in wps.iter_mut() {
                                        let fresh = uuid::Uuid::new_v4();
                                        for id in route.waypoints.iter_mut() {
                                            if *id == wp.id {
                                                *id = fresh;
                                            }
                                        }
                                        wp.id = fresh;
                                    }
                                }
                            }
                            for wp in wps {
                                store.waypoints.insert(wp);
                            }
                            if let Err(e) = store.upsert_route(route) {
                                if let Some(ref mut ui) = self.ui {
                                    ui.routes.status = format!("Could not save: {e}");
                                }
                            }
                        }
                    }
                    if let Some(ref mut ui) = self.ui {
                        ui.routes.net_busy = false;
                        ui.routes.status = if kept_local.is_empty() {
                            format!("Fetched {count} route(s) from Signal K")
                        } else {
                            format!(
                                "Fetched {count} route(s). Kept your changes to {}; \
                                 the server's version is beside it as \"… (Signal K)\".",
                                kept_local.join(", ")
                            )
                        };
                    }
                    if let Some((id, ahead)) = steering_to {
                        let route =
                            self.route_store.as_ref().and_then(|s| s.route(id)).cloned();
                        if let (Some(follow), Some(route)) =
                            (self.route_follow.as_mut(), route)
                        {
                            follow.retarget(&route, &ahead);
                        }
                    }
                }
            }
        }
        self.refresh_route_rows();
        self.needs_redraw = true;
    }

    /// Advance the follower with the latest fix and feed strip and pilot.
    fn update_route_following(&mut self) {
        // Compute everything with short borrows, then write to the UI.
        let update = {
            let (Some(follow), Some(store)) =
                (self.route_follow.as_mut(), self.route_store.as_ref())
            else {
                self.clear_guidance();
                return;
            };
            let Some(route) = store.route(follow.route_id) else {
                self.clear_guidance();
                return;
            };
            let name = route.name.clone();
            if route.legs.is_empty() {
                self.guidance_waiting(Some(format!(
                    "Following {name} — it needs at least two waypoints"
                )));
                return;
            }
            let fresh = self
                .fleet
                .own
                .get("navigation.position")
                .is_some_and(|r| !r.is_stale());
            let Some((lat, lon)) = self.fleet.own.position().filter(|_| fresh) else {
                // No fix, or a fix gone stale: the last guidance is not
                // guidance any more, it is a photograph of one.
                self.guidance_waiting(Some(format!(
                    "Following {name} — waiting for a position fix"
                )));
                return;
            };
            // Signal K is SI: m/s and radians. Guidance speaks knots/degrees.
            let sog_kt = self
                .fleet
                .own
                .number("navigation.speedOverGround")
                .map(|v| v * 3600.0 / 1852.0);
            let cog_deg = self
                .fleet
                .own
                .number("navigation.courseOverGroundTrue")
                .map(f64::to_degrees);
            follow
                .update(
                    route,
                    &store.waypoints,
                    crate::geo::LatLon::new(lat, lon),
                    sog_kt,
                    cog_deg,
                )
        };
        let Some(guidance) = update else {
            // Nothing left to steer to — the route lost its legs under us.
            self.clear_guidance();
            return;
        };

        if let Some(ref mut ui) = self.ui {
            if ui.routes.guidance.as_ref() != Some(&guidance) {
                self.needs_redraw = true;
            }
            ui.routes.guidance = Some(guidance);
            ui.routes.guidance_waiting = None;
        }
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
        if let Some(ref mut ui) = self.ui {
            ui.shop.warning = if fingerprint.is_none() {
                "No chart licence found on this machine; charts can be listed but not \
                 assigned here."
                    .into()
            } else {
                String::new()
            };
        }
        self.shop_send(crate::shop::service::Request::SignIn {
            email,
            password,
            fingerprint,
        });
    }

    /// Drain the shop worker into the panel.
    fn poll_shop(&mut self) {
        let mut reload_after_install = false;
        self.poll_shop_events(&mut reload_after_install);
        // The new cells are drawn now, not after a restart; the view stays
        // where the user left it.
        if reload_after_install {
            self.refresh_installed();
            if let Some(dir) = self.chart_root.clone() {
                if self.chart_load.is_none() {
                    self.start_chart_folder_load(dir, true);
                }
            }
        }
    }

    fn poll_shop_events(&mut self, reload_after_install: &mut bool) {
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
                    ui.shop.installed = match_installed(&charts, &root);
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
                    ui.shop.status = "Installed — loading the new charts…".into();
                    *reload_after_install = true;
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

    /// Is a worker (weather plan, motor plan, polar search, a Signal K
    /// exchange) still at it? The capture harness holds the shot — and the
    /// exit — for this, and the event loop keeps polling for it: nothing
    /// else would wake the app to collect a worker's answer, so without it a
    /// fetched route can sit in the channel until the user happens to move
    /// the mouse.
    /// A Signal K stream is wanted. Its deltas are only read during a frame,
    /// so while one is open something has to keep frames coming even when
    /// nobody touches the screen — otherwise an unattended plotter shows the
    /// boat, the traffic and the instruments as they were when last touched.
    pub fn signalk_live(&self) -> bool {
        self.signalk.is_some() && self.ui.as_ref().is_some_and(|u| u.instruments.active)
    }

    pub fn routing_busy(&self) -> bool {
        self.route_net_jobs > 0
            || self.chart_load.is_some()
            || self.free_jobs > 0
            || self
                .ui
                .as_ref()
                .map(|u| u.weather.busy || u.boat.busy)
                .unwrap_or(false)
    }

    /// Mark that a redraw is needed (called on camera move, etc.)
    pub fn mark_dirty(&mut self) {
        self.needs_redraw = true;
    }

    /// The user has taken the chart in hand: stop keeping the boat centred.
    ///
    /// Every plotter behaves this way, and the reason is what happens
    /// otherwise — the camera is re-centred on the fix every time one
    /// arrives, so a drag is undone before the hand has left the screen and
    /// the chart simply refuses to move. "Keep the boat centred" is then a
    /// mode with no way out except finding the checkbox that turns it off.
    pub fn release_follow(&mut self) {
        let released = self
            .ui
            .as_mut()
            .map(|u| std::mem::replace(&mut u.instruments.follow, false))
            .unwrap_or(false);
        if released {
            self.following = false;
            self.save_settings();
            log::debug!("follow released: the chart was panned by hand");
        }
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
            log::warn!("NAVCORE_SHOT: failed to map readback buffer");
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
                    log::warn!("NAVCORE_SHOT: wrote {} ({}x{})", path, width, height);
                    if let (Ok(dump), Some(log)) =
                        (std::env::var("NAVCORE_DUMP_DRAW"), &self.draw_log)
                    {
                        let lines: Vec<String> = log
                            .borrow()
                            .iter()
                            .map(|v| v.to_string())
                            .collect();
                        if std::fs::write(&dump, lines.join("\n") + "\n").is_ok() {
                            log::warn!(
                                "NAVCORE_DUMP_DRAW: wrote {} draw records to {}",
                                lines.len(),
                                dump
                            );
                        }
                    }
                }
                Err(e) => log::warn!("NAVCORE_SHOT: save failed: {}", e),
            },
            None => log::warn!("NAVCORE_SHOT: bad capture buffer dimensions"),
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
/// Where in the route a waypoint action means, given both the position the
/// UI saw and the mark's identity.
///
/// The position wins when it still holds that mark — a route may legally
/// visit the same mark twice, and then only the position distinguishes them.
/// Otherwise an earlier action in the same batch has renumbered the list, and
/// the identity is what survives that. `None` when the mark has gone
/// altogether, which is the correct no-op.
fn resolve_waypoint(
    route: &crate::nav::Route,
    index: usize,
    waypoint: uuid::Uuid,
) -> Option<usize> {
    if route.waypoints.get(index) == Some(&waypoint) {
        return Some(index);
    }
    route.waypoints.iter().position(|w| *w == waypoint)
}

/// A name for a new route that no existing route already answers to.
/// "Route 1", then "Route 2" — a plotter full of identical "New route"s is
/// a plotter you cannot navigate by name.
fn unique_route_name(store: Option<&crate::nav::RouteStore>) -> String {
    let Some(store) = store else { return "Route 1".into() };
    let taken: std::collections::HashSet<&str> =
        store.routes().map(|r| r.name.as_str()).collect();
    (1..)
        .map(|n| format!("Route {n}"))
        .find(|name| !taken.contains(name.as_str()))
        .unwrap_or_else(|| "Route".into())
}

fn tile_meets_footprint(b: &crate::tiles::TileBounds, quad: &[glam::DVec2; 4]) -> bool {
    let rect = [
        glam::DVec2::new(b.min_x, b.min_y),
        glam::DVec2::new(b.max_x, b.min_y),
        glam::DVec2::new(b.max_x, b.max_y),
        glam::DVec2::new(b.min_x, b.max_y),
    ];
    let axes = |p: &[glam::DVec2; 4]| {
        let mut out = [glam::DVec2::ZERO; 4];
        for i in 0..4 {
            let e = p[(i + 1) % 4] - p[i];
            out[i] = glam::DVec2::new(-e.y, e.x);
        }
        out
    };
    for axis in axes(&rect).into_iter().chain(axes(quad)) {
        if axis.length_squared() < 1e-12 {
            continue;
        }
        let proj = |p: &[glam::DVec2; 4]| {
            let mut lo = f64::MAX;
            let mut hi = f64::MIN;
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

    fn quad(pts: [(f64, f64); 4]) -> [glam::DVec2; 4] {
        pts.map(|(x, y)| glam::DVec2::new(x, y))
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

#[cfg(test)]
mod sample_grid_tests {
    use super::*;

    fn camera_at(cx: f64, cy: f64, zoom: f32) -> Camera {
        Camera::new(cx, cy, zoom, 1400.0, 900.0)
    }

    fn area() -> egui::Rect {
        egui::Rect::from_min_max(egui::pos2(0.0, 40.0), egui::pos2(1400.0, 800.0))
    }

    /// The bug this replaced: the samples used to step from the window's own
    /// corner, so the whole field stood still while the chart slid under it.
    /// Pan the camera and every sample must travel with the chart, by exactly
    /// the distance the chart moved.
    #[test]
    fn the_field_travels_with_the_chart_under_a_pan() {
        let zoom = 50.0;
        let a = sample_grid_for(&camera_at(1_380_000.0, 7_540_000.0, zoom), 1.0, 64.0, area());
        // Shift the camera by a whole number of world units.
        let shift_world = 500.0f64;
        let b = sample_grid_for(
            &camera_at(1_380_000.0 + shift_world, 7_540_000.0, zoom),
            1.0,
            64.0,
            area(),
        );
        assert!(!a.is_empty() && !b.is_empty());
        // Moving the camera east moves the chart west on screen, and the
        // lattice with it.
        let expected = -(shift_world as f32) / zoom;

        // Find a sample in `b` that is the same point of the earth as a
        // sample in `a`, and check where it landed.
        let (i, j) = a
            .geo
            .iter()
            .enumerate()
            .find_map(|(i, ga)| {
                b.geo
                    .iter()
                    .position(|gb| (gb.0 - ga.0).abs() < 1e-9 && (gb.1 - ga.1).abs() < 1e-9)
                    .map(|j| (i, j))
            })
            .expect("the two lattices share no point of the earth");
        let moved = b.screen[j][0] - a.screen[i][0];
        assert!(
            (moved - expected).abs() < 0.01,
            "the same point of sea moved {moved} points, expected {expected}"
        );
        assert!(
            (b.screen[j][1] - a.screen[i][1]).abs() < 0.01,
            "a pure east-west pan moved the field vertically"
        );
    }

    /// Zoom must not re-cut the lattice on every wheel click: the spacing is
    /// quantised to octaves, so a small change of zoom keeps the same points
    /// of sea and only the gaps between them grow.
    #[test]
    fn the_lattice_holds_its_ground_through_a_small_zoom() {
        let here = (1_380_000.0, 7_540_000.0);
        let a = sample_grid_for(&camera_at(here.0, here.1, 50.0), 1.0, 64.0, area());
        let b = sample_grid_for(&camera_at(here.0, here.1, 55.0), 1.0, 64.0, area());
        // The same latitudes are sampled, because the world step did not change.
        let shared = a
            .geo
            .iter()
            .filter(|ga| {
                b.geo
                    .iter()
                    .any(|gb| (gb.0 - ga.0).abs() < 1e-9 && (gb.1 - ga.1).abs() < 1e-9)
            })
            .count();
        assert!(
            shared > a.geo.len() / 2,
            "only {shared} of {} points survived a 10% zoom",
            a.geo.len()
        );
    }

    /// Spacing is honoured to within the octave rounding, so the barbs stay
    /// far enough apart for their feathers not to collide.
    #[test]
    fn the_samples_land_about_as_far_apart_as_asked() {
        let g = sample_grid_for(&camera_at(1_380_000.0, 7_540_000.0, 50.0), 1.0, 64.0, area());
        let gap = (g.screen[1][0] - g.screen[0][0]).abs();
        assert!(
            (45.0..=91.0).contains(&gap),
            "asked for 64 points between samples, got {gap}"
        );
    }

    /// The lattice's size tracks the window, not the zoom: the step is
    /// derived from the same zoom that sets the extent, so the two cancel and
    /// the count stays near `area / spacing` at every scale. Worth pinning,
    /// because it is the reason this can be rebuilt every frame at all.
    #[test]
    fn the_cost_does_not_run_away_with_the_zoom() {
        for zoom in [0.05f32, 5.0, 500.0, 50_000.0, 5_000_000.0] {
            let g = sample_grid_for(&camera_at(1_380_000.0, 7_540_000.0, zoom), 1.0, 64.0, area());
            assert!(!g.is_empty(), "no lattice at {zoom} m/px");
            let n = g.cols * g.rows;
            assert!(n < 2_000, "{n} samples at {zoom} m/px");
        }
    }

    /// A camera that cannot answer must produce nothing rather than a lattice
    /// of NaNs, which would take the whole draw call down with it.
    #[test]
    fn a_broken_camera_is_refused_rather_than_drawn() {
        // A viewport of no size, before the first resize has arrived.
        let g = sample_grid_for(
            &camera_at(0.0, 0.0, 50.0),
            1.0,
            64.0,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(0.0, 0.0)),
        );
        assert!(g.is_empty());
        // A zoom of nothing, and a zoom that is not a number.
        assert!(sample_grid_for(&camera_at(0.0, 0.0, 0.0), 1.0, 64.0, area()).is_empty());
        assert!(sample_grid_for(&camera_at(0.0, 0.0, f32::NAN), 1.0, 64.0, area()).is_empty());
        // Every point of a good lattice is a real number.
        let g = sample_grid_for(&camera_at(1_380_000.0, 7_540_000.0, 50.0), 1.0, 64.0, area());
        assert!(g
            .screen
            .iter()
            .all(|p| p[0].is_finite() && p[1].is_finite()));
        assert!(g.geo.iter().all(|(la, lo)| la.is_finite() && lo.is_finite()));
    }
}

/// The planner's warnings, spelled out after its summary. "(2 warnings)" on
/// its own hid the ones that matter most — a passage that outruns the
/// forecast looks exactly as trustworthy as one that does not.
fn warnings_suffix(warnings: &[String]) -> String {
    if warnings.is_empty() {
        String::new()
    } else {
        format!(" — warning: {}", warnings.join("; "))
    }
}

/// A chart folder being loaded off the UI thread.
struct ChartLoad {
    dir: std::path::PathBuf,
    keep_view: bool,
    rx: std::sync::mpsc::Receiver<Result<(ChartCatalog, Arc<KeyStore>, CachedDecryptor), String>>,
}

/// Read a folder's key lists and build its catalogue. The slow part of
/// opening charts, run on a worker thread by
/// [`RenderState::start_chart_folder_load`].
fn load_chart_folder(
    dir: &std::path::Path,
) -> Result<(ChartCatalog, Arc<KeyStore>, CachedDecryptor), String> {
    let mut keys = KeyStore::new();
    if let Err(e) = keys.load_keylists_in_dir(dir) {
        // Not fatal on its own: the keys may sit beside the cells under
        // another name, and the catalogue will say so cell by cell.
        log::warn!("chart folder {}: no key list read: {e}", dir.display());
    }
    let mut decryptor = CachedDecryptor::open("license");
    let keys = Arc::new(keys);
    let catalog =
        ChartCatalog::from_directory(dir, &keys, &mut decryptor).map_err(|e| format!("{e}"))?;
    Ok((catalog, keys, decryptor))
}

/// Key each listed chart to the edition on disk under `root`. Without this
/// the panel cannot say which edition is held, and a lapsed subscription has
/// nothing to ask the shop for except the current edition — which is the
/// one it will not grant.
fn match_installed(
    charts: &[crate::shop::types::Chart],
    root: &std::path::Path,
) -> HashMap<String, crate::shop::types::Edition> {
    let on_disk = crate::shop::installed::scan(root);
    let mut installed: HashMap<_, _> = charts
        .iter()
        .filter_map(|c| {
            let stem = c.set_stem()?;
            let edition = crate::shop::installed::newest_for(&on_disk, &stem)?;
            Some((c.id.clone(), edition))
        })
        .collect();
    // The stem comes from the shop's ChartList links, and not every reply
    // carries them. One chart set owned and one installed is unambiguous
    // without any key at all — and it is much the commonest case, one
    // region per boat.
    if installed.is_empty() && charts.len() == 1 && on_disk.len() == 1 {
        installed.insert(charts[0].id.clone(), on_disk[0].edition);
    }
    log::info!(
        "chart shop: {} set(s) on disk, {} matched to the listing",
        on_disk.len(),
        installed.len()
    );
    installed
}

/// What the free-chart (NOAA) workers report.
enum FreeEvent {
    /// NOAA's size and date for a package, or why it could not say.
    Probed(&'static str, Result<crate::shop::noaa::Remote, String>),
    /// Bytes so far and total, for the package downloading.
    Progress(String, u64, u64),
    /// A package is in place under this chart folder.
    Installed(String, std::path::PathBuf, crate::shop::noaa::Installed),
    /// A download failed or was cancelled ("cancelled").
    Failed(String, String),
    /// A worker has finished.
    Done,
}
