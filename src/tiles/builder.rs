//! Tile builder - clips geometry to tile bounds for rendering.
//!
//! Builds TilePackets (CPU-side) from chart data, ready for GPU upload.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use bytemuck::{Pod, Zeroable};

use crate::cache::CachedDecryptor;
use crate::decrypt::KeyStore;
use crate::render::symbols::SymbolInstance;
use crate::render::text::SoundingInstance;
use crate::render::{build_line_vertices_multi_indexed, LineVertex};
use crate::render::{HJust, TextParams, VJust};
use crate::s52::{
    depare02_color_token, depcnt02_selected, light_render_info, light_sector_info, litdsn01, sndfrm02,
    DisplayCategory, GeometryType, LinePattern, LineStyleKey, LineStyleTable, RenderInstruction,
    S52Engine,
};

use super::clip::{clip_polyline, clip_polyline_arc, clip_triangle, outcode, triangulate_fan};
use super::{LineBatchKey, TileBounds, TileId};
use crate::senc::BBoxTileRelation;
use crate::senc::{ChartCatalog, ChartData, ChartInfo, Feature, ObjectClass, SencError, s57_code_to_acronym};

// ============================================================
// Shared S-52 Line Lookup Table
// ============================================================

/// Path to S-52 chartsymbols.xml for line lookups
const CHARTSYMBOLS_PATH: &str = "assets/s52/chartsymbols.xml";

/// Shared line style table for LC() symbols, loaded once on first access
static LINE_STYLE_TABLE: OnceLock<Option<LineStyleTable>> = OnceLock::new();

/// Get the shared line style table for LC() symbols, loading it on first access.
/// Returns None if the table could not be loaded (file missing, parse error).
pub(crate) fn get_line_style_table() -> Option<&'static LineStyleTable> {
    LINE_STYLE_TABLE
        .get_or_init(|| match LineStyleTable::load_from_xml(CHARTSYMBOLS_PATH) {
            Ok(table) => {
                log::info!("Loaded S-52 line style table: {} LC symbols", table.len());
                Some(table)
            }
            Err(e) => {
                log::warn!("Failed to load S-52 line style table: {}", e);
                None
            }
        })
        .as_ref()
}

/// LC() lines of a tile, by batch: each piece of line with the arc length
/// (metres) at which it starts along its whole feature line, so the symbol
/// the line shader repeats along it continues across tile boundaries.
type LcPolylines = HashMap<LineBatchKey, Vec<(Vec<[f32; 2]>, f32)>>;

/// Check if a feature should be rendered at the given view scale.
///
/// Uses SCAMIN/SCAMAX attributes from S-57 to filter features.
/// Returns a scale factor (0.0 to 1.0) for soft-SCAMIN scaling.
/// If `bypass_scamin` is true, SCAMIN check is skipped (for DISPLAYBASE/GROUP1 features).
/// If `is_point_symbol` is true, applies soft SCAMIN (gradual fade up to 2x scamin).
/// `chart_native_scale` enables SUPER_SCAMIN: features without a SCAMIN attribute
/// are hidden when view_scale exceeds chart_scale * 2 (matching OpenCPN behavior).
pub fn should_render_at_scale_ex(
    feature: &Feature,
    view_scale: f64,
    bypass_scamin: bool,
    is_point_symbol: bool,
    chart_native_scale: Option<u32>,
) -> f32 {
    // SCAMAX check (always binary cut-off)
    if let Some(scamax) = feature.scamax() {
        if view_scale < scamax {
            return 0.0;
        }
    }

    if bypass_scamin {
        return 1.0;
    }

    if let Some(scamin) = feature.scamin() {
        if view_scale <= scamin {
            return 1.0;
        }

        if is_point_symbol {
            // Soft SCAMIN: gradually shrink the symbol between scamin and scamin * 2.0
            let fade_limit = scamin * 2.0;
            if view_scale > fade_limit {
                return 0.0;
            }

            // Interpolate scale from 1.0 down to 0.5
            let t = (view_scale - scamin) / (fade_limit - scamin);
            return (1.0 - (0.5 * t)) as f32;
        } else {
            // Binary cut-off for lines/areas
            return 0.0;
        }
    }

    // SUPER_SCAMIN: a feature whose cell declares no SCAMIN gets a synthetic one
    // derived from the cell's compilation scale, so overview-chart clutter does
    // not survive into detailed zooms.
    //
    // s52plib.cpp gates this on an exemption list and uses a factor of 4. The
    // exemptions are the classes that describe the water and the land itself —
    // applying the rule to them empties the chart of exactly the features a
    // mariner needs most. navcore applied it to everything at factor 2, which
    // is what erased the dredged basins inside Brondby Havn: the harbour cell is
    // 1:4000, its DRGAREs carry no SCAMIN, and 2 x 4000 is a closer zoom than
    // the harbour is normally viewed at.
    if let Some(chart_scale) = chart_native_scale {
        if !super_scamin_exempt(feature) {
            let super_scamin = chart_scale as f64 * 4.0;
            if view_scale > super_scamin {
                return 0.0;
            }
        }
    }

    1.0
}

/// Classes exempt from SUPER_SCAMIN, per the name test in s52plib.cpp.
///
/// LNDARE is exempt only when it is drawn as a filled area; a LNDARE whose
/// portrayal is not an area colour is treated like any other object.
fn super_scamin_exempt(feature: &Feature) -> bool {
    let name = crate::senc::s57_code_to_acronym(feature.type_code);
    matches!(
        name,
        "LNDARE" | "DEPARE" | "SWPARE" | "RECTRK" | "TSEZNE" | "DRGARE" | "COALNE"
    ) || name.starts_with("TSS")
}

/// Error type for tile building
#[derive(Debug)]
pub enum BuildError {
    Decrypt(crate::decrypt::DecryptError),
    Parse(SencError),
    NoKey(String),
    /// An S-57 cell that could not be read or converted.
    S57(String),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::Decrypt(e) => write!(f, "Decrypt error: {}", e),
            BuildError::Parse(e) => write!(f, "Parse error: {:?}", e),
            BuildError::NoKey(name) => write!(f, "No key for chart: {}", name),
            BuildError::S57(e) => write!(f, "S-57: {}", e),
        }
    }
}

impl std::error::Error for BuildError {}

/// A vertex for area rendering (position + color index into palette)
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct AreaVertex {
    /// Metres from the tile centre (see [`tile_relative`]).
    pub position: [f32; 2],
    pub color_index: u32,
    pub disp_prio: u32,
    /// How much to darken this vertex's palette colour, 0..1.
    ///
    /// Zero for every ordinary fill vertex. The depth-relief option emits an
    /// extra band of triangles just inside each depth area's boundary, shaded
    /// at the edge and fading to nothing inward, which reads as a terraced
    /// seabed. It rides in the same vertex stream as the fills because the area
    /// pipeline is opaque and depth-tested: a band drawn after its own fill
    /// simply overwrites it, so overlapping bands at concave corners cannot
    /// compound into a dark blotch the way alpha blending would.
    pub shade: f32,
}

/// A batch of line vertices for a single style
#[derive(Debug)]
pub struct LineBatch {
    /// Batch key (pass order + S-52 style)
    pub key: LineBatchKey,
    pub vertices: Vec<LineVertex>,
    pub indices: Vec<u32>,
}

/// An LC() line's segments, drawn by the line shader's LC entry points.
#[derive(Debug)]
pub struct LcBatch {
    /// Batch key; `key.style.lc` names the symbol
    pub key: LineBatchKey,
    pub segments: Vec<crate::render::LcSegment>,
}

/// CPU-side tile data ready for GPU upload
#[derive(Debug)]
pub struct TilePacket {
    pub tile_id: TileId,
    /// Clipped area triangles (sorted by priority)
    pub area_vertices: Vec<AreaVertex>,
    /// Vertex offsets per priority level (index i = start of priority i, index 10 = total)
    pub area_priority_offsets: [u32; 11],
    /// Line batches by style (multi-class rendering)
    pub line_batches: Vec<LineBatch>,
    /// LC() lines, by style
    pub lc_batches: Vec<LcBatch>,
    /// Point symbol instances (sorted by priority)
    pub symbol_instances: Vec<SymbolInstance>,
    /// Instance offsets per priority level
    pub symbol_priority_offsets: [u32; 11],
    /// Sounding instances (numeric text)
    pub text_instances: Vec<SoundingInstance>,
    /// Label candidates (TextParams) stored on CPU for global layout and decluttering
    pub label_candidates: Vec<crate::render::TextParams>,
    /// Pattern-filled area vertices (sorted by priority)
    pub pattern_vertices: Vec<crate::render::PatternVertex>,
    /// Pattern vertex offsets per priority level
    pub pattern_priority_offsets: [u32; 11],
    /// Estimated byte size for cache budgeting
    pub byte_size: usize,
    /// Background area vertices — from every chart in the tile coarser than
    /// its finest. Drawn before the foreground so the finer chart wins.
    pub bg_area_vertices: Vec<AreaVertex>,
    /// Background area vertex offsets per priority level
    pub bg_area_priority_offsets: [u32; 11],
    /// Background pattern vertices (from charts covered by a more-detailed chart)
    pub bg_pattern_vertices: Vec<crate::render::PatternVertex>,
    /// Background pattern vertex offsets per priority level
    pub bg_pattern_priority_offsets: [u32; 11],
    /// Light sector arcs and legs, sized in pixels (sorted by priority)
    pub sector_instances: Vec<crate::render::SectorInstance>,
    /// Sector instance offsets per priority level
    pub sector_priority_offsets: [u32; 11],
}

impl TilePacket {
    pub fn new(tile_id: TileId) -> Self {
        Self {
            tile_id,
            area_vertices: Vec::new(),
            area_priority_offsets: [0; 11],
            line_batches: Vec::new(),
            lc_batches: Vec::new(),
            symbol_instances: Vec::new(),
            symbol_priority_offsets: [0; 11],
            text_instances: Vec::new(),
            label_candidates: Vec::new(),
            pattern_vertices: Vec::new(),
            pattern_priority_offsets: [0; 11],
            byte_size: 0,
            bg_area_vertices: Vec::new(),
            bg_area_priority_offsets: [0; 11],
            bg_pattern_vertices: Vec::new(),
            bg_pattern_priority_offsets: [0; 11],
            sector_instances: Vec::new(),
            sector_priority_offsets: [0; 11],
        }
    }

    /// Compute byte size for cache budgeting
    pub fn compute_byte_size(&mut self) {
        let line_bytes: usize = self
            .line_batches
            .iter()
            .map(|b| {
                b.vertices.len() * std::mem::size_of::<LineVertex>()
                    + b.indices.len() * std::mem::size_of::<u32>()
            })
            .sum();
        let symbol_bytes = self.symbol_instances.len() * std::mem::size_of::<SymbolInstance>();
        let text_bytes = self.text_instances.len() * std::mem::size_of::<SoundingInstance>();
        let label_bytes =
            self.label_candidates.len() * std::mem::size_of::<crate::render::TextParams>();
        let pattern_bytes =
            self.pattern_vertices.len() * std::mem::size_of::<crate::render::PatternVertex>();
        let bg_area_bytes = self.bg_area_vertices.len() * std::mem::size_of::<AreaVertex>();
        let bg_pattern_bytes =
            self.bg_pattern_vertices.len() * std::mem::size_of::<crate::render::PatternVertex>();
        let sector_bytes =
            self.sector_instances.len() * std::mem::size_of::<crate::render::SectorInstance>();
        let lc_bytes: usize = self
            .lc_batches
            .iter()
            .map(|b| b.segments.len() * std::mem::size_of::<crate::render::LcSegment>())
            .sum();
        self.byte_size = self.area_vertices.len() * std::mem::size_of::<AreaVertex>()
            + sector_bytes
            + lc_bytes
            + line_bytes
            + symbol_bytes
            + text_bytes
            + label_bytes
            + pattern_bytes
            + bg_area_bytes
            + bg_pattern_bytes;
    }

    /// Whether the packet draws nothing at all.
    ///
    /// Every buffer counts, the background ones included. A tile whose only
    /// content is a coarser chart's land (the finer chart in it charting
    /// nothing there) has all of it in `bg_area_vertices`; leaving those out
    /// marked the tile known-empty, and it showed as a tile-sized square of
    /// sea cut into the land.
    pub fn is_empty(&self) -> bool {
        self.area_vertices.is_empty()
            && self.bg_area_vertices.is_empty()
            && self.pattern_vertices.is_empty()
            && self.bg_pattern_vertices.is_empty()
            && self.line_batches.is_empty()
            && self.symbol_instances.is_empty()
            && self.text_instances.is_empty()
            && self.label_candidates.is_empty()
            && self.sector_instances.is_empty()
            && self.lc_batches.is_empty()
    }

    /// Total line vertex count across all batches
    pub fn total_line_vertices(&self) -> usize {
        self.line_batches.iter().map(|b| b.vertices.len()).sum()
    }

    /// Total line index count across all batches
    pub fn total_line_indices(&self) -> usize {
        self.line_batches.iter().map(|b| b.indices.len()).sum()
    }
}

/// Builds tile packets from chart data with spatial prefiltering and clipping.
pub struct TileBuilder<'a> {
    catalog: &'a ChartCatalog,
    chart_cache: &'a ChartCache,
    /// M_COVR tessellation per chart. Global Mercator, so it is the same for
    /// every tile; shared with the other builders because one is constructed
    /// per tile, and recomputing this per tile cost more than the tile did.
    coverage_cache: &'a CoverageCache,
    keys: &'a KeyStore,
    decryptor: &'a Mutex<CachedDecryptor>,
    /// S-52 presentation engine for display filtering
    s52_engine: Option<&'a S52Engine>,
    /// View meters-per-pixel at current zoom (used for SCAMIN/SCAMAX and LC sizing)
    view_meters_per_pixel: f32,
    /// View pixels-per-millimeter at current DPI/scale factor (used for SCAMIN/SCAMAX and LC sizing)
    view_ppmm: f32,
    /// Viewport width in pixels
    view_width_px: f32,
    /// Viewport height in pixels
    view_height_px: f32,
    /// Camera center in global Mercator meters
    view_center_x: f32,
    /// Camera center in global Mercator meters
    view_center_y: f32,
    /// Provenance log for `navcore --dump-scene`. `None` in normal rendering,
    /// so the instrumentation costs nothing.
    pub scene_log: Option<std::sync::Mutex<super::scene::SceneLog>>,
}

fn instruction_summary(instructions: &[RenderInstruction]) -> String {
    let mut parts = Vec::new();
    for instr in instructions.iter().take(4) {
        let part = match instr {
            RenderInstruction::LineStyle { pattern, width, color } => {
                format!("LS({pattern:?},{width},{color})")
            }
            RenderInstruction::AreaColor { color, .. } => format!("AC({color})"),
            RenderInstruction::AreaPattern { pattern } => format!("AP({pattern})"),
            RenderInstruction::Symbol { name, .. } => format!("SY({name})"),
            RenderInstruction::Text { attribute, .. } => format!("TX({attribute})"),
            RenderInstruction::LineComplex { name } => format!("LC({name})"),
            RenderInstruction::ConditionalSymbology { procedure } => format!("CS({procedure})"),
        };
        parts.push(part);
    }
    if instructions.len() > 4 {
        parts.push("...".to_string());
    }
    parts.join(";")
}

struct LoadedChartContext<'a> {
    chart: Arc<ChartData>,
    info: &'a ChartInfo,
    ref_mx: f64,
    ref_my: f64,
    coverage_triangles: Arc<Vec<[[f64; 2]; 3]>>,
}

/// Coverage tessellations keyed by chart id, shared across tile builders.
pub type CoverageCache = Mutex<HashMap<u64, Arc<Vec<[[f64; 2]; 3]>>>>;

/// Parsed charts, one slot per chart id.
///
/// Two locks, deliberately. The outer one guards the map and is held only long
/// enough to find the slot; the inner one guards *that chart's* load. So two
/// tiles wanting the same chart queue behind each other, while tiles wanting
/// different charts still parse in parallel.
///
/// A single `HashMap<u64, Arc<ChartData>>` cannot do this: checking it, then
/// dropping the lock to parse, then inserting, is check-then-act, and eight
/// rayon threads starting a screenful of tiles together all miss and all parse.
/// One 3802-feature cell was being parsed six times per view — and because the
/// decryptor is behind a single global mutex, those redundant reads serialised
/// and blocked tiles that needed other charts.
pub type ChartCache = Mutex<HashMap<u64, Arc<Mutex<Option<Arc<ChartData>>>>>>;

impl TileBuilder<'_> {
    /// Smallest triangle worth sending to the GPU, in square metres at this
    /// tile's own scale.
    ///
    /// The SENC's tessellation is compiled at the cell's scale, so a chart
    /// drawn well outside that scale carries a great many triangles smaller
    /// than a pixel. Over Zealand at 1:700000 a third of every triangle in the
    /// view fell under one pixel and together they painted 0.8% of it — a third
    /// of the vertex budget for nothing anyone can see. Dropping them is the
    /// zoom LOD: at the scale a cell was made for the test never fires, and the
    /// further out you zoom the more it removes.
    ///
    /// Half a pixel, not a whole one: the gap a dropped sliver leaves shows the
    /// colour beneath, and at half a pixel with 4x multisampling that is a few
    /// per cent of one pixel's colour.
    fn min_triangle_area_m2(&self) -> f32 {
        const MIN_TRIANGLE_PX: f32 = 0.5;
        let mpp = self.view_meters_per_pixel;
        MIN_TRIANGLE_PX * mpp * mpp
    }

    fn below_min_triangle_area(&self, tri: &[[f32; 2]; 3]) -> bool {
        let a = ((tri[1][0] - tri[0][0]) * (tri[2][1] - tri[0][1])
            - (tri[2][0] - tri[0][0]) * (tri[1][1] - tri[0][1]))
            .abs()
            * 0.5;
        a < self.min_triangle_area_m2()
    }

    fn below_min_triangle_area_f64(&self, tri: &[[f64; 2]; 3]) -> bool {
        let a = ((tri[1][0] - tri[0][0]) * (tri[2][1] - tri[0][1])
            - (tri[2][0] - tri[0][0]) * (tri[1][1] - tri[0][1]))
            .abs()
            * 0.5;
        a < self.min_triangle_area_m2() as f64
    }

    /// Whether a chart's declared coverage reaches into `bounds`. A chart
    /// that declares none, or cannot be read here, is given the benefit of
    /// the doubt — it is dropped later if it fails to load.
    fn has_coverage_in(&self, info: &ChartInfo, bounds: &TileBounds) -> bool {
        let Ok(chart) = self.load_chart(info) else { return true };
        let (ref_mx, ref_my) = crate::tiles::latlon_to_mercator(info.ref_lat, info.ref_lon);
        let coverage = self.coverage_for(info.id, &chart, ref_mx, ref_my);
        coverage.is_empty()
            || coverage.iter().any(|t| {
                let (min_x, max_x) = (t[0][0].min(t[1][0]).min(t[2][0]), t[0][0].max(t[1][0]).max(t[2][0]));
                let (min_y, max_y) = (t[0][1].min(t[1][1]).min(t[2][1]), t[0][1].max(t[1][1]).max(t[2][1]));
                min_x <= bounds.max_x && max_x >= bounds.min_x && min_y <= bounds.max_y && max_y >= bounds.min_y
            })
    }

    /// Whether the chart's declared coverage fills `rect`.
    ///
    /// Sampled on a 17 × 17 lattice, corners included — 1/16 of the rect
    /// between samples. Wrong in the safe direction only when it errs: a
    /// sample outside the coverage keeps a coarser chart under this one,
    /// which costs overdraw, never a hole. A chart that declares no coverage
    /// is taken at its extent, as the quilt always did.
    fn coverage_fills(&self, info: &ChartInfo, rect: &TileBounds) -> bool {
        if rect.max_x <= rect.min_x || rect.max_y <= rect.min_y {
            return true;
        }
        let Ok(chart) = self.load_chart(info) else { return true };
        let (ref_mx, ref_my) = crate::tiles::latlon_to_mercator(info.ref_lat, info.ref_lon);
        let coverage = self.coverage_for(info.id, &chart, ref_mx, ref_my);
        if coverage.is_empty() {
            return true;
        }
        const N: usize = 16;
        (0..=N).all(|i| {
            let x = rect.min_x + (rect.max_x - rect.min_x) * i as f64 / N as f64;
            (0..=N).all(|j| {
                let y = rect.min_y + (rect.max_y - rect.min_y) * j as f64 / N as f64;
                point_in_any_triangle([x, y], &coverage)
            })
        })
    }

    /// The chart's coverage tessellation, computed once per chart.
    fn coverage_for(
        &self,
        chart_id: u64,
        chart: &ChartData,
        ref_mx: f64,
        ref_my: f64,
    ) -> Arc<Vec<[[f64; 2]; 3]>> {
        if let Some(hit) = self.coverage_cache.lock().unwrap().get(&chart_id) {
            return Arc::clone(hit);
        }
        let tris = Arc::new(extract_coverage_triangles(chart, ref_mx, ref_my));
        self.coverage_cache
            .lock()
            .unwrap()
            .insert(chart_id, Arc::clone(&tris));
        tris
    }
}

fn chart_debug_name(info: &ChartInfo) -> String {
    if !info.name.is_empty() {
        info.name.clone()
    } else {
        info.path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("<unknown>")
            .to_string()
    }
}

impl<'a> TileBuilder<'a> {
    /// Hard cap to keep per-tile builds bounded.
    ///
    /// Without quilt-style scale selection, some tiles can intersect 100+ charts and
    /// generate enormous vertex buffers (tens of MB), which can stall rendering.
    const MAX_CHARTS_PER_TILE: usize = 8;

    /// Create a new tile builder with its own internal cache (for standalone use)
    pub fn new(
        catalog: &'a ChartCatalog,
        keys: &'a KeyStore,
        decryptor: &'a Mutex<CachedDecryptor>,
        chart_cache: &'a ChartCache,
        coverage_cache: &'a CoverageCache,
    ) -> Self {
        Self {
            catalog,
            chart_cache,
            coverage_cache,
            keys,
            decryptor,
            s52_engine: None,
            view_meters_per_pixel: 1.0,
            view_ppmm: 4.0, // 96 DPI baseline
            view_width_px: 0.0,
            view_height_px: 0.0,
            view_center_x: 0.0,
            view_center_y: 0.0,
            scene_log: None,
        }
    }

    /// Create builder with external chart cache (for persistent use across frames)
    pub fn with_cache(
        catalog: &'a ChartCatalog,
        keys: &'a KeyStore,
        decryptor: &'a Mutex<CachedDecryptor>,
        chart_cache: &'a ChartCache,
        coverage_cache: &'a CoverageCache,
    ) -> Self {
        Self {
            catalog,
            chart_cache,
            coverage_cache,
            keys,
            decryptor,
            s52_engine: None,
            view_meters_per_pixel: 1.0,
            view_ppmm: 4.0, // 96 DPI baseline
            view_width_px: 0.0,
            view_height_px: 0.0,
            view_center_x: 0.0,
            view_center_y: 0.0,
            scene_log: None,
        }
    }

    /// Set the S-52 presentation engine for display category filtering
    /// The cell scale SUPER_SCAMIN should use, or `None` when the rule is off.
    fn super_scamin_scale(&self, info: &ChartInfo) -> Option<u32> {
        self.s52_engine
            .filter(|e| e.settings.use_super_scamin)
            .map(|_| info.native_scale)
    }

    /// Note a feature the builder decided not to draw (`--dump-scene` only).
    fn log_skip(
        &self,
        info: &ChartInfo,
        feature_index: usize,
        feature: &crate::senc::Feature,
        reason: &'static str,
    ) {
        if let Some(log) = &self.scene_log {
            // Same extent the draw path uses, including the degenerate-bbox
            // reconstruction — a skip that cannot be located on screen is only
            // half an answer.
            let extent = feature.area_geometry.as_ref().map(|geom| {
                if let Some(b) = (bbox_is_degenerate(geom.extent))
                    .then(|| area_bounds_global(geom, info.ref_lat, info.ref_lon))
                    .flatten()
                {
                    [b.min_x, b.min_y, b.max_x, b.max_y]
                } else {
                    let (min_x, min_y) =
                        crate::tiles::latlon_to_mercator(geom.extent.min_y, geom.extent.min_x);
                    let (max_x, max_y) =
                        crate::tiles::latlon_to_mercator(geom.extent.max_y, geom.extent.max_x);
                    [min_x, min_y, max_x, max_y]
                }
            });
            log.lock().unwrap().record_skip(
                &info.path.file_stem().unwrap_or_default().to_string_lossy(),
                feature_index,
                crate::senc::s57_code_to_acronym(feature.type_code),
                reason,
                extent,
            );
        }
    }

    /// Collect scene provenance while building (`navcore --dump-scene`).
    pub fn with_scene_log(mut self) -> Self {
        self.scene_log = Some(std::sync::Mutex::new(super::scene::SceneLog::new()));
        self
    }

    /// Take the collected provenance, leaving the builder without a log.
    pub fn take_scene_log(&mut self) -> Option<super::scene::SceneLog> {
        self.scene_log
            .take()
            .map(|m| m.into_inner().unwrap_or_default())
    }

    pub fn with_s52_engine(mut self, engine: &'a S52Engine) -> Self {
        self.s52_engine = Some(engine);
        self
    }

    /// Check whether a TX/TE label with the given text-display-group (dis) should
    /// be suppressed by the user's ShowImportantTextOnly setting. Mirrors
    /// OpenCPN's `m_bShowS57ImportantTextOnly && text->dis >= 20`.
    /// Effective zoom level at/above which low-priority (dis >= 20) labels —
    /// place names, light descriptions, etc. — are shown despite
    /// ShowImportantTextOnly. Below this they are suppressed to avoid a text
    /// stampede at overview scales. Mirrors how OpenCPN's labels only surface
    /// once zoomed into harbour/approach scales.
    const TEXT_DETAIL_ZOOM: u8 = 14;

    #[inline]
    fn should_skip_text(&self, dis: u8) -> bool {
        self.s52_engine
            .map(|e| {
                if !e.settings.show_important_text_only || dis < 20 {
                    return false;
                }
                // Scale-aware LOD: keep the dis >= 20 labels once zoomed into
                // detail (harbour/marina), suppress them at overview zooms.
                let z = super::zoom_from_camera(self.view_meters_per_pixel);
                z < Self::TEXT_DETAIL_ZOOM
            })
            .unwrap_or(false)
    }

    /// Set view parameters used for scale-dependent rendering.
    ///
    /// - `meters_per_pixel`: current zoom in world meters per screen pixel
    /// - `ppmm`: screen pixels per millimeter (includes HiDPI scale factor)
    /// - `view_width_px`: viewport width in pixels
    /// - `view_height_px`: viewport height in pixels
    /// - `view_center_x`: camera center in global Mercator meters
    /// - `view_center_y`: camera center in global Mercator meters
    pub fn with_view_params(
        mut self,
        meters_per_pixel: f32,
        ppmm: f32,
        view_width_px: f32,
        view_height_px: f32,
        view_center_x: f32,
        view_center_y: f32,
    ) -> Self {
        self.view_meters_per_pixel = meters_per_pixel;
        self.view_ppmm = ppmm;
        self.view_width_px = view_width_px;
        self.view_height_px = view_height_px;
        self.view_center_x = view_center_x;
        self.view_center_y = view_center_y;
        self
    }

    fn view_scale_denominator(&self) -> f64 {
        // Scale denominator N in 1:N.
        // 1 px corresponds to (1/ppmm) mm = (0.001/ppmm) meters on screen.
        // So N = ground_metres_per_pixel / (0.001/ppmm).
        //
        // The camera works in *Mercator* metres, which are stretched by
        // 1/cos(latitude) — a factor of 1.77 at Danish latitudes. Feeding that
        // straight in made every SCAMIN test think the view was 1.8x more
        // zoomed out than it is, so detail vanished far too early: at Brøndby
        // the harbour buildings (SCAMIN 11999) were dropped from a 1:8400 view
        // that OpenCPN draws them in. A chart scale is a ground scale.
        let mpp = self.view_meters_per_pixel.max(1e-6) as f64;
        let ppmm = self.view_ppmm.max(1e-6) as f64;
        let (lat, _) = crate::tiles::mercator_to_latlon(
            self.view_center_x as f64,
            self.view_center_y as f64,
        );
        let ground_mpp = mpp * lat.to_radians().cos().max(0.01);
        ground_mpp * ppmm * 1000.0
    }

    /// Build a tile packet (CPU work only, no GPU upload).
    ///
    /// Note: this includes lines even though the tile renderer currently only draws areas.
    /// For smoother MVP rendering, prefer `build_cpu_areas_only()` from the render loop.
    pub fn build_cpu(&self, tile_id: TileId) -> Result<TilePacket, BuildError> {
        self.build_cpu_impl(tile_id, true)
    }

    /// Build a tile packet containing areas only (CPU work only, no GPU upload).
    ///
    /// This avoids doing significant extra work (line clipping + vertex generation)
    /// until tile-mode line rendering is integrated.
    pub fn build_cpu_areas_only(&self, tile_id: TileId) -> Result<TilePacket, BuildError> {
        self.build_cpu_impl(tile_id, false)
    }

    /// Inspect one tile and report which features from less-detailed charts are masked
    /// by more-detailed chart coverage polygons.
    pub fn inspect_tile_coverage(&self, tile_id: TileId) -> Result<String, BuildError> {
        let bounds = tile_id.bounds();
        let charts = self.catalog.charts_for_tile(&bounds);
        let mut loaded_charts: Vec<LoadedChartContext<'_>> = Vec::new();

        for info in charts.into_iter().take(Self::MAX_CHARTS_PER_TILE) {
            let chart = Some(self.load_chart(info)?);
            if let Some(chart) = chart {
                let (ref_mx, ref_my) = crate::tiles::latlon_to_mercator(info.ref_lat, info.ref_lon);
                let coverage_triangles = self.coverage_for(info.id, &chart, ref_mx, ref_my);
                loaded_charts.push(LoadedChartContext {
                    chart,
                    info,
                    ref_mx,
                    ref_my,
                    coverage_triangles,
                });
            }
        }

        let mut report = String::new();
        writeln!(
            &mut report,
            "Tile {:?}: {} intersecting charts",
            tile_id,
            loaded_charts.len()
        )
        .ok();

        if let Some(log) = &self.scene_log {
            let mut log = log.lock().unwrap();
            for ctx in &loaded_charts {
                log.record_coverage(
                    &ctx.info.path.file_stem().unwrap_or_default().to_string_lossy(),
                    ctx.info.native_scale,
                    [
                        ctx.info.extent_mercator.min_x,
                        ctx.info.extent_mercator.min_y,
                        ctx.info.extent_mercator.max_x,
                        ctx.info.extent_mercator.max_y,
                    ],
                    ctx.coverage_triangles.iter().map(|t| t.to_vec()).collect(),
                );
            }
        }

        for (chart_index, ctx) in loaded_charts.iter().enumerate() {
            let detailed_coverages = collect_more_detailed_coverages(&loaded_charts, chart_index);
            writeln!(
                &mut report,
                "- chart {} (1:{}): {} coverage polygons, {} more-detailed masks",
                chart_debug_name(ctx.info),
                ctx.info.native_scale,
                ctx.coverage_triangles.len(),
                detailed_coverages.len()
            )
            .ok();

            let mut masked_counts: HashMap<String, usize> = HashMap::new();
            let mut rendered_counts: HashMap<String, usize> = HashMap::new();
            let mut rendered_details: Vec<String> = Vec::new();
            let engine = self.s52_engine;

            for feature in ctx.chart.areas() {
                let Some(geom) = &feature.area_geometry else { continue };
                let bbox = geom.extent;
                let (cx, cy) = crate::tiles::latlon_to_mercator(
                    (bbox.min_y + bbox.max_y) / 2.0,
                    (bbox.min_x + bbox.max_x) / 2.0,
                );
                let resolved = engine.and_then(|e| e.resolve_feature(feature, GeometryType::Area));
                let is_masked = !feature.is_coverage()
                    && point_in_any_triangle([cx, cy], &detailed_coverages)
                    && relation_all_vertices_covered(
                        geom,
                        ctx.info.ref_lat,
                        ctx.info.ref_lon,
                        &detailed_coverages,
                    );
                if is_masked && resolved.is_some() {
                    *masked_counts
                        .entry(format!("AREA {}", s57_code_to_acronym(feature.type_code)))
                        .or_insert(0) += 1;
                    continue;
                }
                if let Some(resolved) = resolved {
                    if geom.extent.tile_relation(&bounds) != BBoxTileRelation::Outside {
                        if matches!(
                            feature.object_class,
                            ObjectClass::Coverage
                                | ObjectClass::CompilationScale
                                | ObjectClass::DepthArea
                                | ObjectClass::DredgedArea
                                | ObjectClass::ShorelineConstruction
                                | ObjectClass::Coastline
                        ) {
                            rendered_details.push(format!(
                                "AREA {} center=({:.0},{:.0}) bbox=({:.0}x{:.0}) prio={} {}",
                                s57_code_to_acronym(feature.type_code),
                                cx,
                                cy,
                                (bbox.max_x - bbox.min_x).abs(),
                                (bbox.max_y - bbox.min_y).abs(),
                                resolved.priority,
                                instruction_summary(&resolved.instructions),
                            ));
                        }
                        *rendered_counts
                            .entry(format!(
                                "AREA {} prio={} {}",
                                s57_code_to_acronym(feature.type_code),
                                resolved.priority,
                                instruction_summary(&resolved.instructions),
                            ))
                            .or_insert(0) += 1;
                    }
                }
            }

            for feature in ctx.chart.lines() {
                let Some(geom) = &feature.line_geometry else { continue };
                let bbox = geom.extent;
                let (cx, cy) = crate::tiles::latlon_to_mercator(
                    (bbox.min_y + bbox.max_y) / 2.0,
                    (bbox.min_x + bbox.max_x) / 2.0,
                );
                let resolved = engine.and_then(|e| e.resolve_feature(feature, GeometryType::Line));
                if point_in_any_triangle([cx, cy], &detailed_coverages) && resolved.is_some() {
                    *masked_counts
                        .entry(format!("LINE {}", s57_code_to_acronym(feature.type_code)))
                        .or_insert(0) += 1;
                    continue;
                }
                if let Some(resolved) = resolved {
                    let segments = geom.resolve(&ctx.chart.edge_table);
                    let mut drew = false;
                    for segment in segments {
                        if segment.len() < 2 {
                            continue;
                        }
                        let global: Vec<[f64; 2]> = segment
                            .iter()
                            .map(|p| [ctx.ref_mx + p[0] as f64, ctx.ref_my + p[1] as f64])
                            .collect();
                        let clipped = clip_polyline(&global, &bounds);
                        let clipped = if detailed_coverages.is_empty() {
                            clipped
                        } else {
                            clipped
                                .into_iter()
                                .flat_map(|poly| mask_polyline_by_coverage(&poly, &detailed_coverages))
                                .collect()
                        };
                        if clipped.iter().any(|poly| poly.len() >= 2) {
                            drew = true;
                            break;
                        }
                    }
                    if drew {
                        if matches!(
                            feature.object_class,
                            ObjectClass::ShorelineConstruction | ObjectClass::Coastline
                        ) {
                            rendered_details.push(format!(
                                "LINE {} center=({:.0},{:.0}) bbox=({:.0}x{:.0}) prio={} {}",
                                s57_code_to_acronym(feature.type_code),
                                cx,
                                cy,
                                (bbox.max_x - bbox.min_x).abs(),
                                (bbox.max_y - bbox.min_y).abs(),
                                resolved.priority,
                                instruction_summary(&resolved.instructions),
                            ));
                        }
                        *rendered_counts
                            .entry(format!(
                                "LINE {} prio={} {}",
                                s57_code_to_acronym(feature.type_code),
                                resolved.priority,
                                instruction_summary(&resolved.instructions),
                            ))
                            .or_insert(0) += 1;
                    }
                }
            }

            for feature in ctx.chart.points() {
                let Some(point) = &feature.point_geometry else { continue };
                let (mx, my) = crate::tiles::latlon_to_mercator(point.x, point.y);
                let resolved = engine.and_then(|e| e.resolve_feature(feature, GeometryType::Point));
                if point_in_any_triangle([mx, my], &detailed_coverages) && resolved.is_some() {
                    *masked_counts
                        .entry(format!("POINT {}", s57_code_to_acronym(feature.type_code)))
                        .or_insert(0) += 1;
                    continue;
                }
                if let Some(resolved) = resolved {
                    if mx >= bounds.min_x
                        && mx <= bounds.max_x
                        && my >= bounds.min_y
                        && my <= bounds.max_y
                    {
                        *rendered_counts
                            .entry(format!(
                                "POINT {} prio={} {}",
                                s57_code_to_acronym(feature.type_code),
                                resolved.priority,
                                instruction_summary(&resolved.instructions),
                            ))
                            .or_insert(0) += 1;
                    }
                }
            }

            for feature in ctx.chart.soundings() {
                let Some(mp) = &feature.multipoint_geometry else { continue };
                for point in &mp.points {
                    let merc = [ctx.ref_mx + point[0] as f64, ctx.ref_my + point[1] as f64];
                    if point_in_any_triangle(merc, &detailed_coverages) {
                        *masked_counts.entry("SOUNDG".to_string()).or_insert(0) += 1;
                    }
                }
            }

            let mut entries: Vec<_> = masked_counts.into_iter().collect();
            entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            for (label, count) in entries {
                writeln!(&mut report, "  masked {} x{}", label, count).ok();
            }

            let mut rendered_entries: Vec<_> = rendered_counts.into_iter().collect();
            rendered_entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            for (label, count) in rendered_entries.into_iter().take(20) {
                writeln!(&mut report, "  rendered {} x{}", label, count).ok();
            }
            rendered_details.sort();
            for detail in rendered_details.into_iter().take(24) {
                writeln!(&mut report, "    {}", detail).ok();
            }
        }

        Ok(report)
    }

    fn build_cpu_impl(
        &self,
        tile_id: TileId,
        include_lines: bool,
    ) -> Result<TilePacket, BuildError> {
        let profile = std::env::var("NAVCORE_PROFILE")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false);
        let build_start = profile.then(Instant::now);
        let bounds = tile_id.bounds();
        // Select charts appropriate to this tile's display scale so zoomed-out
        // tiles don't aggregate the full detail of every overlapping large-scale
        // chart (see ChartCatalog::charts_for_tile_scaled).
        let tile_scale_denom = super::tile_scale_denominator(tile_id, self.view_ppmm as f64);
        let charts = self.catalog.charts_for_tile_where(
            &bounds,
            tile_scale_denom,
            &|info| self.has_coverage_in(info, &bounds),
            &|info, part| self.coverage_fills(info, part),
        );

        if log::log_enabled!(log::Level::Debug) {
            log::debug!("=== TILE {:?} ===", tile_id);
            log::debug!(
                "  bounds (Mercator): ({:.0},{:.0})-({:.0},{:.0})",
                bounds.min_x,
                bounds.min_y,
                bounds.max_x,
                bounds.max_y
            );
            log::debug!("  charts intersecting: {}", charts.len());
            for info in charts.iter().take(5) {
                log::debug!(
                    "    chart '{}' (1:{}): ({:.0},{:.0})-({:.0},{:.0}) ref=({:.4},{:.4})",
                    info.name,
                    info.native_scale,
                    info.extent_mercator.min_x,
                    info.extent_mercator.min_y,
                    info.extent_mercator.max_x,
                    info.extent_mercator.max_y,
                    info.ref_lat,
                    info.ref_lon
                );
            }
            if charts.len() > 5 {
                log::debug!("    ... {} more charts omitted", charts.len() - 5);
            }
        }

        let mut packet = TilePacket::new(tile_id);
        let mut all_symbol_priorities: Vec<(u8, usize)> = Vec::new();
        // (priority, is_background, vert_start, vert_end, pat_start, pat_end)
        let mut all_area_ranges: Vec<(u8, bool, usize, usize, usize, usize)> = Vec::new();
        // Deferred label candidates for two-phase decluttering (C4a).
        // Collected during build_areas/build_symbols, laid out only after AABB pre-check.
        let mut label_candidates: Vec<TextParams> = Vec::new();

        // Shared line collections across all charts — merged batches reduce GPU buffer count
        let mut polylines_by_key: HashMap<LineBatchKey, Vec<Vec<[f32; 2]>>> =
            HashMap::with_capacity(32);
        let mut lc_polylines: LcPolylines = HashMap::new();
        let mut line_stats_total = LineBuildStats::default();

        let mut loaded_charts: Vec<LoadedChartContext<'_>> = Vec::new();
        let load_start = profile.then(Instant::now);

        // Process charts in scale order (small scale first = background)
        // Skip charts that fail to load - some may have parsing issues.
        //
        // Over the cap, the coarsest go, not the finest: the list is ordered
        // coarse to fine, and taking its head threw away the very charts the
        // tile was built for. The world basemap, first of all, stays — it
        // is a handful of polygons and the only land outside the charts.
        let charts = if charts.len() > Self::MAX_CHARTS_PER_TILE {
            let (world, rest): (Vec<_>, Vec<_>) = charts
                .into_iter()
                .partition(|c| crate::s57::basemap::is_basemap(&c.path));
            let keep = Self::MAX_CHARTS_PER_TILE.saturating_sub(world.len());
            let skip = rest.len().saturating_sub(keep);
            world.into_iter().chain(rest.into_iter().skip(skip)).collect()
        } else {
            charts
        };
        for info in charts.into_iter() {
            let chart = match self.load_chart(info) {
                Ok(chart) => Some(chart),
                Err(e) => {
                    // Log but continue - don't fail entire tile for one bad chart
                    log::warn!("Skipping chart {}: {}", info.name, e);
                    continue;
                }
            };
            if let Some(chart) = chart {
                let (ref_mx, ref_my) = crate::tiles::latlon_to_mercator(info.ref_lat, info.ref_lon);
                let coverage_triangles = self.coverage_for(info.id, &chart, ref_mx, ref_my);
                loaded_charts.push(LoadedChartContext {
                    chart,
                    info,
                    ref_mx,
                    ref_my,
                    coverage_triangles,
                });
            }
        }

        if let Some(log) = &self.scene_log {
            let mut log = log.lock().unwrap();
            for ctx in &loaded_charts {
                log.record_coverage(
                    &ctx.info.path.file_stem().unwrap_or_default().to_string_lossy(),
                    ctx.info.native_scale,
                    [
                        ctx.info.extent_mercator.min_x,
                        ctx.info.extent_mercator.min_y,
                        ctx.info.extent_mercator.max_x,
                        ctx.info.extent_mercator.max_y,
                    ],
                    ctx.coverage_triangles.iter().map(|t| t.to_vec()).collect(),
                );
            }
        }

        let load_ms = load_start.map(|t| t.elapsed().as_micros()).unwrap_or(0);
        let feat_start = profile.then(Instant::now);

        // The finest scale present in this tile. Everything coarser is drawn
        // first so the finest cell wins by overdraw; nothing is masked away.
        let finest_scale = loaded_charts
            .iter()
            .map(|c| c.info.native_scale)
            .min()
            .unwrap_or(0);

        for (chart_index, ctx) in loaded_charts.iter().enumerate() {
            let detailed_coverages = collect_more_detailed_coverages(&loaded_charts, chart_index);

            // Never the world basemap: a chart's coverage says where it has
            // data, not that anything of it is drawn at this zoom — NOAA
            // cells hide their land behind SCAMIN when zoomed out, and
            // skipping the basemap under them left the land blank.
            if chart_index + 1 < loaded_charts.len()
                && !crate::s57::basemap::is_basemap(&ctx.info.path)
                && tile_region_fully_covered(bounds, &detailed_coverages)
            {
                continue;
            }

            // A chart is "background" when a finer-scale chart shares the tile.
            let is_background = ctx.info.native_scale > finest_scale;

            // Zoomed in past what a 1:50 000 000 outline can honestly show,
            // the basemap's land is no longer drawn: off Skåne it put the
            // Swedish coast kilometres out over open water. What is not
            // covered by a real chart is uncharted, and S-52 says to show it
            // as such — NODTA, the "no data" grey — so the basemap lays that
            // under the whole tile instead, and the charts draw over it.
            if crate::s57::basemap::is_basemap(&ctx.info.path) && tile_scale_denom < NO_DATA_FINER_THAN {
                if let Some(nodta) = self.s52_engine.and_then(|e| e.get_color_index("NODTA")) {
                    let vert_start = packet.area_vertices.len();
                    let pat_start = packet.pattern_vertices.len();
                    packet.area_vertices.extend(no_data_quad(&bounds, nodta as u32));
                    all_area_ranges.push((
                        0,
                        is_background,
                        vert_start,
                        packet.area_vertices.len(),
                        pat_start,
                        pat_start,
                    ));
                    continue;
                }
            }

            // Process area features with spatial prefilter
            let area_stats = self.build_areas(
                    &ctx.chart,
                    ctx.info,
                    &bounds,
                    &mut packet,
                    ctx.ref_mx,
                    ctx.ref_my,
                    &mut label_candidates,
                    &mut all_area_ranges,
                    &mut lc_polylines,
                    &detailed_coverages,
                    &ctx.coverage_triangles,
                    is_background,
                );
                if log::log_enabled!(log::Level::Debug) {
                    log::debug!(
                        "  chart '{}' areas: total={} with_geom={} bbox_pass={} fast={} slow={} tris_seen={} trivial_accept={} trivial_reject={} tris_clipped={} verts_added={}",
                        ctx.info.name,
                        area_stats.total_features,
                        area_stats.with_geometry,
                        area_stats.bbox_pass,
                        area_stats.fast_path_features,
                        area_stats.slow_path_features,
                        area_stats.triangles_seen,
                        area_stats.trivial_accept_triangles,
                        area_stats.trivial_reject_triangles,
                        area_stats.triangles_clipped,
                        area_stats.vertices_added
                    );
                }

                if include_lines {
                    // Process line features — accumulates into shared collections
                    let line_stats = self.build_lines(
                        &ctx.chart,
                        ctx.info,
                        &bounds,
                        &mut polylines_by_key,
                        &mut lc_polylines,
                        &detailed_coverages,
                        is_background,
                        &mut label_candidates,
                    );
                    if log::log_enabled!(log::Level::Debug) {
                        log::debug!(
                            "  chart '{}' lines: total={} with_geom={} bbox_pass={} segments_out={} verts_added={}",
                            ctx.info.name,
                            line_stats.total_features,
                            line_stats.with_geometry,
                            line_stats.bbox_pass,
                            line_stats.segments_out,
                            line_stats.vertices_added
                        );
                        log::debug!(
                            "    S-52: ls={} cs={} cs_fb={} lc={} lc_miss={} lc_fb={} skip_other={} no_lookup={}",
                            line_stats.styled_by_ls,
                            line_stats.styled_by_cs,
                            line_stats.styled_by_cs_fallback,
                            line_stats.styled_by_lc,
                            line_stats.styled_by_lc_missing,
                            line_stats.styled_by_lc_fallback,
                            line_stats.skipped_other,
                            line_stats.skipped_no_lookup
                        );
                    }
                    line_stats_total.merge(&line_stats);
                }

                // Process point features for symbols
                let symbol_stats = self.build_symbols(
                    &ctx.chart,
                    ctx.info,
                    &bounds,
                    &mut packet,
                    &mut all_symbol_priorities,
                    &mut label_candidates,
                    &detailed_coverages,
                );
                if log::log_enabled!(log::Level::Debug) {
                    log::debug!(
                        "  chart '{}' symbols: total_pts={} matched={} multi_sy={} bbox_pass={} added={}",
                        ctx.info.name,
                        symbol_stats.total_features,
                        symbol_stats.matched_symbol,
                        symbol_stats.multi_symbol_features,
                        symbol_stats.bbox_pass,
                        symbol_stats.instances_added
                    );
                }

                let soundings_added = self.build_soundings(
                    &ctx.chart,
                    ctx.info,
                    tile_id,
                    &bounds,
                    &mut packet,
                    ctx.ref_mx,
                    ctx.ref_my,
                    &detailed_coverages,
                );
                if log::log_enabled!(log::Level::Debug) && soundings_added > 0 {
                    log::debug!(
                        "  chart '{}' soundings: added={}",
                        ctx.info.name,
                        soundings_added
                    );
                }
        }

        let feat_ms = feat_start.map(|t| t.elapsed().as_micros()).unwrap_or(0);
        let fin_start = profile.then(Instant::now);

        // Finalize merged line batches (across all charts)
        if include_lines {
            // Build line batches for each key, sorted by (disp_prio, pass, style_hash)
            let mut keys: Vec<_> = polylines_by_key.keys().cloned().collect();
            keys.sort();

            for key in keys {
                let polylines = polylines_by_key.remove(&key).unwrap();
                if polylines.is_empty() {
                    continue;
                }

                let (vertices, indices) = build_line_vertices_multi_indexed(&polylines);
                if vertices.is_empty() {
                    continue;
                }

                line_stats_total.vertices_added += vertices.len();

                packet.line_batches.push(LineBatch {
                    key,
                    vertices,
                    indices,
                });
            }

            // LC() lines: their segments, which the line shader dresses with
            // the symbol at a constant size on screen (see render::lc_pattern).
            let mut lc_keys: Vec<_> = lc_polylines.keys().cloned().collect();
            lc_keys.sort();
            for key in lc_keys {
                let pieces = lc_polylines.remove(&key).unwrap();
                let segments = crate::render::lc_segments(&pieces);
                if segments.is_empty() {
                    continue;
                }
                packet.lc_batches.push(LcBatch { key, segments });
            }

            if log::log_enabled!(log::Level::Debug) {
                log::debug!(
                    "  line totals: features={} batches={} verts={}",
                    line_stats_total.total_features,
                    packet.line_batches.len(),
                    line_stats_total.vertices_added,
                );
            }
        }

        // Sort ALL area vertices globally by priority and split into fg/bg buffers.
        // This ensures correct S-52 draw order across multi-chart tiles.
        // Background geometry is drawn first; foreground over it.
        if !all_area_ranges.is_empty() {
            all_area_ranges.sort_by_key(|r| r.0);

            let old_areas = std::mem::take(&mut packet.area_vertices);
            let old_patterns = std::mem::take(&mut packet.pattern_vertices);

            // Pre-count fg/bg sizes for reserve
            let (mut fg_area_count, mut bg_area_count) = (0usize, 0usize);
            let (mut fg_pat_count, mut bg_pat_count) = (0usize, 0usize);
            for &(_, is_bg, vs, ve, ps, pe) in &all_area_ranges {
                if is_bg {
                    bg_area_count += ve - vs;
                    bg_pat_count += pe - ps;
                } else {
                    fg_area_count += ve - vs;
                    fg_pat_count += pe - ps;
                }
            }

            packet.area_vertices.reserve(fg_area_count);
            packet.bg_area_vertices.reserve(bg_area_count);
            packet.pattern_vertices.reserve(fg_pat_count);
            packet.bg_pattern_vertices.reserve(bg_pat_count);

            let mut fg_area_offsets = [0u32; 11];
            let mut fg_pattern_offsets = [0u32; 11];
            let mut bg_area_offsets = [0u32; 11];
            let mut bg_pattern_offsets = [0u32; 11];
            let mut fg_prio = 0usize;
            let mut bg_prio = 0usize;

            for &(prio, is_bg, vs, ve, ps, pe) in &all_area_ranges {
                let p = (prio as usize).min(9);
                if is_bg {
                    while bg_prio <= p {
                        bg_area_offsets[bg_prio] = packet.bg_area_vertices.len() as u32;
                        bg_pattern_offsets[bg_prio] = packet.bg_pattern_vertices.len() as u32;
                        bg_prio += 1;
                    }
                    packet.bg_area_vertices.extend_from_slice(&old_areas[vs..ve]);
                    packet.bg_pattern_vertices.extend_from_slice(&old_patterns[ps..pe]);
                } else {
                    while fg_prio <= p {
                        fg_area_offsets[fg_prio] = packet.area_vertices.len() as u32;
                        fg_pattern_offsets[fg_prio] = packet.pattern_vertices.len() as u32;
                        fg_prio += 1;
                    }
                    packet.area_vertices.extend_from_slice(&old_areas[vs..ve]);
                    packet.pattern_vertices.extend_from_slice(&old_patterns[ps..pe]);
                }
            }
            while fg_prio <= 10 {
                fg_area_offsets[fg_prio] = packet.area_vertices.len() as u32;
                fg_pattern_offsets[fg_prio] = packet.pattern_vertices.len() as u32;
                fg_prio += 1;
            }
            while bg_prio <= 10 {
                bg_area_offsets[bg_prio] = packet.bg_area_vertices.len() as u32;
                bg_pattern_offsets[bg_prio] = packet.bg_pattern_vertices.len() as u32;
                bg_prio += 1;
            }

            // The scene log records vertices as the feature loop emits them;
            // this records the buffer *after* the priority reorder, which is
            // what actually reaches the GPU. If a bbox here does not cover the
            // ground the features did, the reorder mis-sliced.
            if let Some(log) = &self.scene_log {
                let mut bboxes = Vec::new();
                let (ox, oy) = bounds.center();
                for p in 0..10usize {
                    let (s0, e0) = (fg_area_offsets[p] as usize, fg_area_offsets[p + 1] as usize);
                    if e0 > s0 {
                        let mut b = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
                        for v in &packet.area_vertices[s0..e0] {
                            b[0] = b[0].min(v.position[0] as f64 + ox);
                            b[1] = b[1].min(v.position[1] as f64 + oy);
                            b[2] = b[2].max(v.position[0] as f64 + ox);
                            b[3] = b[3].max(v.position[1] as f64 + oy);
                        }
                        bboxes.push((p as u8, (e0 - s0) as u32, b));
                    }
                }
                log.lock().unwrap().record_packet(bboxes);
            }

            packet.area_priority_offsets = fg_area_offsets;
            packet.pattern_priority_offsets = fg_pattern_offsets;
            packet.bg_area_priority_offsets = bg_area_offsets;
            packet.bg_pattern_priority_offsets = bg_pattern_offsets;
        }

        // Sort ALL symbol instances globally by priority and compute one set of offsets.
        // This must happen after all charts are processed so multi-chart tiles get correct offsets.
        if !all_symbol_priorities.is_empty() {
            all_symbol_priorities.sort_by_key(|&(prio, _)| prio);

            let old_symbols = std::mem::take(&mut packet.symbol_instances);
            packet.symbol_instances.reserve(old_symbols.len());

            let mut offsets = [0u32; 11];
            let mut current_prio = 0usize;

            for &(prio, orig_idx) in &all_symbol_priorities {
                let p = (prio as usize).min(9);
                while current_prio <= p {
                    offsets[current_prio] = packet.symbol_instances.len() as u32;
                    current_prio += 1;
                }
                packet.symbol_instances.push(old_symbols[orig_idx]);
            }
            while current_prio <= 10 {
                offsets[current_prio] = packet.symbol_instances.len() as u32;
                current_prio += 1;
            }
            packet.symbol_priority_offsets = offsets;
        }

        // Sector arcs and legs, by priority, keeping their emission order
        // (outline, colour, legs) within a priority.
        if !packet.sector_instances.is_empty() {
            packet.sector_instances.sort_by_key(|s| s.disp_prio.min(9));
            packet.sector_priority_offsets = priority_offsets(
                packet.sector_instances.iter().map(|s| s.disp_prio.min(9) as u8),
            );
        }

        // Two-phase label decluttering: cheap AABB pre-check then full glyph layout (C4a).
        // Only ~20% of labels survive decluttering, so we save ~80% of layout_text() calls.
        // The mariner's "text" switch: names, light descriptions and other
        // TX/TE labels go; soundings have their own switch.
        let show_text = self.s52_engine.is_none_or(|e| e.settings.show_text);
        packet.label_candidates = if show_text { label_candidates } else { Vec::new() };
        // Text layout and decluttering deferred to global pass

        let fin_ms = fin_start.map(|t| t.elapsed().as_micros()).unwrap_or(0);
        if profile {
            log::info!(
                "profile.tile_phases: tile={:?} load={}us features={}us finalize={}us verts={} lines={} syms={}",
                tile_id, load_ms, feat_ms, fin_ms,
                packet.area_vertices.len() + packet.bg_area_vertices.len(),
                packet.total_line_vertices(),
                packet.symbol_instances.len(),
            );
        }
        packet.compute_byte_size();
        if log::log_enabled!(log::Level::Debug) {
            log::debug!(
                "  tile {:?} packet: area_verts={} line_batches={} line_verts={} symbols={} labels={} soundings={} bytes={}",
                tile_id,
                packet.area_vertices.len(),
                packet.line_batches.len(),
                packet.total_line_vertices(),
                packet.symbol_instances.len(),
                packet.label_candidates.len(),
                packet.text_instances.len(),
                packet.byte_size
            );
        }
        if let Some(start) = build_start {
            log::info!(
                "profile.build_cpu_impl: {} ms tile={:?} charts_loaded={} include_lines={} bytes={}",
                start.elapsed().as_millis(),
                tile_id,
                loaded_charts.len(),
                include_lines,
                packet.byte_size
            );
        }
        Ok(packet)
    }

    /// Ensure chart is loaded in cache
    /// The parsed chart, loading it if this is the first request.
    ///
    /// Concurrent callers for the same chart wait for the first one rather than
    /// each parsing their own copy — see [`ChartCache`].
    pub fn load_chart(&self, info: &ChartInfo) -> Result<Arc<ChartData>, BuildError> {
        let slot = {
            let mut cache = self.chart_cache.lock().unwrap();
            Arc::clone(cache.entry(info.id).or_default())
        };
        let mut slot = slot.lock().unwrap();
        if let Some(chart) = slot.as_ref() {
            return Ok(Arc::clone(chart));
        }

        // The world basemap, built into the program.
        if crate::s57::basemap::is_basemap(&info.path) {
            let bytes = crate::s57::basemap::senc().to_vec();
            let chart = Arc::new(ChartData::parse(bytes).map_err(BuildError::Parse)?);
            *slot = Some(Arc::clone(&chart));
            return Ok(chart);
        }

        // A free S-57 cell: converted (or read from the cache), no key.
        if ChartCatalog::is_s57(&info.path) {
            let bytes = self
                .decryptor
                .lock()
                .unwrap()
                .s57_senc(&info.path)
                .map_err(BuildError::S57)?;
            let chart = Arc::new(ChartData::parse(bytes).map_err(BuildError::Parse)?);
            *slot = Some(Arc::clone(&chart));
            return Ok(chart);
        }

        // Get chart name for key lookup
        let chart_name = info
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_default();

        let install_key = self
            .keys
            .lookup(&chart_name)
            .ok_or_else(|| BuildError::NoKey(chart_name.clone()))?
            .to_string();

        let senc_bytes = self
            .decryptor
            .lock()
            .unwrap()
            .decrypt_chart(&info.path, &install_key)
            .map_err(BuildError::Decrypt)?;

        let chart = Arc::new(ChartData::parse(senc_bytes).map_err(BuildError::Parse)?);
        *slot = Some(Arc::clone(&chart));
        Ok(chart)
    }

    /// Build area triangles with spatial prefilter and clipping
    fn build_areas(
        &self,
        chart: &ChartData,
        info: &ChartInfo,
        bounds: &TileBounds,
        packet: &mut TilePacket,
        ref_mx: f64,
        ref_my: f64,
        label_candidates: &mut Vec<TextParams>,
        all_area_ranges: &mut Vec<(u8, bool, usize, usize, usize, usize)>,
        lc_polylines: &mut LcPolylines,
        detailed_coverages: &[[[f64; 2]; 3]],
        // The chart's own M_COVR rings. Reported by `--dump-scene` so a chart
        // painting outside its declared coverage is visible; not used to gate
        // drawing, because doing that correctly needs geometry clipped to the
        // ring, and both cheaper tests (feature centroid, extent overlap)
        // measured *worse* against the reference than drawing everything.
        coverage_triangles: &[[[f64; 2]; 3]],
        is_background: bool,
    ) -> AreaBuildStats {
        let _ = coverage_triangles;
        let mut stats = AreaBuildStats::default();
        let mut logged_first_bbox = false;
        let before_verts = packet.area_vertices.len();

        // Edges owned by the high-priority coastline-formers (land area, coastline,
        // shoreline construction). Lower-priority area boundaries (e.g. the
        // magenta CTNARE caution boundary, prio 2) that share these edges are
        // suppressed so the coast is drawn once as the coastline — OpenCPN's
        // PrioritizeLineFeature shared-edge rule. Edge indices are per-chart.
        let coast_edges = collect_coast_edges(chart);

        for (feature_index, feature) in chart
            .features
            .iter()
            .enumerate()
            .filter(|(_, f)| f.feature_type == crate::senc::FeatureType::Area)
        {
            stats.total_features += 1;

            // Cheapest test first. Every other test in this loop — the display
            // category, the display priority, SCAMIN, the full S-52 resolve —
            // costs table lookups and hashing, and a tile covers a small
            // fraction of a chart, so the overwhelming majority of features are
            // not in it. Doing the bbox test last meant a 100-feature tile paid
            // S-52 resolution on every feature of every chart overlapping it:
            // 30 ms per tile to emit a few hundred vertices.
            //
            // A degenerate bbox (some cells store one) is not trustworthy here,
            // so those fall through to the slower path below that measures the
            // real bounds from the geometry.
            if let Some(ref geom) = feature.area_geometry {
                if !bbox_is_degenerate(geom.extent)
                    && geom.extent.tile_relation(bounds) == BBoxTileRelation::Outside
                {
                    self.log_skip(info, feature_index, feature, "outside-tile");
                    continue;
                }
            }

            // SCAMIN/SCAMAX filtering: bypass SCAMIN for DISPLAYBASE features
            // (coastline, depth areas, safety contours must always be visible)
            // "SCAMIN must not apply to GROUP1 objects, Meta Objects or
            // DisplayCategoryBase objects" — s52plib.cpp, guarding against ENCs
            // that encode a spurious SCAMIN on a depth area or coastline.
            let bypass_scamin = self
                .s52_engine
                .map(|e| {
                    e.display_category_for(feature, GeometryType::Area)
                        == Some(DisplayCategory::Displaybase)
                        || e.get_display_priority(feature, GeometryType::Area)
                            == crate::s52::DisplayPriority::Group1.as_u8()
                })
                .unwrap_or(false);
            let scamin_scale = should_render_at_scale_ex(
                feature,
                self.view_scale_denominator(),
                bypass_scamin,
                false,
                self.super_scamin_scale(info),
            );
            if scamin_scale == 0.0 {
                self.log_skip(info, feature_index, feature, "scamin");
                continue;
            }

            // Resolve all S-52 info in ONE call (replaces 5-6 separate lookups)
            let engine = match self.s52_engine {
                Some(e) => e,
                None => continue,
            };
            let resolved = match engine.resolve_feature(feature, GeometryType::Area) {
                Some(r) => r,
                None => {
                    self.log_skip(info, feature_index, feature, "no-lup-or-display-category");
                    continue;
                }
            };

            let has_nodata_fill = resolved.instructions.iter().any(|instr| {
                matches!(
                    instr,
                    RenderInstruction::AreaColor { color, .. } if color == "NODTA"
                )
            });
            let has_prtsur01 = resolved.instructions.iter().any(|instr| {
                matches!(
                    instr,
                    RenderInstruction::AreaPattern { pattern } if pattern == "PRTSUR01"
                )
            });
            if let Some(ref geom) = feature.area_geometry {
                stats.with_geometry += 1;


                let actual_bounds = if bbox_is_degenerate(geom.extent) {
                    area_bounds_global(geom, info.ref_lat, info.ref_lon)
                } else {
                    None
                };
                let (cx, cy) = if let Some(bounds) = actual_bounds {
                    (
                        (bounds.min_x + bounds.max_x) * 0.5,
                        (bounds.min_y + bounds.max_y) * 0.5,
                    )
                } else {
                    let bbox = geom.extent;
                    crate::tiles::latlon_to_mercator(
                        (bbox.min_y + bbox.max_y) / 2.0,
                        (bbox.min_x + bbox.max_x) / 2.0,
                    )
                };

                // OpenCPN's quilting hides no-data (NODTA) and incompletely-surveyed
                // (PRTSUR01) fills only where a more detailed chart actually covers
                // the feature — not whenever *any* detailed overlay is present in the
                // tile. Check the feature centroid against detailed coverages so a
                // partially-overlapped low-scale polygon still renders where it isn't
                // superseded.
                if (has_nodata_fill || has_prtsur01)
                    && !detailed_coverages.is_empty()
                    && point_in_any_triangle([cx, cy], &detailed_coverages)
                {
                    continue;
                }
                // Coverage does not mask: the quilt is resolved by draw order.
                // Foreground areas at the same priority naturally overwrite background
                // (they appear later in the vertex buffer, and depth test is LessEqual).

                if log::log_enabled!(log::Level::Debug) && !logged_first_bbox {
                    let (min_x_m, min_y_m, max_x_m, max_y_m) = if let Some(bounds) = actual_bounds {
                        (bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y)
                    } else {
                        let bbox = geom.extent;
                        let (min_x_m, min_y_m) =
                            crate::tiles::latlon_to_mercator(bbox.min_y, bbox.min_x);
                        let (max_x_m, max_y_m) =
                            crate::tiles::latlon_to_mercator(bbox.max_y, bbox.max_x);
                        (min_x_m, min_y_m, max_x_m, max_y_m)
                    };
                    log::debug!(
                        "  first area bbox (stored): lon=({:.4},{:.4}) lat=({:.4},{:.4}) degenerate={}",
                        geom.extent.min_x,
                        geom.extent.max_x,
                        geom.extent.min_y,
                        geom.extent.max_y,
                        bbox_is_degenerate(geom.extent)
                    );
                    log::debug!(
                        "  first area bbox (Mercator): ({:.0},{:.0})-({:.0},{:.0})",
                        min_x_m,
                        min_y_m,
                        max_x_m,
                        max_y_m
                    );
                    logged_first_bbox = true;
                }

                // Spatial prefilter: classify bbox vs tile relationship
                let relation = if let Some(actual_bounds) = actual_bounds {
                    tile_relation_mercator(actual_bounds, *bounds)
                } else {
                    geom.extent.tile_relation(bounds)
                };
                if relation == BBoxTileRelation::Outside {
                    self.log_skip(info, feature_index, feature, "outside-tile");
                    continue;
                }
                stats.bbox_pass += 1;

                // DRGARE (dredged area) and DEPARE share LUP group 1 → priority 0
                // in S-52. OpenCPN's razRules 2-D table resolves the tie by LUP
                // load order so DRGARE paints on top of the enclosing DEPARE.
                // navcore collapses the tie to OSENC feature order, which makes
                // marina basins flip to the enclosing-DEPARE colour (DEPDW pale
                // blue, which reads as "white"). Bump DRGARE by +1 so its shallow
                // DEPVS shade always paints last.
                let feature_priority = if feature.object_class == ObjectClass::DredgedArea {
                    (resolved.priority + 1).min(9)
                } else {
                    resolved.priority
                };
                let vert_start = packet.area_vertices.len();
                let pat_start = packet.pattern_vertices.len();

                // Extract color index from resolved instructions (first AC instruction)
                let color_index_opt =
                    self.extract_area_color_index(feature, engine, &resolved.instructions);

                // Extract AP() pattern id from resolved instructions.
                // Do not send PRTSUR01 through the generic area-pattern renderer:
                // it is a special horizontal survey/no-data pattern, and our tiled
                // AP shader turns it into large false translucent blocks.
                let pattern_id = resolved.instructions.iter().find_map(|instr| {
                    if let RenderInstruction::AreaPattern { pattern } = instr {
                        if pattern == "PRTSUR01" {
                            return None;
                        }
                        crate::render::pattern_id_from_name(pattern)
                    } else {
                        None
                    }
                });

                // An area with a pattern but no AC() — M_QUAL's survey-quality
                // overlay is `AP(DQUALA11);LS(DASH,2,CHGRD)` — still has a fill
                // to draw: the pattern. Requiring a colour dropped the pattern
                // with it. `color_index` of None means "no solid fill", not
                // "nothing to draw"; only areas with neither are skipped.
                if let Some(color_index) = color_index_opt.or(pattern_id.map(|_| 0)) {
                    let solid_fill = color_index_opt.is_some();
                    if relation == BBoxTileRelation::FullyInside {
                        // FAST PATH: feature bbox fully inside tile — emit vertices directly
                        // No clipping, no f64 conversion, no heap allocations
                        // (the quilt is resolved by draw order, not by masking)
                        stats.fast_path_features += 1;
                        let (ox, oy) = bounds.center();
                        geom.for_each_triangle_direct(ref_mx - ox, ref_my - oy, |tri| {
                            if self.below_min_triangle_area(&tri) {
                                stats.subpixel_triangles += 1;
                                return;
                            }
                            for pos in tri {
                                if solid_fill {
                                    packet.area_vertices.push(AreaVertex {
                                        position: pos,
                                        color_index: color_index as u32,
                                        disp_prio: feature_priority as u32,
                                        shade: 0.0,
                                    });
                                }
                                if let Some(pid) = pattern_id {
                                    packet.pattern_vertices.push(crate::render::PatternVertex {
                                        position: pos,
                                        pattern_id: pid,
                                        offset: [0.0, 0.0],
                                        disp_prio: feature_priority as u32,
                                    });
                                }
                            }
                        });
                    } else {
                        // SLOW PATH: feature straddles tile boundary — per-triangle clipping
                        stats.slow_path_features += 1;
                        geom.for_each_triangle_global(info.ref_lat, info.ref_lon, |tri| {
                            stats.triangles_seen += 1;

                            if self.below_min_triangle_area_f64(&tri) {
                                stats.subpixel_triangles += 1;
                                return;
                            }

                            // No coverage masking: draw order resolves the quilt.

                            // Per-triangle outcode fast-accept/reject before full SH clipping
                            let oc0 = outcode(tri[0], bounds);
                            let oc1 = outcode(tri[1], bounds);
                            let oc2 = outcode(tri[2], bounds);

                            if oc0 & oc1 & oc2 != 0 {
                                // Trivial reject: all vertices outside same edge
                                stats.trivial_reject_triangles += 1;
                                return;
                            }

                            if oc0 | oc1 | oc2 == 0 {
                                // Trivial accept: all vertices inside tile
                                stats.trivial_accept_triangles += 1;
                                for &vertex in &tri {
                                    let local = self.global_to_vertex(vertex, bounds);
                                    if solid_fill {
                                        packet.area_vertices.push(AreaVertex {
                                            position: local,
                                            color_index: color_index as u32,
                                            disp_prio: feature_priority as u32,
                                            shade: 0.0,
                                        });
                                    }
                                    if let Some(pid) = pattern_id {
                                        packet.pattern_vertices.push(
                                            crate::render::PatternVertex {
                                                position: local,
                                                pattern_id: pid,
                                                offset: [0.0, 0.0],
                                                disp_prio: feature_priority as u32,
                                            },
                                        );
                                    }
                                }
                                return;
                            }

                            // Full Sutherland-Hodgman clipping needed
                            let clipped = clip_triangle(tri, bounds);
                            if !clipped.is_empty() {
                                stats.triangles_clipped += 1;
                                let triangulated = triangulate_fan(&clipped);
                                for vertex in &triangulated {
                                    let local = self.global_to_vertex(*vertex, bounds);
                                    if solid_fill {
                                        packet.area_vertices.push(AreaVertex {
                                            position: local,
                                            color_index: color_index as u32,
                                            disp_prio: feature_priority as u32,
                                            shade: 0.0,
                                        });
                                    }
                                    if let Some(pid) = pattern_id {
                                        packet.pattern_vertices.push(
                                            crate::render::PatternVertex {
                                                position: local,
                                                pattern_id: pid,
                                                offset: [0.0, 0.0],
                                                disp_prio: feature_priority as u32,
                                            },
                                        );
                                    }
                                }
                            }
                        });
                    }

                    // Depth relief: shade a groove along this depth area's own
                    // boundary. Emitted after the fill so it overwrites it.
                    //
                    // Skipped for the same features whose boundary strokes are
                    // suppressed below: an incompletely-surveyed area's rings
                    // run right across the cell, including under the land, and
                    // a groove there is a line no chart contains.
                    if solid_fill
                        && !has_nodata_fill
                        && !has_prtsur01
                        && self.depth_relief_enabled()
                        && is_depth_area(feature)
                    {
                        self.add_depth_relief_band(
                            chart,
                            geom,
                            &coast_edges,
                            bounds,
                            packet,
                            ref_mx,
                            ref_my,
                            color_index as u32,
                            feature_priority,
                        );
                    }

                    let senc_end = packet.area_vertices.len();
                    // Re-tessellate only when the SENC carries no tessellation
                    // at all. "The stored triangles emitted nothing for this
                    // tile" is the *normal* case for a feature whose bounding
                    // box overlaps the tile but whose polygon does not, and
                    // re-running earcut over the whole ring set then fills the
                    // concavities and holes the real tessellation excludes: an
                    // OBSTRN at Mosede painted 250x95px of harbour DEPVS that
                    // the reference leaves as the surrounding DEPMS.
                    if solid_fill
                        && packet.area_vertices.len() == vert_start
                        && geom.triangles.is_empty()
                    {
                        self.add_area_ring_fill_fallback(
                            chart,
                            geom,
                            bounds,
                            packet,
                            ref_mx,
                            ref_my,
                            color_index as u32,
                            pattern_id,
                            feature_priority,
                            relation,
                        );
                    }

                    if let Some(log) = &self.scene_log {
                        let source = if senc_end > vert_start {
                            if stats.triangles_clipped > 0 {
                                super::scene::AreaSource::SencTrianglesClipped
                            } else {
                                super::scene::AreaSource::SencTriangles
                            }
                        } else {
                            super::scene::AreaSource::RingFallback
                        };
                        let (min_x, min_y) = super::latlon_to_mercator(
                            geom.extent.min_y,
                            geom.extent.min_x,
                        );
                        let (max_x, max_y) = super::latlon_to_mercator(
                            geom.extent.max_y,
                            geom.extent.max_x,
                        );
                        // Vertices are tile-relative (see global_to_vertex);
                        // the log is in global Mercator, so add the origin back.
                        let (ox, oy) = bounds.center();
                        let g = |p: [f32; 2]| [p[0] as f64 + ox, p[1] as f64 + oy];
                        let tris: Vec<[[f64; 2]; 3]> = packet.area_vertices
                            [vert_start..packet.area_vertices.len()]
                            .chunks_exact(3)
                            .map(|t| [g(t[0].position), g(t[1].position), g(t[2].position)])
                            .collect();
                        let mut log = log.lock().unwrap();
                        log.begin_feature(
                            &info.path.file_stem().unwrap_or_default().to_string_lossy(),
                            feature_index,
                            crate::senc::s57_code_to_acronym(feature.type_code),
                            [min_x, min_y, max_x, max_y],
                        );
                        log.record_area(
                            source,
                            feature_priority,
                            color_index as u32,
                            info.native_scale,
                            is_background,
                            tris,
                        );
                        log.end_feature();
                    }
                } else if self.scene_log.is_some() {
                    self.log_skip(info, feature_index, feature, "no-area-colour");
                } // end color_index gate

                if has_prtsur01 {
                    self.add_prtsur01_hatches(
                        chart,
                        geom,
                        bounds,
                        packet,
                        ref_mx,
                        ref_my,
                        feature_priority,
                        is_background,
                    );
                }

                // Track vertex range and priority for this feature
                let vert_end = packet.area_vertices.len();
                let pat_end = packet.pattern_vertices.len();
                if vert_end > vert_start || pat_end > pat_start {
                    all_area_ranges.push((
                        feature_priority,
                        is_background,
                        vert_start,
                        vert_end,
                        pat_start,
                        pat_end,
                    ));
                }

                // Extract boundary line styles from resolved instructions (already CS-expanded).
                // For incompletely surveyed / no-data depth areas, OpenCPN does not present
                // the dense internal polygon-edge mesh the way our per-feature stroking does.
                // Suppress those internal boundary strokes and leave the area portrayal.
                let draw_chart_outline = feature.object_class == ObjectClass::Coverage
                    && self
                        .s52_engine
                        .map(|e| e.settings.show_chart_boundaries)
                        .unwrap_or(false);
                let boundary_priority = if draw_chart_outline {
                    9
                } else {
                    feature_priority
                };
                let suppress_area_boundary_lines =
                    feature.is_depth_area() && (has_nodata_fill || has_prtsur01);
                let mut line_styles: Vec<(u8, LineStyleKey)> = Vec::new();
                let mut lc_style_ops: Vec<(u8, String, String)> = Vec::new();
                if draw_chart_outline {
                    // OpenCPN's visible green box is closer to the chart extent outline
                    // than to the raw M_COVR polygon. Draw only that rectangle, and
                    // skip the ordinary HODATA01/coverage-line path for these meta features.
                    line_styles.push((0, LineStyleKey::new(LinePattern::Solid, 2, "UINFG")));
                } else if !matches!(
                    feature.object_class,
                    ObjectClass::Coverage | ObjectClass::CompilationScale
                ) && !suppress_area_boundary_lines
                {
                    let mut line_pass: u8 = 0;
                    for instr in &resolved.instructions {
                        match instr {
                            RenderInstruction::LineStyle {
                                pattern,
                                width,
                                color,
                            } => {
                                line_styles
                                    .push((line_pass, LineStyleKey::new(*pattern, *width, color)));
                                line_pass += 1;
                            }
                            RenderInstruction::LineComplex { name } => {
                                let color_ref = get_line_style_table()
                                    .and_then(|table| table.get(name))
                                    .map(|symbol| symbol.color_ref.clone())
                                    .unwrap_or_else(|| "CHBLK".to_string());
                                lc_style_ops.push((line_pass, name.clone(), color_ref));
                                line_pass += 1;
                            }
                            _ => {}
                        }
                    }
                }

                if !line_styles.is_empty() || !lc_style_ops.is_empty() {
                    if log::log_enabled!(log::Level::Debug) {
                        log::debug!(
                            "AREA-BOUNDARY: {} (code {}) chart_scale={} — LS:{:?} LC:{:?}",
                            s57_code_to_acronym(feature.type_code),
                            feature.type_code,
                            info.native_scale,
                            line_styles.iter().map(|(_, k)| format!("{:?}", k)).collect::<Vec<_>>(),
                            lc_style_ops.iter().map(|(_, name, _)| name.as_str()).collect::<Vec<_>>(),
                        );
                    }
                    let rings = if draw_chart_outline {
                        // Rings are chart-local (relative to the chart's
                        // reference point), like `resolve_rings` returns.
                        let e = &info.extent_mercator;
                        let (min_x, min_y) = ((e.min_x - ref_mx) as f32, (e.min_y - ref_my) as f32);
                        let (max_x, max_y) = ((e.max_x - ref_mx) as f32, (e.max_y - ref_my) as f32);
                        vec![vec![
                            [min_x, min_y],
                            [max_x, min_y],
                            [max_x, max_y],
                            [min_x, max_y],
                            [min_x, min_y],
                        ]]
                    } else if !coast_edges.is_empty() && (boundary_priority as i32) < 7 {
                        // Suppress boundary segments that coincide with the coast
                        // (shared with a higher-priority coastline-former).
                        geom.resolve_rings_excluding(&chart.edge_table, &coast_edges)
                    } else {
                        geom.resolve_rings(&chart.edge_table)
                    };
                    // One LC() batch per pass, for the symbols the table has.
                    let lc_keys: Vec<LineBatchKey> = lc_style_ops
                        .iter()
                        .filter_map(|(pass_idx, symbol_name, color_ref)| {
                            let lc = crate::render::lc_symbol_index(symbol_name)?;
                            Some(LineBatchKey::new_with_priority(
                                boundary_priority,
                                *pass_idx,
                                LineStyleKey::complex(lc, color_ref.as_str()),
                                0,
                                is_background,
                            ))
                        })
                        .collect();

                    for ring in &rings {
                        let mut global: Vec<[f64; 2]> = ring
                            .iter()
                            .map(|p| [ref_mx + p[0] as f64, ref_my + p[1] as f64])
                            .collect();
                        // Close only a genuine ring. `resolve_rings` breaks a
                        // boundary into open paths wherever an edge is dropped
                        // (shared with the coast) or the node chain is
                        // discontinuous, and joining those ends draws a segment
                        // no chart contains — a restricted-area corridor
                        // reaching the shore came out as a straight magenta
                        // chord across several kilometres of land.
                        close_open_ring_if_rounding_gap(&mut global);
                        if !lc_keys.is_empty() {
                            let lc_ring = lc_direction(global.clone());
                            for (segment, start_arc) in clip_polyline_arc(&lc_ring, bounds) {
                                if segment.len() < 2 {
                                    continue;
                                }
                                let pts: Vec<[f32; 2]> = segment
                                    .iter()
                                    .map(|p| self.global_to_vertex(*p, bounds))
                                    .collect();
                                for key in &lc_keys {
                                    lc_polylines
                                        .entry(key.clone())
                                        .or_default()
                                        .push((pts.clone(), start_arc as f32));
                                }
                            }
                        }
                        let clipped = clip_polyline(&global, bounds);
                        for segment in clipped {
                            if segment.len() < 2 {
                                continue;
                            }
                            let pts: Vec<[f32; 2]> = segment
                                .iter()
                                .map(|p| self.global_to_vertex(*p, bounds))
                                .collect();
                            if let Some(log) = &self.scene_log {
                                if let Some((_, style)) = line_styles.first() {
                                    log.lock().unwrap().record_line(
                                        &info
                                            .path
                                            .file_stem()
                                            .unwrap_or_default()
                                            .to_string_lossy(),
                                        feature_index,
                                        crate::senc::s57_code_to_acronym(feature.type_code),
                                        super::scene::LineSource::AreaBoundary,
                                        boundary_priority,
                                        is_background,
                                        style_label(style),
                                        vec![segment.clone()],
                                    );
                                }
                            }
                            for (pass_idx, style) in &line_styles {
                                let key = LineBatchKey::new_with_priority(
                                    boundary_priority,
                                    *pass_idx,
                                    style.clone(),
                                    0,
                                    is_background,
                                );
                                let (vertices, indices) =
                                    build_line_vertices_multi_indexed(&[pts.clone()]);
                                if !vertices.is_empty() {
                                    packet.line_batches.push(LineBatch {
                                        key,
                                        vertices,
                                        indices,
                                    });
                                }
                            }
                        }
                    }
                }

                // Collect text label candidates from resolved instructions (deferred layout)
                for instr in &resolved.instructions {
                    if let RenderInstruction::Text {
                        attribute,
                        format,
                        hjust,
                        vjust,
                        xoffs,
                        yoffs,
                        color: color_token,
                        weight,
                        bsize,
                        space,
                        dis,
                        ..
                    } = instr
                    {
                        if self.should_skip_text(*dis) {
                            continue;
                        }
                        let text_value =
                            get_text_for_attribute(feature, attribute, format.as_deref());
                        if let Some(text) = text_value {
                            let padding_m = 500.0 * self.view_meters_per_pixel as f64;
                            if cx >= bounds.min_x - padding_m
                                && cx <= bounds.max_x + padding_m
                                && cy >= bounds.min_y - padding_m
                                && cy <= bounds.max_y + padding_m
                            {
	                                let color = engine
	                                    .get_color(color_token)
	                                    .unwrap_or([0.0, 0.0, 0.0, 1.0]);
	                                let color_index =
	                                    engine.get_color_index(color_token).unwrap_or(0) as u32;
	                                label_candidates.push(TextParams {
	                                    position: self.global_to_vertex([cx, cy], bounds),
	                                    text,
	                                    color,
	                                    color_index,
	                                    scale: s52_text_scale(*bsize, *weight, self.view_ppmm),
	                                    bold: *weight >= 6,
	                                    space: *space,
                                    hjust: HJust::from(*hjust),
                                    vjust: VJust::from(*vjust),
                                    xoffs: *xoffs as i32,
                                    yoffs: *yoffs as i32,
                                    disp_prio: feature_priority,
                                    dis: *dis,
                                });
                            }
                        }
                    }
                }
            }
        }

        stats.vertices_added = packet.area_vertices.len().saturating_sub(before_verts);

        stats
    }

    fn add_prtsur01_hatches(
        &self,
        chart: &ChartData,
        geom: &crate::senc::AreaGeometry,
        bounds: &TileBounds,
        packet: &mut TilePacket,
        ref_mx: f64,
        ref_my: f64,
        priority: u8,
        is_background: bool,
    ) {
        let rings = geom.resolve_rings(&chart.edge_table);
        if rings.is_empty() {
            return;
        }

        let global_rings: Vec<Vec<[f64; 2]>> = rings
            .iter()
            .map(|ring| {
                let mut global: Vec<[f64; 2]> = ring
                    .iter()
                    .map(|p| [ref_mx + p[0] as f64, ref_my + p[1] as f64])
                    .collect();
                close_ring_if_needed(&mut global);
                global
            })
            .filter(|ring| ring.len() >= 4)
            .collect();
        if global_rings.is_empty() {
            return;
        }

        let polylines = prtsur01_hatch_polylines(
            &global_rings,
            bounds,
            self.view_meters_per_pixel as f64,
            self.view_ppmm as f64,
        );
        if polylines.is_empty() {
            return;
        }

        let local_polylines: Vec<Vec<[f32; 2]>> = polylines
            .into_iter()
            .map(|line| {
                line.into_iter()
                    .map(|point| self.global_to_vertex(point, bounds))
                    .collect()
            })
            .collect();
        let (vertices, indices) = build_line_vertices_multi_indexed(&local_polylines);
        if vertices.is_empty() {
            return;
        }

        packet.line_batches.push(LineBatch {
            key: LineBatchKey::new_with_priority(
                priority,
                0,
                LineStyleKey::new(LinePattern::Solid, 2, "CHGRD"),
                0,
                is_background,
            ),
            vertices,
            indices,
        });
    }

    fn depth_relief_enabled(&self) -> bool {
        self.s52_engine
            .map(|e| e.settings.depth_relief)
            .unwrap_or(false)
    }

    /// Shade a soft groove along a depth area's boundary.
    ///
    /// The band straddles the edge — darkest on the line, fading to nothing at
    /// `RELIEF_WIDTH_PX` either side — which reads as the step between one
    /// depth band and the next. Depth areas are nested, each boundary being a
    /// surveyed contour, so shading every one gives the terraced look without
    /// inventing anything about the seabed between them.
    ///
    /// Straddling rather than shading only the inside is deliberate: it needs
    /// no notion of which side is "in". `resolve_rings` returns open paths
    /// wherever the node chain breaks or an edge is shared, and an inward-only
    /// band silently skipped every one of those — which in practice was most of
    /// the large depth areas, leaving the effect visible only in harbours.
    #[allow(clippy::too_many_arguments)]
    fn add_depth_relief_band(
        &self,
        chart: &ChartData,
        geom: &crate::senc::AreaGeometry,
        coast_edges: &HashSet<u32>,
        bounds: &TileBounds,
        packet: &mut TilePacket,
        ref_mx: f64,
        ref_my: f64,
        color_index: u32,
        priority: u8,
    ) {
        /// Half-width of the groove, in nominal pixels.
        const RELIEF_HALF_WIDTH_PX: f64 = 5.0;
        /// How dark the centre line is. Enough to read as a step, not enough to
        /// be mistaken for a different depth shade.
        const RELIEF_SHADE: f32 = 0.22;

        let half = RELIEF_HALF_WIDTH_PX * self.view_meters_per_pixel as f64;
        if half <= 0.0 {
            return;
        }

        // Edges shared with the coast are dropped: the groove is meant to mark
        // the step from one depth band to the next, and the shoreline already
        // carries its own coastline stroke. It also keeps the band off the
        // land, where a depth area's ring can run for kilometres beneath a
        // neighbouring chart's LNDARE.
        for ring in geom.resolve_rings_excluding(&chart.edge_table, coast_edges) {
            let pts: Vec<[f64; 2]> = ring
                .iter()
                .map(|p| [ref_mx + p[0] as f64, ref_my + p[1] as f64])
                .collect();
            for w in pts.windows(2) {
                let (a, b) = (w[0], w[1]);
                let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
                let len = (dx * dx + dy * dy).sqrt();
                if len < 1e-9 {
                    continue;
                }
                let (nx, ny) = (-dy / len * half, dx / len * half);
                let quad = [
                    [a[0] + nx, a[1] + ny],
                    [b[0] + nx, b[1] + ny],
                    [b[0] - nx, b[1] - ny],
                    [a[0] - nx, a[1] - ny],
                ];
                if quad.iter().all(|p| p[0] > bounds.max_x)
                    || quad.iter().all(|p| p[0] < bounds.min_x)
                    || quad.iter().all(|p| p[1] > bounds.max_y)
                    || quad.iter().all(|p| p[1] < bounds.min_y)
                {
                    continue;
                }
                let v = |p: [f64; 2], shade: f32| AreaVertex {
                    position: self.global_to_vertex(p, bounds),
                    color_index,
                    disp_prio: priority as u32,
                    shade,
                };
                // Two triangles per side, meeting on the boundary itself.
                for (outer0, outer1) in [(quad[0], quad[1]), (quad[3], quad[2])] {
                    packet.area_vertices.push(v(a, RELIEF_SHADE));
                    packet.area_vertices.push(v(b, RELIEF_SHADE));
                    packet.area_vertices.push(v(outer1, 0.0));
                    packet.area_vertices.push(v(a, RELIEF_SHADE));
                    packet.area_vertices.push(v(outer1, 0.0));
                    packet.area_vertices.push(v(outer0, 0.0));
                }
            }
        }
    }

    fn add_area_ring_fill_fallback(
        &self,
        chart: &ChartData,
        geom: &crate::senc::AreaGeometry,
        bounds: &TileBounds,
        packet: &mut TilePacket,
        ref_mx: f64,
        ref_my: f64,
        color_index: u32,
        pattern_id: Option<u32>,
        priority: u8,
        relation: BBoxTileRelation,
    ) {
        let rings = geom.resolve_rings(&chart.edge_table);
        if rings.is_empty() {
            return;
        }

        let mut vertices_flat = Vec::new();
        let mut points = Vec::new();
        let mut hole_indices = Vec::new();

        for ring in rings {
            let mut global: Vec<[f64; 2]> = ring
                .iter()
                .map(|p| [ref_mx + p[0] as f64, ref_my + p[1] as f64])
                .collect();
            close_ring_if_needed(&mut global);
            if global.len() >= 2 && global.first() == global.last() {
                global.pop();
            }
            if global.len() < 3 {
                continue;
            }

            if !points.is_empty() {
                hole_indices.push(points.len());
            }
            for point in global {
                vertices_flat.push(point[0]);
                vertices_flat.push(point[1]);
                points.push(point);
            }
        }

        if points.len() < 3 {
            return;
        }

        let Ok(indices) = earcutr::earcut(&vertices_flat, &hole_indices, 2) else {
            log::debug!("area ring fallback tessellation failed");
            return;
        };

        for tri_idx in indices.chunks_exact(3) {
            let tri = [points[tri_idx[0]], points[tri_idx[1]], points[tri_idx[2]]];
            if triangle_area_abs(tri) < 1e-6 {
                continue;
            }

            if relation == BBoxTileRelation::FullyInside {
                for vertex in tri {
                    let local = self.global_to_vertex(vertex, bounds);
                    push_area_and_pattern_vertex(
                        packet,
                        local,
                        color_index,
                        pattern_id,
                        priority,
                    );
                }
                continue;
            }

            let clipped = clip_triangle(tri, bounds);
            if clipped.is_empty() {
                continue;
            }
            for vertex in triangulate_fan(&clipped) {
                let local = self.global_to_vertex(vertex, bounds);
                push_area_and_pattern_vertex(packet, local, color_index, pattern_id, priority);
            }
        }
    }

    /// Build line segments with spatial prefilter and clipping.
    /// Groups features by style using S-52 lookup tables.
    /// Populates shared `polylines_by_key` and `lc_patterns` collections
    /// which are finalized into batches by `build_cpu_impl()` after all charts.
    #[allow(clippy::type_complexity)]
    fn build_lines(
        &self,
        chart: &ChartData,
        info: &ChartInfo,
        bounds: &TileBounds,
        polylines_by_key: &mut HashMap<LineBatchKey, Vec<Vec<[f32; 2]>>>,
        lc_polylines: &mut LcPolylines,
        _detailed_coverages: &[[[f64; 2]; 3]],
        is_background: bool,
        label_candidates: &mut Vec<TextParams>,
    ) -> LineBuildStats {
        let mut stats = LineBuildStats::default();

        // The chart's own safety contour — the next contour it actually has
        // at or deeper than the setting — decided once for all its lines.
        let safety_ctx = crate::s52::CsContext {
            safety_contour: self
                .s52_engine
                .and_then(|e| e.chart_safety_contour(&chart.features)),
            ..Default::default()
        };

        // Enumerated over all features, not over `chart.lines()`, so the index
        // matches the `chart#index` ids `--dump-ir` emits and a finding in one
        // tool can be looked up in the other.
        for (feature_index, feature) in chart
            .features
            .iter()
            .enumerate()
            .filter(|(_, f)| f.feature_type == crate::senc::FeatureType::Line)
        {
            stats.total_features += 1;

            // Cheapest test first — see the note in `build_areas`.
            if let Some(ref geom) = feature.line_geometry {
                if !bbox_is_degenerate(geom.extent)
                    && geom.extent.tile_relation(bounds) == BBoxTileRelation::Outside
                {
                    continue;
                }
            }

            // SCAMIN/SCAMAX filtering: bypass SCAMIN for DISPLAYBASE features
            let bypass_scamin = self
                .s52_engine
                .map(|e| {
                    e.get_display_category(feature.type_code, GeometryType::Line)
                        == Some(DisplayCategory::Displaybase)
                        // DEPCNT02 clears the safety contour's SCAMIN outright
                        // (`Scamin = 1e8+1`). It is the one line that must be on
                        // the chart at every zoom.
                        || e.is_promoted_safety_contour(feature, safety_ctx.safety_contour)
                })
                .unwrap_or(false);
            let scamin_scale = should_render_at_scale_ex(
                feature,
                self.view_scale_denominator(),
                bypass_scamin,
                false,
                self.super_scamin_scale(info),
            );
            if scamin_scale == 0.0 {

                continue;
            }

            if let Some(ref geom) = feature.line_geometry {
                stats.with_geometry += 1;

                // No coverage masking: the bg/fg split puts coarser charts
                // under finer ones and draw order settles it.

                let acronym = s57_code_to_acronym(feature.type_code);

                // === Unified S-52 resolution via resolve_feature() ===
                // Uses lookup_best_fast (skips attrs alloc for generic entries),
                // handles display category filtering, and CS expansion in one call.
                let lookup_result: Option<(
                    u8,
                    u32,
                    Vec<(u8, LineStyleKey, Option<(String, String)>)>,
                )> = if let Some(engine) = self.s52_engine {
                    let ctx = if feature.type_code == 43 {
                        &safety_ctx
                    } else {
                        crate::s52::CsContext::EMPTY
                    };
                    let resolved = match engine.resolve_feature_ctx(feature, GeometryType::Line, ctx) {
                        Some(r) => r,
                        None => {
                            // No LUP match or filtered by display category
                            stats.skipped_other += 1;
                            continue;
                        }
                    };

                    let disp_prio = resolved.priority;

                    log::debug!(
                        "LINE-FEATURE: {} (code {}) chart='{}' scale={} prio={}",
                        acronym, feature.type_code, info.name, info.native_scale, disp_prio
                    );

                    // Extract line style ops from resolved (CS-expanded) instructions
                    let mut style_ops: Vec<(u8, LineStyleKey, Option<(String, String)>)> =
                        Vec::new();
                    let mut pass_idx: u8 = 0;

                    for instr in &resolved.instructions {
                        match instr {
                            RenderInstruction::LineStyle {
                                pattern,
                                width,
                                color,
                            } => {
                                stats.styled_by_ls += 1;
                                style_ops.push((
                                    pass_idx,
                                    LineStyleKey::new(*pattern, *width, color),
                                    None,
                                ));
                                pass_idx += 1;
                            }
                            RenderInstruction::LineComplex { name } => {
                                if let Some(lc_table) = get_line_style_table() {
                                    if let Some(symbol) = lc_table.get(name) {
                                        stats.styled_by_lc += 1;
                                        log::debug!(
                                            "LC({}) for {} -> color={}",
                                            name,
                                            acronym,
                                            symbol.color_ref
                                        );
                                        style_ops.push((
                                            pass_idx,
                                            LineStyleKey::new(
                                                LinePattern::Solid,
                                                1,
                                                &symbol.color_ref,
                                            ),
                                            Some((name.clone(), symbol.color_ref.clone())),
                                        ));
                                    } else {
                                        stats.styled_by_lc_missing += 1;
                                        log::trace!(
                                            "LC({}) symbol not found for {}",
                                            name,
                                            acronym
                                        );
                                        style_ops.push((
                                            pass_idx,
                                            LineStyleKey::new(LinePattern::Solid, 1, "CHBLK"),
                                            None,
                                        ));
                                    }
                                } else {
                                    stats.styled_by_lc_fallback += 1;
                                    log::trace!("LC({}) no table for {}", name, acronym);
                                    style_ops.push((
                                        pass_idx,
                                        LineStyleKey::new(LinePattern::Solid, 1, "CHBLK"),
                                        None,
                                    ));
                                }
                                pass_idx += 1;
                            }
                            _ => {} // Skip non-line instructions (SY, AC, etc.)
                        }
                    }

                    // Collect TX/TE text instructions for line labels (e.g. VALDCO on depth contours).
                    // Place labels at arc-length midpoints of each polyline so a long contour
                    // gets multiple labels along its length, matching OpenCPN's visual output.
                    // Falls back to extent-center for degenerate geometry.
                    {
                        let padding_m = 500.0 * self.view_meters_per_pixel as f64;
                        let (ref_mx, ref_my) = crate::tiles::latlon_to_mercator(info.ref_lat, info.ref_lon);
                        let polylines_global: Vec<Vec<[f64; 2]>> =
                            geom.resolve_global(&chart.edge_table, info.ref_lat, info.ref_lon);
                        let label_positions: Vec<(f64, f64)> = if polylines_global.is_empty() {
                            // Fallback: extent center
                            let ext = &geom.extent;
                            vec![(
                                ref_mx + (ext.min_x as f64 + ext.max_x as f64) * 0.5,
                                ref_my + (ext.min_y as f64 + ext.max_y as f64) * 0.5,
                            )]
                        } else {
                            polylines_global
                                .iter()
                                .filter_map(|pl| polyline_midpoint(pl))
                                .collect()
                        };

                        for instr in &resolved.instructions {
                            if let RenderInstruction::Text {
                                attribute,
                                format,
                                hjust,
                                vjust,
                                xoffs,
                                yoffs,
                                color: color_token,
                                weight,
                                bsize,
                                space,
                                dis,
                                ..
                            } = instr
                            {
                                if self.should_skip_text(*dis) {
                                    continue;
                                }
                                let Some(text) =
                                    get_text_for_attribute(feature, attribute, format.as_deref())
                                else {
                                    continue;
                                };
	                                let color = engine
	                                    .get_color(color_token)
	                                    .unwrap_or([0.0, 0.0, 0.0, 1.0]);
	                                let color_index =
	                                    engine.get_color_index(color_token).unwrap_or(0) as u32;
	                                for &(cx, cy) in &label_positions {
                                    if cx < bounds.min_x - padding_m
                                        || cx > bounds.max_x + padding_m
                                        || cy < bounds.min_y - padding_m
                                        || cy > bounds.max_y + padding_m
                                    {
                                        continue;
                                    }
                                    label_candidates.push(TextParams {
                                        position: self.global_to_vertex([cx, cy], bounds),
	                                        text: text.clone(),
	                                        color,
	                                        color_index,
	                                        scale: s52_text_scale(*bsize, *weight, self.view_ppmm),
	                                        bold: *weight >= 6,
	                                        space: *space,
                                        hjust: HJust::from(*hjust),
                                        vjust: VJust::from(*vjust),
                                        xoffs: *xoffs as i32,
                                        yoffs: *yoffs as i32,
                                        disp_prio: disp_prio,
                                        dis: *dis,
                                    });
                                }
                            }
                        }
                    }

                    if style_ops.is_empty() {
                        None
                    } else {
                        Some((disp_prio, 0, style_ops))
                    }
                } else {
                    // No S-52 engine - fallback to legacy path
                    let fallback_key =
                        legacy_line_style(feature, self.s52_engine, safety_ctx.safety_contour);
                    if let Some(key) = fallback_key {
                        Some((4, 0, vec![(0u8, key, None)]))
                    } else {
                        stats.skipped_no_lookup += 1;
                        continue;
                    }
                };

                // Extract result or skip
                let (disp_prio, lookup_id, style_ops) = match lookup_result {
                    Some(r) => r,
                    None => continue,
                };

                // Skip features with no style operations
                if style_ops.is_empty() {
                    continue;
                }

                // Spatial prefilter: skip features outside tile
                if !geom.extent.intersects_tile(bounds) {
                    continue;
                }
                stats.bbox_pass += 1;

                let polylines = geom.resolve_global(&chart.edge_table, info.ref_lat, info.ref_lon);

                // Track LC polylines for this feature, with the arc length
                // each piece starts at along its feature line.
                let mut feature_lc_polylines: Vec<(Vec<[f32; 2]>, f32)> = Vec::new();
                // The first LC() of the feature: (symbol, colour, pass).
                let feature_lc_info: Option<(String, String, u8)> =
                    style_ops.iter().find_map(|(pass, _, lc)| {
                        lc.as_ref().map(|(sym, col)| (sym.clone(), col.clone(), *pass))
                    });
                let non_lc_keys: Vec<LineBatchKey> = style_ops
                    .iter()
                    .filter(|(_, _, lc)| lc.is_none())
                    .map(|(pass, key, _)| {
                        LineBatchKey::new_with_priority(
                            disp_prio,
                            *pass,
                            key.clone(),
                            lookup_id,
                            is_background,
                        )
                    })
                    .collect();

                // LOD: simplification tolerance = half a screen pixel in Mercator meters
                let simplify_eps = self.view_meters_per_pixel as f64 * 0.5;

                for polyline in polylines {
                    let clipped_segments = if non_lc_keys.is_empty() {
                        Vec::new()
                    } else {
                        clip_polyline(&polyline, bounds)
                    };

                    for segment in clipped_segments {
                        // No coverage masking: draw order resolves the quilt.

                        // Douglas-Peucker simplification: drop sub-pixel detail
                        let segment = super::clip::simplify_polyline(&segment, simplify_eps);
                        if segment.len() < 2 {
                            continue;
                        }
                        stats.segments_out += segment.len() - 1;

                        if let Some(log) = &self.scene_log {
                            if let Some((_, style, _)) = style_ops.first() {
                                log.lock().unwrap().record_line(
                                    &info.path.file_stem().unwrap_or_default().to_string_lossy(),
                                    feature_index,
                                    acronym,
                                    super::scene::LineSource::LineFeature,
                                    disp_prio,
                                    is_background,
                                    style_label(style),
                                    vec![segment.clone()],
                                );
                            }
                        }

                        let mut pts: Vec<[f32; 2]> = Vec::with_capacity(segment.len());
                        for p in segment {
                            pts.push(self.global_to_vertex(p, bounds));
                        }

                        // Distribute pts to the batch keys; move into the last.
                        if let Some((last, rest)) = non_lc_keys.split_last() {
                            for batch_key in rest {
                                polylines_by_key
                                    .entry(batch_key.clone())
                                    .or_default()
                                    .push(pts.clone());
                            }
                            polylines_by_key
                                .entry(last.clone())
                                .or_default()
                                .push(std::mem::take(&mut pts));
                        }
                    }

                    // The LC() line: clipped with its arc length along the whole
                    // line, and run in the direction the symbols face.
                    if feature_lc_info.is_some() {
                        let lc_line = lc_direction(polyline);
                        for (segment, start_arc) in clip_polyline_arc(&lc_line, bounds) {
                            let segment = super::clip::simplify_polyline(&segment, simplify_eps);
                            if segment.len() < 2 {
                                continue;
                            }
                            // An LC()-only line is recorded here; one that
                            // also has LS() was recorded with those.
                            if non_lc_keys.is_empty() {
                                if let (Some(log), Some((_, style, _))) =
                                    (&self.scene_log, style_ops.first())
                                {
                                    log.lock().unwrap().record_line(
                                        &info.path.file_stem().unwrap_or_default().to_string_lossy(),
                                        feature_index,
                                        acronym,
                                        super::scene::LineSource::LineFeature,
                                        disp_prio,
                                        is_background,
                                        style_label(style),
                                        vec![segment.clone()],
                                    );
                                }
                                stats.segments_out += segment.len() - 1;
                            }
                            let pts: Vec<[f32; 2]> = segment
                                .iter()
                                .map(|p| self.global_to_vertex(*p, bounds))
                                .collect();
                            feature_lc_polylines.push((pts, start_arc as f32));
                        }
                    }
                }

                // If this feature has LC pattern info, store for later processing
                if let Some((symbol_name, color_ref, pass)) = feature_lc_info {
                    if let Some(lc) = crate::render::lc_symbol_index(&symbol_name) {
                        if !feature_lc_polylines.is_empty() {
                            let key = LineBatchKey::new_with_priority(
                                disp_prio,
                                pass,
                                LineStyleKey::complex(lc, color_ref),
                                lookup_id,
                                is_background,
                            );
                            lc_polylines.entry(key).or_default().extend(feature_lc_polylines);
                        }
                    }
                }
            }
        }

        stats
    }

    /// Build symbol instances from point features with spatial prefilter
    fn build_symbols(
        &self,
        chart: &ChartData,
        info: &ChartInfo,
        bounds: &TileBounds,
        packet: &mut TilePacket,
        all_symbol_priorities: &mut Vec<(u8, usize)>,
        label_candidates: &mut Vec<TextParams>,
        detailed_coverages: &[[[f64; 2]; 3]],
    ) -> SymbolBuildStats {
        use crate::senc::FeatureType;

        let mut stats = SymbolBuildStats::default();

        // Track last light position to avoid duplicate text at co-located lights
        let mut last_light_pos: Option<(f64, f64)> = None;

        // The depth areas around this tile, for UDWHAZ03's "is this rock an
        // isolated danger?" — built only if the tile has a rock, wreck or
        // obstruction to ask about. Without it every danger was resolved
        // with no surroundings, and ISODGR51 could never be drawn.
        let mut depth_index: Option<crate::s52::DepthAreaIndex> = None;

        for feature in &chart.features {
            // Only process point features
            if feature.feature_type != FeatureType::Point {
                continue;
            }
            stats.total_features += 1;

            // Cheapest test first — see the note in `build_areas`. A point is
            // its own bounding box, so this is exact; the margin is for symbols
            // whose art extends beyond the pivot.
            if let Some(pg) = &feature.point_geometry {
                let (mx, my) = super::latlon_to_mercator(pg.x, pg.y);
                let margin = (bounds.max_x - bounds.min_x) * 0.05;
                if mx < bounds.min_x - margin
                    || mx > bounds.max_x + margin
                    || my < bounds.min_y - margin
                    || my > bounds.max_y + margin
                {
                    continue;
                }
            }

            // SCAMIN/SCAMAX filtering: bypass SCAMIN for DISPLAYBASE features
            let bypass_scamin = self
                .s52_engine
                .and_then(|e| e.get_display_category(feature.type_code, GeometryType::Point))
                .map(|cat| cat == DisplayCategory::Displaybase)
                .unwrap_or(false);
            let scamin_scale = should_render_at_scale_ex(
                feature,
                self.view_scale_denominator(),
                bypass_scamin,
                true,
                self.super_scamin_scale(info),
            );
            if scamin_scale == 0.0 {

                continue;
            }

            // --- Unified symbol resolution via S52Engine ---
            // All point features (including LIGHTS) go through resolve_feature()
            // which handles LUP lookup + CS procedure expansion in one step.
            let engine = match self.s52_engine {
                Some(e) => e,
                None => continue,
            };

            let danger_ctx = if matches!(feature.type_code, 86 | 153 | 159) {
                let index = depth_index.get_or_insert_with(|| {
                    let margin = (bounds.max_x - bounds.min_x) * 0.1;
                    crate::s52::DepthAreaIndex::build_near(
                        &chart.features,
                        chart.header.ref_lat,
                        chart.header.ref_lon,
                        Some((
                            [bounds.min_x - margin, bounds.min_y - margin],
                            [bounds.max_x + margin, bounds.max_y + margin],
                        )),
                    )
                });
                Some(index.context_for(feature))
            } else {
                None
            };
            let resolved = match engine.resolve_feature_ctx(
                feature,
                GeometryType::Point,
                danger_ctx.as_ref().unwrap_or(crate::s52::CsContext::EMPTY),
            ) {
                Some(r) => r,
                None => {
                    log::debug!("no LUP for point class={:?}", feature.object_class);
                    continue;
                }
            };

            // Emit every SY() instruction in the resolved (CS-expanded) list.
            // OpenCPN treats point portrayal as an ordered sequence of operations;
            // stopping at the first symbol drops supplementary marks from CS/LUPs.
            // Each entry is (atlas id, rotation in degrees). The rotation comes
            // from the instruction itself — SY(LIGHTS12,135) for a light flare,
            // SY(TSSLPT51,ORIENT) for a traffic-lane arrow that must point
            // along the lane.
            let mut symbol_ids: Vec<u32> = Vec::new();
            let mut symbol_rotations: Vec<Option<f64>> = Vec::new();
            for instr in &resolved.instructions {
                if let RenderInstruction::Symbol { name, rotation } = instr {
                    if let Some(id) = crate::render::symbols::symbol_id_from_s52_name(name) {
                        symbol_ids.push(id);
                        symbol_rotations.push(rotation.as_ref().and_then(|r| r.degrees(feature)));
                    } else {
                        log::debug!(
                            "symbol '{}' not in atlas for class={:?}",
                            name,
                            feature.object_class
                        );
                    }
                }
            }

            // LIGHTS special handling: rotation and sector info
            let (symbol_rotation_deg, orient_text, light_sector) =
                if feature.object_class == ObjectClass::Light {
                    let info = light_render_info(feature, &engine.settings);
                    let sector = light_sector_info(feature);
                    if sector.is_some() {
                        // Valid sector light: draw ONLY the bounded sector arc (matches
                        // OpenCPN s52cnsy.cpp:1280, which REPLACES the symbol command with
                        // the CA() arc). Suppress the all-round LIGHTS91/92/93 circle that
                        // would otherwise be drawn as a full ring on top of the arc.
                        symbol_ids.clear();
                        symbol_rotations.clear();
                    } else if symbol_ids.is_empty() {
                        // Non-sector light: fall back to the CS-selected flare/all-round symbol.
                        if let Some(id) =
                            crate::render::symbols::symbol_id_from_s52_name(info.symbol_name)
                        {
                            symbol_ids.push(id);
                            symbol_rotations.push(info.rotation_deg);
                        }
                    }
                    (info.rotation_deg, info.orient_text, sector)
                } else {
                    (None, None, None)
                };

            // Skip only when there is nothing to draw: no symbol AND no sector arc.
            // (A valid sector light has an empty symbol list but must still reach the
            // arc/label code below.)
            if symbol_ids.is_empty() && light_sector.is_none() {
                log::debug!("no atlas symbol for class={:?}", feature.object_class);
                continue;
            }
            stats.matched_symbol += 1;
            if symbol_ids.len() > 1 {
                stats.multi_symbol_features += 1;
                log::debug!(
                    "multi-symbol point class={:?} count={} instr={}",
                    feature.object_class,
                    symbol_ids.len(),
                    instruction_summary(&resolved.instructions)
                );
            }

            // Get point geometry
            let point_geom = match &feature.point_geometry {
                Some(pg) => pg,
                None => continue,
            };
            stats.with_geometry += 1;

            // Convert WGS84 (lat, lon) to global Mercator
            // OSENC stores: pg.x = latitude, pg.y = longitude (opposite of typical convention)
            let (mx, my) = super::latlon_to_mercator(point_geom.x, point_geom.y);

            if point_in_any_triangle([mx, my], detailed_coverages) {
                continue;
            }

            // Spatial prefilter: skip symbols outside tile bounds
            if mx < bounds.min_x || mx > bounds.max_x || my < bounds.min_y || my > bounds.max_y {
                continue;
            }
            stats.bbox_pass += 1;

            // Use priority from resolved feature (already computed by resolve_feature)
            let sym_priority = resolved.priority;

            for (sym_i, symbol_id) in symbol_ids.into_iter().enumerate() {
                let rotation_deg = symbol_rotations
                    .get(sym_i)
                    .copied()
                    .flatten()
                    .or(symbol_rotation_deg);
                let rotation = rotation_deg
                    .map(|deg| (deg as f32) * (std::f32::consts::PI / 180.0))
                    .unwrap_or(0.0);
                let sym_idx = packet.symbol_instances.len();
                packet.symbol_instances.push(SymbolInstance {
                    position: self.global_to_vertex([mx, my], bounds),
                    symbol_id,
                    rotation,
                    disp_prio: sym_priority as u32,
                    scale: scamin_scale,
                    // S-52 rotations are all from true north; a symbol given
                    // none stands upright on the screen.
                    true_bearing: rotation_deg.is_some() as u32,
                });
                all_symbol_priorities.push((sym_priority, sym_idx));
                stats.instances_added += 1;
            }

            if feature.object_class == ObjectClass::Light {
                log::trace!(
                    "light feature at ({:.1},{:.1}), is_first will be checked next",
                    mx,
                    my
                );
                if let Some(ref sector) = light_sector {
                    self.add_light_sector_lines(
                        packet,
                        self.global_to_vertex([mx, my], bounds),
                        sector,
                        resolved.priority,
                    );
                }

                // Generate light description text (only for first light at each position).
                // Sector lights carry unique directional information and must never be
                // deduped against a nearby all-round light — otherwise the sector arc is
                // drawn but the "Fl 3s 4m 3Nm" label silently disappears.
                let pos = (mx, my);
                let dedupe_radius_m = 15.0_f64;
                let is_first_at_pos = if light_sector.is_some() {
                    true
                } else {
                    match last_light_pos {
                        Some(last) => {
                            let dx = last.0 - pos.0;
                            let dy = last.1 - pos.1;
                            (dx * dx + dy * dy) > dedupe_radius_m * dedupe_radius_m
                        }
                        None => true,
                    }
                };

                if is_first_at_pos {
                    let desc = litdsn01(feature);
                    log::trace!(
                        "litdsn01 result for light at ({:.1},{:.1}): {:?}",
                        mx,
                        my,
                        desc
                    );
                    if let Some(desc) = desc {
                        // Collect light description as candidate (deferred layout)
	                        let color = self
	                            .s52_engine
	                            .and_then(|e| e.get_color("CHBLK"))
	                            .unwrap_or([0.0, 0.0, 0.0, 1.0]);
	                        let color_index = self
	                            .s52_engine
	                            .and_then(|e| e.get_color_index("CHBLK"))
	                            .unwrap_or(0) as u32;
	                            label_candidates.push(TextParams {
	                                position: self.global_to_vertex([mx, my], bounds),
	                                text: desc,
	                                color,
	                                color_index,
	                                scale: s52_text_scale(11, 5, self.view_ppmm),
	                                bold: false,
	                                space: crate::render::text_layout::SPACE_STANDARD,
                                hjust: HJust::Left,
                                vjust: VJust::Center,
                                xoffs: 1,
                                yoffs: 0,
                                disp_prio: sym_priority,
                                dis: 24,
                            });
                        }
                    if let Some(text) = orient_text {
	                        let color = self
	                            .s52_engine
	                            .and_then(|e| e.get_color("CHBLK"))
	                            .unwrap_or([0.0, 0.0, 0.0, 1.0]);
	                        let color_index = self
	                            .s52_engine
	                            .and_then(|e| e.get_color_index("CHBLK"))
	                            .unwrap_or(0) as u32;
	                        label_candidates.push(TextParams {
	                            position: self.global_to_vertex([mx, my], bounds),
	                            text,
	                            color,
	                            color_index,
	                            scale: s52_text_scale(10, 5, self.view_ppmm),
	                            bold: false,
	                            space: crate::render::text_layout::SPACE_STANDARD,
                            hjust: HJust::Left,
                            vjust: VJust::Top,
                            xoffs: 2,
                            yoffs: 1,
                            disp_prio: sym_priority,
                            dis: 27,
                        });
                    }
                    log::trace!(
                        "label_candidates after light text: {}",
                        label_candidates.len()
                    );
                }
                last_light_pos = Some(pos);
            } else {
                // Collect text label candidates from TX/TE instructions (deferred layout)
                for instr in &resolved.instructions {
                    if let RenderInstruction::Text {
                        attribute,
                        format,
                        hjust,
                        vjust,
                        xoffs,
                        yoffs,
                        color: color_token,
                        weight,
                        bsize,
                        space,
                        dis,
                        ..
                    } = instr
                    {
                        if self.should_skip_text(*dis) {
                            continue;
                        }
                        // A depth on an obstruction or wreck is a *sounding*.
                        // OBSTRN04 and WRECKS02 hand it to SNDFRM02, which
                        // emits digit symbols; navcore draws soundings as text,
                        // so route it through the same layout instead of the
                        // label path. Otherwise it came out at label size and
                        // with a decimal point — "9.2" at twice the height of
                        // the "9₂" the reference draws in the same place.
                        if attribute == "VALSOU" {
                            if let Some(depth) = feature.attribute_float("VALSOU") {
                                let info = crate::s52::cs::sndfrm02(depth, feature, &engine.settings);
                                let color_index =
                                    engine.get_color_index(info.color_token()).unwrap_or(0) as u32;
                                packet.text_instances.push(SoundingInstance {
                                    position: self.global_to_vertex([mx, my], bounds),
                                    depth: info.whole_part as f32,
                                    flags: info.to_flags(),
                                    scale: 1.8,
                                    color_index,
                                });
                            }
                            continue;
                        }
                        let text_value =
                            get_text_for_attribute(feature, attribute, format.as_deref());
                        if let Some(text) = text_value {
	                            let color = engine
	                                .get_color(color_token)
	                                .unwrap_or([0.0, 0.0, 0.0, 1.0]);
	                            let color_index =
	                                engine.get_color_index(color_token).unwrap_or(0) as u32;
	                            label_candidates.push(TextParams {
	                                position: self.global_to_vertex([mx, my], bounds),
	                                text,
	                                color,
	                                color_index,
	                                scale: s52_text_scale(*bsize, *weight, self.view_ppmm),
	                                bold: *weight >= 6,
	                                space: *space,
                                hjust: HJust::from(*hjust),
                                vjust: VJust::from(*vjust),
                                xoffs: *xoffs as i32,
                                yoffs: *yoffs as i32,
                                disp_prio: sym_priority,
                                dis: *dis,
                            });
                        }
                    }
                }
            }
        }

        log::trace!(
            "build_symbols total label_glyphs: {}",
            packet.label_candidates.len()
        );

        stats
    }

    /// Build sounding instances (numeric text) from SOUNDG multipoint features.
    fn build_soundings(
        &self,
        chart: &ChartData,
        info: &ChartInfo,
        tile_id: TileId,
        bounds: &TileBounds,
        packet: &mut TilePacket,
        ref_mx: f64,
        ref_my: f64,
        detailed_coverages: &[[[f64; 2]; 3]],
    ) -> usize {
        let default_settings = crate::s52::MarinerSettings::default();
        let settings = self
            .s52_engine
            .map(|e| &e.settings)
            .unwrap_or(&default_settings);

        let sounding_count = chart.soundings().count();

        // Use the tile's z-implied scale for SCAMIN filtering, NOT camera zoom.
        // This ensures tiles are consistent: z=13 tiles always show soundings if SCAMIN allows,
        // regardless of exactly where in the zoom range the camera is.
        //
        // Corrected to ground metres at the tile's latitude, as
        // `view_scale_denominator` does for every other layer: Mercator
        // metres are 1/cos(lat) too long, and without this soundings were
        // SCAMIN-dropped ~1.7x too early at Danish latitudes while the
        // contours and areas around them stayed.
        let tile_mpp = super::meters_per_pixel(tile_id.z);
        let (tile_lat, _) = crate::tiles::mercator_to_latlon(
            (bounds.min_x + bounds.max_x) * 0.5,
            (bounds.min_y + bounds.max_y) * 0.5,
        );
        let ground_mpp = tile_mpp * tile_lat.to_radians().cos().max(0.01);
        let tile_view_scale = ground_mpp * (self.view_ppmm as f64) * 1000.0;

        log::debug!("build_soundings: chart has {} sounding features, show_soundings={}, tile_z={}, tile_scale=1:{}",
            sounding_count, settings.show_soundings, tile_id.z, tile_view_scale as u64);

        if !settings.show_soundings {
            return 0;
        }

        // Multipoint coordinates are in SM meters relative to chart reference point.
        // Convert to global Mercator by adding the Mercator-projected reference offset,
        // same as area geometry does in for_each_triangle_global().

        let mut added = 0usize;
        let mut skipped_scale = 0usize;
        let mut skipped_bounds = 0usize;

        for feature in chart.soundings() {
            // SCAMIN/SCAMAX filtering using tile-implied scale
            let scamin_scale = should_render_at_scale_ex(feature, tile_view_scale, false, true, Some(info.native_scale));
            if scamin_scale == 0.0 {
                if skipped_scale == 0 {
                    // Log first skipped sounding's SCAMIN for debugging
                    log::debug!(
                        "  first skipped sounding SCAMIN={:?} (tile_scale=1:{})",
                        feature.scamin(),
                        tile_view_scale as u64
                    );
                }
                skipped_scale += 1;
                continue;
            }

            // NOTE: We intentionally skip S-52 display category filtering for soundings.
            // SOUNDG is in the "Other" category (show_other=false by default), but soundings
            // have their own dedicated `show_soundings` setting that we've already checked above.
            // This matches OpenCPN behavior where soundings are controlled separately.

            let Some(ref mp) = feature.multipoint_geometry else {
                continue;
            };

            for point in &mp.points {
                // SM meters → global Mercator (add reference point offset)
                let x = ref_mx + point[0] as f64;
                let y = ref_my + point[1] as f64;
                let depth = point[2] as f64;

                if point_in_any_triangle([x, y], detailed_coverages) {
                    continue;
                }

                let padding_m = 100.0 * self.view_meters_per_pixel as f64; // ~100px padding for soundings
                if x < bounds.min_x - padding_m
                    || x > bounds.max_x + padding_m
                    || y < bounds.min_y - padding_m
                    || y > bounds.max_y + padding_m
                {
                    skipped_bounds += 1;
                    continue;
                }

                if depth.abs() < 0.01 || depth.is_nan() {
                    log::warn!("sounding edge case: depth={} at ({:.1},{:.1})", depth, x, y);
                }
                let render_info = sndfrm02(depth, feature, settings);
                let color_index = self
                    .s52_engine
                    .and_then(|engine| engine.get_color_index(render_info.color_token()))
                    .unwrap_or(0) as u32;
                packet.text_instances.push(SoundingInstance {
                    position: self.global_to_vertex([x, y], bounds),
                    depth: render_info.whole_part as f32,
                    flags: render_info.to_flags(),
                    scale: scamin_scale * 1.8,
                    color_index,
                });
                added += 1;
            }
        }

        if sounding_count > 0 {
            log::debug!(
                "build_soundings: added={}, skipped_scale={}, skipped_bounds={}",
                added,
                skipped_scale,
                skipped_bounds
            );
        }

        added
    }

    /// A sector light's figure, sized on screen: see [`light_sector_instances`].
    fn add_light_sector_lines(
        &self,
        packet: &mut TilePacket,
        center: [f32; 2],
        sector: &crate::s52::LightSectorInfo,
        priority: u8,
    ) {
        packet.sector_instances.extend(light_sector_instances(
            center,
            sector,
            self.view_ppmm.max(1.0),
            self.s52_engine.map(|e| &e.tables),
            priority,
        ));
    }

    /// Convert global Mercator coordinates to a tile vertex position.
    ///
    /// Every position in a tile packet is in metres *relative to the tile's
    /// centre* (`TileBounds::center`), taken in f64 before the cast. Global
    /// Mercator is 7.5–8.4e6 m here, where an f32 cannot resolve better than
    /// 0.5–1 m; relative to the centre of even the largest tile the error is
    /// a fraction of a millimetre. The renderer draws each tile with the
    /// camera matrix re-based on the same centre (`Camera::view_projection_relative`).
    fn global_to_vertex(&self, global: [f64; 2], bounds: &TileBounds) -> [f32; 2] {
        tile_relative(global, bounds)
    }

    /// Get area color index for a feature using S-52 palette.
    ///
    /// Returns None for features that should be skipped (transparent).
    /// Extract area color index from already-resolved instructions.
    /// Handles special cases (land, buildings, depth) with hardcoded tokens,
    /// then falls back to the first AC instruction in the resolved list.
    fn extract_area_color_index(
        &self,
        feature: &Feature,
        engine: &S52Engine,
        instructions: &[RenderInstruction],
    ) -> Option<u16> {
        // Special cases with hardcoded color tokens
        if feature.is_land() {
            return engine.get_color_index("LANDA");
        }
        if feature.object_class == ObjectClass::Building {
            return engine.get_color_index("CHBRN");
        }
        if feature.is_depth_area() {
            let color_token = depare02_color_token(feature, &engine.settings);
            return engine.get_color_index(color_token);
        }

        // General case: extract from first AC instruction (already CS-expanded)
        for instr in instructions {
            if let RenderInstruction::AreaColor { color, .. } = instr {
                return engine.get_color_index(color);
            }
        }
        None
    }
}

/// Get displayable text for a feature attribute, optionally formatted with a TE format string.
fn get_text_for_attribute(
    feature: &Feature,
    attribute: &str,
    format: Option<&str>,
) -> Option<String> {
    use crate::senc::AttributeValue;

    let attr_val = if attribute == "OBJNAM" {
        // Match OpenCPN's national text behavior: when a rule requests OBJNAM,
        // prefer NOBJNM if it exists and differs.
        feature
            .attributes
            .get("NOBJNM")
            .or_else(|| feature.attributes.get(attribute))?
    } else {
        feature.attributes.get(attribute)?
    };
    match attr_val {
        AttributeValue::String(s) => {
            if s.is_empty() {
                return None;
            }
            Some(sanitize_bitmap_text(s))
        }
        AttributeValue::Integer(v) => {
            if let Some(fmt) = format {
                // Simple format handling for common patterns
                Some(sanitize_bitmap_text(&format_s52_text(fmt, *v as f64)))
            } else {
                Some(v.to_string())
            }
        }
        AttributeValue::Float(v) => {
            if let Some(fmt) = format {
                Some(sanitize_bitmap_text(&format_s52_text(fmt, *v)))
            } else {
                Some(format!("{:.1}", v))
            }
        }
    }
}

/// The S-52 line style as written, for the scene log: `DASH,2,CHMGD`.
fn style_label(style: &LineStyleKey) -> String {
    format!(
        "{:?},{},{}",
        style.pattern, style.width, style.color_token
    )
    .to_uppercase()
}

/// Whether a feature is one of the depth areas the relief effect shades.
///
/// DEPARE (42) and DRGARE (46) are the two that carry a depth range and get a
/// depth shade; UNSARE and the rest are flat by definition.
fn is_depth_area(feature: &crate::senc::Feature) -> bool {
    matches!(feature.type_code, 42 | 46)
}

/// Replace characters the label font has no glyph for.
///
/// The atlas covers ASCII and Latin-1, so Danish, German and French chart names
/// now render as written — this used to fold "Brøndby" to "Brondby" because the
/// 5x7 bitmap table stopped at U+007F. What remains is the tail beyond Latin-1:
/// fold it to the nearest plain letter rather than drop it, so a name stays
/// readable instead of losing characters.
fn sanitize_bitmap_text(input: &str) -> String {
    let font = crate::render::font::atlas();
    input
        .chars()
        .map(|c| {
            if font.has_glyph(c) {
                return c;
            }
            match c {
                'Ǽ' => 'Æ',
                'ǽ' => 'æ',
                _ if c.is_whitespace() => ' ',
                _ => '?',
            }
        })
        .collect()
}

/// Collect the per-chart edge indices belonging to the coastline-forming
/// features — land areas (LNDARE), coastlines (COALNE) and shoreline
/// constructions (SLCONS). These edges are drawn at high priority as the coast;
/// lower-priority area boundaries that share them are suppressed (OpenCPN's
/// PrioritizeLineFeature shared-edge rule).
fn collect_coast_edges(chart: &ChartData) -> HashSet<u32> {
    let mut edges = HashSet::new();
    for feature in chart.areas() {
        if feature.is_land() {
            if let Some(geom) = &feature.area_geometry {
                edges.extend(geom.edge_refs.iter().map(|e| e.index()));
            }
        }
    }
    for feature in chart.lines() {
        if feature.is_coastline() || feature.is_shoreline_construction() {
            if let Some(geom) = &feature.line_geometry {
                edges.extend(geom.edge_refs.iter().map(|e| e.index()));
            }
        }
    }
    edges
}

fn close_ring_if_needed(points: &mut Vec<[f64; 2]>) {
    if points.len() < 2 {
        return;
    }

    let first = points[0];
    let last = *points.last().unwrap();
    if (first[0] - last[0]).abs() > 1e-3 || (first[1] - last[1]).abs() > 1e-3 {
        points.push(first);
    }
}

/// Close a stroked boundary path only if its ends are a rounding error apart.
///
/// A complete ring already returns to its first node, so the only thing left to
/// bridge is f32 noise from the SENC's chart-local coordinates. A path whose
/// ends are genuinely apart is an *open* path — the boundary was cut where an
/// edge is shared with a higher-priority feature, or the node chain broke — and
/// joining it draws a straight line across whatever lies between.
fn close_open_ring_if_rounding_gap(points: &mut Vec<[f64; 2]>) {
    if points.len() < 2 {
        return;
    }
    let first = points[0];
    let last = *points.last().unwrap();
    let gap = ((first[0] - last[0]).powi(2) + (first[1] - last[1]).powi(2)).sqrt();
    // Mercator metres. Sub-pixel at every zoom the renderer supports, and far
    // below the shortest real edge in an ENC.
    const ROUNDING_GAP_M: f64 = 1.0;
    if gap > 1e-3 && gap < ROUNDING_GAP_M {
        points.push(first);
    }
}

/// Is the point inside any of the coverage triangles?
fn point_in_any_triangle(p: [f64; 2], tris: &[[[f64; 2]; 3]]) -> bool {
    let side = |a: [f64; 2], b: [f64; 2]| (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0]);
    tris.iter().any(|t| {
        let (d0, d1, d2) = (side(t[0], t[1]), side(t[1], t[2]), side(t[2], t[0]));
        let neg = d0 < 0.0 || d1 < 0.0 || d2 < 0.0;
        let pos = d0 > 0.0 || d1 > 0.0 || d2 > 0.0;
        !(neg && pos)
    })
}

// triangle_centroid and triangle_masked_by_coverage removed —
// Per-triangle coverage checks were removed with the coverage mask itself.

fn relation_all_vertices_covered(
    geom: &crate::senc::AreaGeometry,
    ref_lat: f64,
    ref_lon: f64,
    coverage: &[[[f64; 2]; 3]],
) -> bool {
    if coverage.is_empty() {
        return false;
    }

    let mut any_vertex = false;
    let mut all_covered = true;
    geom.for_each_triangle_global(ref_lat, ref_lon, |tri| {
        for vertex in tri {
            any_vertex = true;
            if !point_in_any_triangle(vertex, coverage) {
                all_covered = false;
                break;
            }
        }
    });
    any_vertex && all_covered
}

fn mask_polyline_by_coverage(
    polyline: &[[f64; 2]],
    coverage: &[[[f64; 2]; 3]],
) -> Vec<Vec<[f64; 2]>> {
    if coverage.is_empty() || polyline.len() < 2 {
        return vec![polyline.to_vec()];
    }

    let mut kept: Vec<Vec<[f64; 2]>> = Vec::new();
    let mut current: Vec<[f64; 2]> = Vec::new();

    for window in polyline.windows(2) {
        let a = window[0];
        let b = window[1];
        let midpoint = [(a[0] + b[0]) * 0.5, (a[1] + b[1]) * 0.5];
        let quarter_a = [
            a[0] + (b[0] - a[0]) * 0.25,
            a[1] + (b[1] - a[1]) * 0.25,
        ];
        let quarter_b = [
            a[0] + (b[0] - a[0]) * 0.75,
            a[1] + (b[1] - a[1]) * 0.75,
        ];

        if point_in_any_triangle(a, coverage)
            || point_in_any_triangle(b, coverage)
            || point_in_any_triangle(midpoint, coverage)
            || point_in_any_triangle(quarter_a, coverage)
            || point_in_any_triangle(quarter_b, coverage)
        {
            if current.len() >= 2 {
                kept.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
            continue;
        }

        if current.is_empty() {
            current.push(a);
        } else if current.last().copied() != Some(a) {
            current.push(a);
        }
        current.push(b);
    }

    if current.len() >= 2 {
        kept.push(current);
    }

    kept
}

fn tile_region_fully_covered(bounds: TileBounds, coverage: &[[[f64; 2]; 3]]) -> bool {
    if coverage.is_empty() {
        return false;
    }

    let test_points = [
        [bounds.min_x, bounds.min_y],
        [bounds.min_x, bounds.max_y],
        [bounds.max_x, bounds.min_y],
        [bounds.max_x, bounds.max_y],
        [(bounds.min_x + bounds.max_x) * 0.5, (bounds.min_y + bounds.max_y) * 0.5],
    ];

    test_points
        .into_iter()
        .all(|point| point_in_any_triangle(point, coverage))
}

fn collect_more_detailed_coverages(
    loaded_charts: &[LoadedChartContext<'_>],
    chart_index: usize,
) -> Vec<[[f64; 2]; 3]> {
    loaded_charts[chart_index + 1..]
        .iter()
        .flat_map(|ctx| ctx.coverage_triangles.iter().copied())
        .collect()
}

/// The chart's declared coverage, as triangles in global Mercator metres.
///
/// Taken from the M_COVR feature's own tessellation, not from its edge rings.
/// The rings are not reliably reconstructable: one 1:22000 cell lists 172 edge
/// references for its coverage and `resolve_rings` can recover only two
/// two-point fragments from them, because most of those edges are not in the
/// cell's edge table at all. Closing those fragments invented polygons that
/// masked the chart underneath — the pale rectangles over land — and the
/// obvious retreat, using the chart's extent rectangle, is worse: an extent is
/// a bounding box, and masking by it asserts data across every bay the cell
/// does not chart.
///
/// Empty means "this chart makes no claim about where it has data", and the
/// quilt must then not mask anything on its behalf. Drawing a coarse chart
/// under a detailed one costs a few overdrawn triangles; masking it out where
/// the detailed chart has nothing costs a hole.
fn extract_coverage_triangles(
    chart: &ChartData,
    ref_mx: f64,
    ref_my: f64,
) -> Vec<[[f64; 2]; 3]> {
    let mut tris = Vec::new();
    for feature in chart.areas() {
        if !feature.is_coverage() || feature.attribute_int("CATCOV") != Some(1) {
            continue;
        }
        let Some(geom) = &feature.area_geometry else {
            continue;
        };
        for prim in &geom.triangles {
            for chunk in prim.to_triangles().chunks_exact(3) {
                tris.push([
                    [ref_mx + chunk[0][0] as f64, ref_my + chunk[0][1] as f64],
                    [ref_mx + chunk[1][0] as f64, ref_my + chunk[1][1] as f64],
                    [ref_mx + chunk[2][0] as f64, ref_my + chunk[2][1] as f64],
                ]);
            }
        }
    }
    tris
}

/// Compute the arc-length midpoint of a polyline. Returns None for empty or
/// zero-length input.
fn polyline_midpoint(points: &[[f64; 2]]) -> Option<(f64, f64)> {
    if points.len() < 2 {
        return points.first().map(|p| (p[0], p[1]));
    }
    let mut total = 0.0_f64;
    let segments: Vec<f64> = points
        .windows(2)
        .map(|w| {
            let dx = w[1][0] - w[0][0];
            let dy = w[1][1] - w[0][1];
            let len = (dx * dx + dy * dy).sqrt();
            total += len;
            len
        })
        .collect();
    if total <= 0.0 {
        return Some((points[0][0], points[0][1]));
    }
    let half = total * 0.5;
    let mut walked = 0.0_f64;
    for (i, len) in segments.iter().enumerate() {
        if walked + len >= half {
            let t = if *len > 0.0 { (half - walked) / len } else { 0.0 };
            let a = &points[i];
            let b = &points[i + 1];
            return Some((a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t));
        }
        walked += len;
    }
    Some((
        points[points.len() - 1][0],
        points[points.len() - 1][1],
    ))
}

fn prtsur01_hatch_polylines(
    rings: &[Vec<[f64; 2]>],
    bounds: &TileBounds,
    meters_per_pixel: f64,
    ppmm: f64,
) -> Vec<Vec<[f64; 2]>> {
    if rings.is_empty() || meters_per_pixel <= 0.0 || ppmm <= 0.0 {
        return Vec::new();
    }

    // PRTSUR01's source HPGL is a short horizontal SW2 stroke. The spacing
    // below uses the pattern's 10 mm minimum distance to keep the mark sparse
    // and screen-stable without sending the degenerate zero-height AP tile to
    // the generic pattern shader.
    let dash_len_m = (201.0 * 0.01 * ppmm * meters_per_pixel).max(meters_per_pixel * 2.0);
    let step_m = (1000.0 * 0.01 * ppmm * meters_per_pixel).max(dash_len_m * 2.0);
    let y_step_m = step_m;

    let mut polylines = Vec::new();
    let mut y = (bounds.min_y / y_step_m).floor() * y_step_m;
    if y < bounds.min_y {
        y += y_step_m;
    }

    const MAX_PRTSUR01_STROKES_PER_TILE_FEATURE: usize = 4096;
    while y <= bounds.max_y && polylines.len() < MAX_PRTSUR01_STROKES_PER_TILE_FEATURE {
        let intervals = horizontal_polygon_intervals(rings, y, bounds.min_x, bounds.max_x);
        for (x0, x1) in intervals {
            let mut x = (x0 / step_m).floor() * step_m;
            if x < x0 {
                x += step_m;
            }
            while x < x1 && polylines.len() < MAX_PRTSUR01_STROKES_PER_TILE_FEATURE {
                let end_x = (x + dash_len_m).min(x1);
                if end_x - x >= meters_per_pixel {
                    polylines.push(vec![[x, y], [end_x, y]]);
                }
                x += step_m;
            }
        }
        y += y_step_m;
    }

    polylines
}

fn horizontal_polygon_intervals(
    rings: &[Vec<[f64; 2]>],
    y: f64,
    min_x: f64,
    max_x: f64,
) -> Vec<(f64, f64)> {
    let mut xs = Vec::new();
    for ring in rings {
        for edge in ring.windows(2) {
            let a = edge[0];
            let b = edge[1];
            if (a[1] > y) == (b[1] > y) {
                continue;
            }
            let dy = b[1] - a[1];
            if dy.abs() < 1e-9 {
                continue;
            }
            let t = (y - a[1]) / dy;
            xs.push(a[0] + t * (b[0] - a[0]));
        }
    }

    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut intervals = Vec::new();
    for pair in xs.chunks_exact(2) {
        let x0 = pair[0].max(min_x);
        let x1 = pair[1].min(max_x);
        if x1 > x0 {
            intervals.push((x0, x1));
        }
    }
    intervals
}

/// Convert a CHARS body-size (points) and weight to a bitmap-font scale.
///
/// OpenCPN maps the S-52 `bsize` to a font POINT size — `fontSize = (bsize/20)*4 + default`,
/// floored near 10pt (s52plib.cpp:2465-2488) — then rasterizes it at the display DPI. The
/// previous formula (`bsize/8`) ignored DPI entirely, so labels came out ~2x too small on a
/// 1x display and ~3-4x too small on Retina. `view_ppmm` carries the HiDPI factor (4.0 px/mm
/// at 96 DPI x1, 8.0 at x2), so we size the 8px glyph cell directly from the point size at
/// that DPI. Weight 6+ ("bold") nudges the scale up slightly, matching OpenCPN's bold OBJNAMs.
fn s52_text_scale(bsize: u8, weight: u8, view_ppmm: f32) -> f32 {
    let points = ((bsize as f32 / 20.0) * 4.0 + 12.0).max(10.0);
    // 0.0352 tunes the 8px cap-height cell to OpenCPN's rendered label size
    // (bsize=11 -> ~14px logical cap on x1, ~28px physical on x2).
    // Weight is no longer faked with size: the atlas carries a real bold face,
    // selected via TextParams::bold.
    let _ = weight;
    points * view_ppmm * 0.0352
}

/// Format a numeric value using S-52 TE format string (subset of printf-style).
/// Handles common patterns like "%4.1lf", "%d", etc.
fn format_s52_text(fmt: &str, value: f64) -> String {
    // Strip any prefix text before % (e.g., "clr %4.1lf" → extract "clr " prefix)
    if let Some(pct_pos) = fmt.find('%') {
        let prefix = &fmt[..pct_pos];
        let spec = &fmt[pct_pos + 1..];

        // Parse the format spec: remove trailing 'lf', 'f', 'd' etc.
        let formatted = if spec.ends_with("lf") || spec.ends_with('f') {
            // Float format - extract precision
            let clean = spec.trim_end_matches("lf").trim_end_matches('f');
            if let Some(dot_pos) = clean.find('.') {
                let precision: usize = clean[dot_pos + 1..].parse().unwrap_or(1);
                format!("{:.prec$}", value, prec = precision)
            } else {
                format!("{:.1}", value)
            }
        } else if spec.ends_with('d') {
            format!("{}", value as i64)
        } else {
            format!("{}", value)
        };

        format!("{}{}", prefix, formatted)
    } else {
        // No format specifier, just return the value
        format!("{}", value)
    }
}

/// A sector light's figure: the outlined, coloured arc and the two dashed
/// legs, as screen-sized instances centred on the light (metres from the tile
/// centre).
///
/// S-52 sizes all three in millimetres on the display, so they are built in
/// pixels at the display density `ppmm` — never in Mercator metres, which made
/// them scale with the zoom (see `render::sectors`). Nothing here depends on
/// the zoom.
fn light_sector_instances(
    center: [f32; 2],
    sector: &crate::s52::LightSectorInfo,
    ppmm: f32,
    tables: Option<&crate::s52::LookupTables>,
    priority: u8,
) -> Vec<crate::render::SectorInstance> {
    use crate::render::sectors::{SectorInstance, SECTOR_ARC, SECTOR_LEG};

    let sectr1 = sector.sectr1;
    let sectr2 = if sector.sectr2 <= sectr1 {
        sector.sectr2 + 360.0
    } else {
        sector.sectr2
    };
    let sweep = sectr2 - sectr1;
    // Only a degenerate sweep is dropped. The caller has already removed
    // the light's symbol, so returning here for a full circle (a major
    // all-round light) or a sub-degree leading sector would leave the
    // light drawn as nothing at all.
    if sweep <= 0.0 || sweep > 360.0 {
        return Vec::new();
    }

    let to_chart = |angle: f64| {
        if angle > 180.0 {
            angle - 180.0
        } else {
            angle + 180.0
        }
    };
    let s1 = to_chart(sectr1);
    let s2 = {
        let raw = to_chart(sectr2);
        if raw <= s1 {
            raw + 360.0
        } else {
            raw
        }
    };

    // The CA() radius argument behaves as a *diameter* on screen: OpenCPN
    // computes `rad = radius * canvas_pix_per_mm`, but the circle it draws
    // for `CA(...,10.0,25.0)` measures 76 px across in the 1:2600 reference
    // capture, where 10 mm at that canvas density would be 160 px. Halving
    // reproduces the reference to within 5% across all four views.
    let mm_to_px = |mm: f64| (mm * 0.5 * ppmm as f64) as f32;
    let arc_radius_px = mm_to_px(sector.arc_radius_mm);
    let sector_radius_px = mm_to_px(sector.sector_radius_mm);
    if arc_radius_px <= 0.0 {
        return Vec::new();
    }

    let (outline_color, arc_color, arc_width) = if sector.faint {
        ("CHBLK", "CHBRN", 1u8)
    } else {
        ("OUTLW", sector.arc_color_token, 2u8)
    };
    // The same widths, colours and dash as the line styles these were
    // once drawn with.
    let style = |pattern, width, color: &str| {
        crate::render::s52_styles::style_for_key(&LineStyleKey::new(pattern, width, color), ppmm, tables)
    };
    let outline = style(LinePattern::Solid, 4, outline_color);
    let arc = style(LinePattern::Solid, arc_width, arc_color);
    let legs = style(LinePattern::Dashed, 2, "CHBLK");

    // The arc belongs to the LIGHTS object and draws at that object's LUP
    // priority (Hazards, 8) — not a fixed level. A hardcoded 4 happened to
    // sit above everything while navcore's priority ladder was shifted one
    // step low; with the correct S-52 numbering it fell under the line
    // symbology and the sector arcs vanished from the chart.
    let bearings = [s1.to_radians() as f32, s2.to_radians() as f32];
    let instance = |kind, radius_px, style: &crate::render::LineStyle, bearings: [f32; 2]| {
        SectorInstance {
            position: center,
            radius_px,
            width_px: style.width_px,
            bearings,
            dash_px: [style.dash_on_px, style.dash_off_px],
            color_index: style.color_index,
            kind,
            disp_prio: priority as u32,
        }
    };
    // Outline under the colour, then the legs.
    let mut out = vec![
        instance(SECTOR_ARC, arc_radius_px, &outline, bearings),
        instance(SECTOR_ARC, arc_radius_px, &arc, bearings),
    ];
    // Sector legs — a full circle (an all-round light) has none.
    if sector.sector_radius_mm > 0.0 && sweep < 360.0 {
        for b in bearings {
            out.push(instance(SECTOR_LEG, sector_radius_px, &legs, [b, b]));
        }
    }
    out
}

/// A line in the direction its LC() symbols are laid along.
///
/// As s52plib's `draw_lc_poly`: it takes the winding of the whole line
/// (closing it) and, if that is clockwise *on screen* — y down, so
/// anticlockwise in Mercator, y up — walks it backwards. A symbol drawn to
/// one side of the line therefore faces the same way round every area.
fn lc_direction(mut line: Vec<[f64; 2]>) -> Vec<[f64; 2]> {
    let Some(&o) = line.first() else { return line };
    // Relative to the first point, so the products stay small.
    let rel = |p: [f64; 2]| [p[0] - o[0], p[1] - o[1]];
    let mut twice_area = 0.0;
    for i in 0..line.len() {
        let a = rel(line[i]);
        let b = rel(line[(i + 1) % line.len()]);
        twice_area += a[0] * b[1] - a[1] * b[0];
    }
    if twice_area > 0.0 {
        line.reverse();
    }
    line
}

/// Start of each priority level in a list already sorted by priority: entry
/// `p` is the first index at priority `p` or above, entry 10 the length.
fn priority_offsets(prios: impl Iterator<Item = u8>) -> [u32; 11] {
    let mut offsets = [0u32; 11];
    let mut next = 0usize;
    let mut n = 0u32;
    for p in prios {
        while next <= p as usize {
            offsets[next] = n;
            next += 1;
        }
        n += 1;
    }
    while next <= 10 {
        offsets[next] = n;
        next += 1;
    }
    offsets
}

/// A global Mercator point relative to a tile's centre, the frame every
/// position in a [`TilePacket`] is stored in.
/// Finer than this display scale (1:N), the world basemap stops drawing its
/// land and marks everything outside the real charts as "no data" instead.
/// Natural Earth's 1:50m outline is roughly right at a few million; at a
/// harbour scale it is kilometres off, and a wrong coast is worse than none.
pub const NO_DATA_FINER_THAN: f64 = 2_000_000.0;

/// Two triangles covering the whole tile in the given palette colour, at
/// the lowest display priority, so any chart drawn in the tile covers them.
/// A hair larger than the tile so neighbouring tiles leave no seam.
fn no_data_quad(bounds: &TileBounds, color_index: u32) -> [AreaVertex; 6] {
    let hw = ((bounds.max_x - bounds.min_x) * 0.5 * 1.001) as f32;
    let hh = ((bounds.max_y - bounds.min_y) * 0.5 * 1.001) as f32;
    let v = |x: f32, y: f32| AreaVertex { position: [x, y], color_index, disp_prio: 0, shade: 0.0 };
    [v(-hw, -hh), v(hw, -hh), v(hw, hh), v(-hw, -hh), v(hw, hh), v(-hw, hh)]
}

pub fn tile_relative(global: [f64; 2], bounds: &TileBounds) -> [f32; 2] {
    let (ox, oy) = bounds.center();
    [(global[0] - ox) as f32, (global[1] - oy) as f32]
}

fn bbox_is_degenerate(bbox: crate::senc::BBox) -> bool {
    !bbox.min_x.is_finite()
        || !bbox.max_x.is_finite()
        || !bbox.min_y.is_finite()
        || !bbox.max_y.is_finite()
        || (bbox.max_x - bbox.min_x).abs() < 1e-9
        || (bbox.max_y - bbox.min_y).abs() < 1e-9
}

fn area_bounds_global(
    geom: &crate::senc::AreaGeometry,
    ref_lat: f64,
    ref_lon: f64,
) -> Option<TileBounds> {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    let mut any = false;

    geom.for_each_triangle_global(ref_lat, ref_lon, |tri| {
        for vertex in tri {
            any = true;
            min_x = min_x.min(vertex[0]);
            min_y = min_y.min(vertex[1]);
            max_x = max_x.max(vertex[0]);
            max_y = max_y.max(vertex[1]);
        }
    });

    any.then_some(TileBounds {
        min_x,
        min_y,
        max_x,
        max_y,
    })
}

fn tile_relation_mercator(feature_bounds: TileBounds, tile: TileBounds) -> BBoxTileRelation {
    if feature_bounds.max_x < tile.min_x
        || feature_bounds.min_x > tile.max_x
        || feature_bounds.max_y < tile.min_y
        || feature_bounds.min_y > tile.max_y
    {
        BBoxTileRelation::Outside
    } else if feature_bounds.min_x >= tile.min_x
        && feature_bounds.max_x <= tile.max_x
        && feature_bounds.min_y >= tile.min_y
        && feature_bounds.max_y <= tile.max_y
    {
        BBoxTileRelation::FullyInside
    } else {
        BBoxTileRelation::Intersects
    }
}

fn triangle_area_abs(tri: [[f64; 2]; 3]) -> f64 {
    ((tri[1][0] - tri[0][0]) * (tri[2][1] - tri[0][1])
        - (tri[2][0] - tri[0][0]) * (tri[1][1] - tri[0][1]))
        .abs()
        * 0.5
}

fn push_area_and_pattern_vertex(
    packet: &mut TilePacket,
    position: [f32; 2],
    color_index: u32,
    pattern_id: Option<u32>,
    priority: u8,
) {
    packet.area_vertices.push(AreaVertex {
        position,
        color_index,
        disp_prio: priority as u32,
        shade: 0.0,
    });
    if let Some(pattern_id) = pattern_id {
        packet.pattern_vertices.push(crate::render::PatternVertex {
            position,
            pattern_id,
            offset: [0.0, 0.0],
            disp_prio: priority as u32,
        });
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct AreaBuildStats {
    total_features: usize,
    with_geometry: usize,
    bbox_pass: usize,
    fast_path_features: usize,
    slow_path_features: usize,
    triangles_seen: usize,
    subpixel_triangles: usize,
    triangles_clipped: usize,
    trivial_accept_triangles: usize,
    trivial_reject_triangles: usize,
    vertices_added: usize,
}

#[derive(Debug, Default, Clone, Copy)]
struct LineBuildStats {
    total_features: usize,
    with_geometry: usize,
    bbox_pass: usize,
    segments_out: usize,
    vertices_added: usize,
    // S-52 instrumentation
    styled_by_ls: usize,          // Features styled via LS() instruction
    styled_by_cs: usize,          // Features styled via CS() procedure execution
    styled_by_cs_fallback: usize, // Features needing CS() - using fallback (unknown proc)
    styled_by_lc: usize,          // Features styled via LC() with valid symbol
    styled_by_lc_missing: usize,  // Features with LC() but symbol not found in table
    styled_by_lc_fallback: usize, // Features needing LC() - using LS fallback style
    skipped_other: usize,         // Skipped: display_cat == "Other"
    skipped_no_lookup: usize,     // Skipped: no S-52 lookup for object class
}

impl LineBuildStats {
    fn merge(&mut self, other: &LineBuildStats) {
        self.total_features += other.total_features;
        self.with_geometry += other.with_geometry;
        self.bbox_pass += other.bbox_pass;
        self.segments_out += other.segments_out;
        self.vertices_added += other.vertices_added;
        self.styled_by_ls += other.styled_by_ls;
        self.styled_by_cs += other.styled_by_cs;
        self.styled_by_cs_fallback += other.styled_by_cs_fallback;
        self.styled_by_lc += other.styled_by_lc;
        self.styled_by_lc_missing += other.styled_by_lc_missing;
        self.styled_by_lc_fallback += other.styled_by_lc_fallback;
        self.skipped_other += other.skipped_other;
        self.skipped_no_lookup += other.skipped_no_lookup;
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct SymbolBuildStats {
    total_features: usize,
    with_geometry: usize,
    bbox_pass: usize,
    matched_symbol: usize,
    multi_symbol_features: usize,
    instances_added: usize,
}

/// Legacy line style lookup for when S-52 lookup table is unavailable.
/// Uses ObjectClass matching as fallback.
fn legacy_line_style(
    feature: &Feature,
    s52_engine: Option<&S52Engine>,
    safety_contour: Option<f64>,
) -> Option<LineStyleKey> {
    match feature.object_class {
        ObjectClass::Coastline => Some(LineStyleKey::new(LinePattern::Solid, 1, "CSTLN")),
        ObjectClass::ShorelineConstruction => {
            // Basic SLCONS03 classification
            if let Some(catslc) = feature.attribute_int("CATSLC") {
                if catslc == 4 || catslc == 6 {
                    return Some(LineStyleKey::new(LinePattern::Solid, 4, "CSTLN"));
                }
            }
            Some(LineStyleKey::new(LinePattern::Solid, 2, "CSTLN"))
        }
        ObjectClass::DepthContour => {
            if let Some(engine) = s52_engine {
                // Use full depcnt02 which handles safety contour AND QUAPOS (low accuracy)
                let style = depcnt02_selected(feature, &engine.settings, safety_contour);
                return Some(LineStyleKey::new(
                    style.pattern(),
                    style.width(),
                    style.color_token(),
                ));
            }
            // Fallback without S52 engine
            Some(LineStyleKey::new(LinePattern::Solid, 1, "DEPCN"))
        }
        ObjectClass::CableOverhead => Some(LineStyleKey::new(LinePattern::Dashed, 4, "CHGRD")),
        ObjectClass::CableSubmarine => Some(LineStyleKey::new(LinePattern::Dashed, 1, "CHMGD")),
        ObjectClass::TrafficSeparationLine => {
            Some(LineStyleKey::new(LinePattern::Solid, 6, "TRFCF"))
        }
        ObjectClass::Road => Some(LineStyleKey::new(LinePattern::Solid, 2, "LANDF")),
        ObjectClass::RiverBank => Some(LineStyleKey::new(LinePattern::Dotted, 2, "CSTLN")),
        ObjectClass::Pipeline => Some(LineStyleKey::new(LinePattern::Solid, 2, "CHGRD")),
        _ => None, // Unknown class
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::senc::{AttributeValue, Feature, FeatureType};

    #[test]
    fn area_vertex_layout_matches_shader() {
        // chart.wgsl expects:
        // @location(0) position: vec2<f32>
        // @location(1) color_index: u32
        // @location(2) disp_prio: u32
        // @location(3) shade: f32
        assert_eq!(std::mem::size_of::<AreaVertex>(), 20);
        assert_eq!(std::mem::align_of::<AreaVertex>(), 4);
    }

    /// Packet positions are metres from the tile centre, and the renderer
    /// re-bases each tile on `TileId::origin`. The two must agree exactly, and
    /// the offset must survive f32 at Danish latitudes to well under a
    /// millimetre — global Mercator in f32 would be 0.5 m out here.
    #[test]
    fn the_no_data_quad_covers_the_whole_tile() {
        let (mx, my) = crate::tiles::latlon_to_mercator(55.9, 12.7);
        let tile = TileId::from_mercator(mx, my, 12);
        let b = tile.bounds();
        let q = no_data_quad(&b, 7);
        let (hw, hh) = (((b.max_x - b.min_x) * 0.5) as f32, ((b.max_y - b.min_y) * 0.5) as f32);
        for corner in [[-hw, -hh], [hw, -hh], [hw, hh], [-hw, hh]] {
            let reached = q.iter().any(|v| {
                (v.position[0].abs() >= corner[0].abs()) && (v.position[1].abs() >= corner[1].abs())
                    && v.position[0].signum() == corner[0].signum()
                    && v.position[1].signum() == corner[1].signum()
            });
            assert!(reached, "corner {corner:?} not covered");
        }
        assert!(q.iter().all(|v| v.color_index == 7 && v.disp_prio == 0));
    }

    #[test]
    fn tile_relative_positions_keep_millimetres() {
        let (mx, my) = crate::tiles::latlon_to_mercator(55.70333, 12.61457);
        let tile = TileId::from_mercator(mx, my, 18);
        let origin = tile.origin();
        let p = [mx + 0.0123, my - 0.0456];
        let rel = tile_relative(p, &tile.bounds());
        let back = [origin.x + rel[0] as f64, origin.y + rel[1] as f64];
        assert!((back[0] - p[0]).abs() < 1e-5 && (back[1] - p[1]).abs() < 1e-5, "{back:?} vs {p:?}");
        assert!(((p[1] as f32) as f64 - p[1]).abs() > 0.01, "global f32 was expected to be coarse here");
    }

    fn sector(sectr1: f64, sectr2: f64) -> crate::s52::LightSectorInfo {
        crate::s52::LightSectorInfo {
            sectr1,
            sectr2,
            arc_radius_mm: 10.0,
            sector_radius_mm: 14.0,
            arc_color_token: "LITRD",
            faint: false,
        }
    }

    /// A sector light is an outline, a coloured arc and two dashed legs,
    /// sized in pixels from millimetres at the display density alone —
    /// there is no zoom anywhere in the input, which is the point.
    #[test]
    fn sector_figures_are_sized_in_pixels() {
        use crate::render::sectors::{SECTOR_ARC, SECTOR_LEG};
        let ppmm = 8.0;
        let out = light_sector_instances([1.5, -2.0], &sector(320.0, 336.0), ppmm, None, 8);
        assert_eq!(out.len(), 4);
        assert_eq!(out.iter().filter(|s| s.kind == SECTOR_ARC).count(), 2);
        let legs: Vec<_> = out.iter().filter(|s| s.kind == SECTOR_LEG).collect();
        assert_eq!(legs.len(), 2);
        for s in &out {
            assert_eq!(s.position, [1.5, -2.0]);
            assert_eq!(s.disp_prio, 8);
        }
        // CA radius behaves as a diameter: 10 mm -> 5 mm -> 40 px at 8 px/mm.
        assert!((out[0].radius_px - 40.0).abs() < 1e-4);
        assert!((legs[0].radius_px - 56.0).abs() < 1e-4);
        // The outline is wider than the colour it frames; legs are dashed.
        assert!(out[0].width_px > out[1].width_px);
        assert!(legs[0].dash_px[0] > 0.0 && legs[0].dash_px[1] > 0.0);
        // Bearings: SECTR 320..336 seen from seaward is 140..156 from the light.
        let deg = |r: f32| r.to_degrees();
        assert!((deg(out[1].bearings[0]) - 140.0).abs() < 1e-3);
        assert!((deg(out[1].bearings[1]) - 156.0).abs() < 1e-3);
        // Twice the density, twice the pixels.
        let hi = light_sector_instances([0.0, 0.0], &sector(320.0, 336.0), 16.0, None, 8);
        assert!((hi[0].radius_px - 80.0).abs() < 1e-4);
    }

    /// An all-round light is a ring with no legs; a sector wrapping through
    /// north keeps its sweep.
    #[test]
    fn full_circles_have_no_legs_and_wraps_keep_their_sweep() {
        let ring = light_sector_instances([0.0, 0.0], &sector(0.0, 360.0), 4.0, None, 8);
        assert_eq!(ring.len(), 2);
        let wrap = light_sector_instances([0.0, 0.0], &sector(350.0, 10.0), 4.0, None, 8);
        let sweep = (wrap[1].bearings[1] - wrap[1].bearings[0]).to_degrees();
        assert!((sweep - 20.0).abs() < 1e-3, "{sweep}");
    }

    #[test]
    fn priority_offsets_mark_where_each_level_starts() {
        assert_eq!(
            priority_offsets([0u8, 0, 3, 3, 9].into_iter()),
            [0, 2, 2, 2, 4, 4, 4, 4, 4, 4, 5]
        );
        assert_eq!(priority_offsets(std::iter::empty()), [0; 11]);
    }

    /// s52plib walks an LC() line backwards when it winds clockwise on
    /// screen — anticlockwise in Mercator, where y is up.
    #[test]
    fn lc_lines_run_clockwise_in_mercator() {
        let ccw = vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
        let mut cw = ccw.clone();
        cw.reverse();
        assert_eq!(lc_direction(ccw.clone()), cw);
        assert_eq!(lc_direction(cw.clone()), cw);
        // Far from the origin, as real coordinates are.
        let far: Vec<[f64; 2]> = ccw.iter().map(|p| [p[0] + 1.4e6, p[1] + 7.5e6]).collect();
        assert_eq!(lc_direction(far.clone())[0], far[3]);
    }

    #[test]
    fn tile_packet_byte_size() {
        let mut packet = TilePacket::new(TileId::new(10, 512, 512));
        packet.area_vertices.push(AreaVertex {
            position: [0.0, 0.0],
            color_index: 0,
            disp_prio: 0,
            shade: 0.0,
        });
        packet.compute_byte_size();
        assert_eq!(packet.byte_size, std::mem::size_of::<AreaVertex>());
    }

    // test_scamin_and_scamax_combined: removed (missing make_feature_with_scamin helper)

    // LC symbol lookup tests

    #[test]
    fn test_line_style_table_loads() {
        let table = get_line_style_table();
        assert!(
            table.is_some(),
            "LineStyleTable should load from chartsymbols.xml"
        );
        let table = table.unwrap();
        assert!(table.len() > 0, "LineStyleTable should have symbols");
    }

    #[test]
    fn test_line_style_table_has_lowacc21() {
        // LOWACC21 is used by COALNE features with low accuracy positions
        let table = get_line_style_table().expect("table should load");
        let symbol = table.get("LOWACC21");
        assert!(
            symbol.is_some(),
            "LOWACC21 should exist in line style table"
        );
        let symbol = symbol.unwrap();
        assert_eq!(
            symbol.color_ref, "CSTLN",
            "LOWACC21 uses ACSTLN in the XML; the parser strips the pen letter"
        );
    }

    #[test]
    fn test_line_style_table_missing_symbol() {
        // Non-existent symbols should return None
        let table = get_line_style_table().expect("table should load");
        assert!(table.get("NONEXISTENT").is_none());
    }

    #[test]
    fn test_line_batch_key_ordering() {
        use crate::s52::LineStyleKey;

        // Lower priority should sort first
        let key1 = LineBatchKey::new_with_priority(
            2,
            0,
            LineStyleKey::new(LinePattern::Solid, 1, "CHBLK"),
            1,
            false,
        );
        let key2 = LineBatchKey::new_with_priority(
            4,
            0,
            LineStyleKey::new(LinePattern::Solid, 1, "CHBLK"),
            2,
            false,
        );
        assert!(key1 < key2, "disp_prio=2 should sort before disp_prio=4");

        // Same priority, lower pass should sort first
        let key3 = LineBatchKey::new_with_priority(
            4,
            0,
            LineStyleKey::new(LinePattern::Solid, 1, "CHBLK"),
            3,
            false,
        );
        let key4 = LineBatchKey::new_with_priority(
            4,
            1,
            LineStyleKey::new(LinePattern::Solid, 1, "CHBLK"),
            4,
            false,
        );
        assert!(key3 < key4, "pass=0 should sort before pass=1");
    }

    #[test]
    fn text_prefers_national_object_name() {
        let mut attributes = crate::senc::Attributes::new();
        attributes.insert("OBJNAM",
            AttributeValue::String("Brondby".to_string()),
        );
        attributes.insert("NOBJNM",
            AttributeValue::String("Brøndby".to_string()),
        );

        let feature = Feature {
            type_code: 0,
            object_class: ObjectClass::Other,
            feature_type: FeatureType::Point,
            attributes,
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        };

        // The national name wins, and it now survives verbatim: the label font
        // carries Latin-1, so "Brøndby" no longer has to be folded to "Brondby".
        assert_eq!(
            get_text_for_attribute(&feature, "OBJNAM", None),
            Some("Brøndby".to_string())
        );
    }

    #[test]
    fn close_ring_appends_missing_final_edge() {
        let mut points = vec![[1.0, 2.0], [3.0, 4.0], [5.0, 6.0]];
        close_ring_if_needed(&mut points);
        assert_eq!(points.len(), 4);
        assert_eq!(points.first(), points.last());
    }

    #[test]
    fn point_in_triangle_detects_coverage_hits() {
        let tris = [
            [[0.0, 0.0], [10.0, 0.0], [10.0, 10.0]],
            [[0.0, 0.0], [10.0, 10.0], [0.0, 10.0]],
        ];
        assert!(point_in_any_triangle([5.0, 5.0], &tris));
        assert!(point_in_any_triangle([1.0, 9.0], &tris));
        assert!(!point_in_any_triangle([15.0, 5.0], &tris));
        assert!(!point_in_any_triangle([-1.0, 5.0], &tris));
    }

    #[test]
    fn detects_degenerate_bbox() {
        let bbox = crate::senc::BBox {
            min_x: 12.0,
            max_x: 12.0,
            min_y: 55.0,
            max_y: 55.0,
        };

        assert!(bbox_is_degenerate(bbox));
    }

    #[test]
    fn tile_relation_mercator_handles_large_triangle_bounds() {
        let feature_bounds = TileBounds {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 100.0,
            max_y: 100.0,
        };
        let tile = TileBounds {
            min_x: 10.0,
            min_y: 10.0,
            max_x: 20.0,
            max_y: 20.0,
        };

        assert_eq!(
            tile_relation_mercator(feature_bounds, tile),
            BBoxTileRelation::Intersects
        );
    }
}
