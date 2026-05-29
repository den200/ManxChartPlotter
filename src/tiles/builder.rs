//! Tile builder - clips geometry to tile bounds for rendering.
//!
//! Builds TilePackets (CPU-side) from chart data, ready for GPU upload.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use bytemuck::{Pod, Zeroable};

use crate::cache::CachedDecryptor;
use crate::decrypt::KeyStore;
use crate::render::lc_pattern::{
    compute_phase_offset, generate_stamps_along_polyline_with_phase, stamps_to_polylines_by_width,
    LcPatternTable, LcRenderConfig,
};
use crate::render::symbols::SymbolInstance;
use crate::render::text::SoundingInstance;
use crate::render::{build_line_vertices_multi_indexed, LineVertex};
use crate::render::{HJust, TextParams, VJust};
use crate::s52::{
    depare02_color_token, depcnt02, light_render_info, light_sector_info, litdsn01, sndfrm02,
    DisplayCategory, GeometryType, LinePattern, LineStyleKey, LineStyleTable, RenderInstruction,
    S52Engine,
};

use super::clip::{clip_polyline, clip_triangle, outcode, triangulate_fan};
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

/// Shared LC pattern table (precomputed HPGL primitives), loaded once on first access
static LC_PATTERN_TABLE: OnceLock<Option<LcPatternTable>> = OnceLock::new();

/// Get the shared LC pattern table, loading it on first access.
pub(crate) fn get_lc_pattern_table() -> Option<&'static LcPatternTable> {
    LC_PATTERN_TABLE
        .get_or_init(|| {
            // First ensure line style table is loaded
            if let Some(line_styles) = get_line_style_table() {
                let patterns = LcPatternTable::from_line_style_table(line_styles);
                log::info!(
                    "Built LC pattern table: {} patterns with HPGL primitives",
                    patterns.len()
                );
                Some(patterns)
            } else {
                None
            }
        })
        .as_ref()
}

/// Check if a feature should be rendered at the given view scale.
///
/// Uses SCAMIN/SCAMAX attributes from S-57 to filter features.
/// Returns a scale factor (0.0 to 1.0) for soft-SCAMIN scaling.
/// If `bypass_scamin` is true, SCAMIN check is skipped (for DISPLAYBASE/GROUP1 features).
/// If `is_point_symbol` is true, applies soft SCAMIN (gradual fade up to 2x scamin).
/// `chart_native_scale` enables SUPER_SCAMIN: features without a SCAMIN attribute
/// are hidden when view_scale exceeds chart_scale * 2 (matching OpenCPN behavior).
fn should_render_at_scale_ex(
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

    // SUPER_SCAMIN: features without SCAMIN attribute use chart_scale * 2.
    // Prevents features from overview/small-scale charts showing at detailed zoom levels.
    if let Some(chart_scale) = chart_native_scale {
        let super_scamin = chart_scale as f64 * 2.0;
        if view_scale > super_scamin {
            return 0.0;
        }
    }

    1.0
}

/// Error type for tile building
#[derive(Debug)]
pub enum BuildError {
    Decrypt(crate::decrypt::DecryptError),
    Parse(SencError),
    NoKey(String),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::Decrypt(e) => write!(f, "Decrypt error: {}", e),
            BuildError::Parse(e) => write!(f, "Parse error: {:?}", e),
            BuildError::NoKey(name) => write!(f, "No key for chart: {}", name),
        }
    }
}

impl std::error::Error for BuildError {}

/// A vertex for area rendering (position + color index into palette)
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct AreaVertex {
    pub position: [f32; 2],
    pub color_index: u32,
    pub disp_prio: u32,
}

/// A batch of line vertices for a single style
#[derive(Debug)]
pub struct LineBatch {
    /// Batch key (pass order + S-52 style)
    pub key: LineBatchKey,
    pub vertices: Vec<LineVertex>,
    pub indices: Vec<u32>,
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
    /// Coverage mask triangles — triangulated M_COVR polygons from more-detailed charts.
    /// Used by GPU stencil pass to mask background chart geometry.
    /// Each vertex is [x, y] in Spherical Mercator.
    pub coverage_vertices: Vec<[f32; 2]>,
    /// Background area vertices (from charts covered by a more-detailed chart).
    /// Drawn with stencil test (pass when stencil==0) to suppress behind foreground.
    pub bg_area_vertices: Vec<AreaVertex>,
    /// Background area vertex offsets per priority level
    pub bg_area_priority_offsets: [u32; 11],
    /// Background pattern vertices (from charts covered by a more-detailed chart)
    pub bg_pattern_vertices: Vec<crate::render::PatternVertex>,
    /// Background pattern vertex offsets per priority level
    pub bg_pattern_priority_offsets: [u32; 11],
}

impl TilePacket {
    pub fn new(tile_id: TileId) -> Self {
        Self {
            tile_id,
            area_vertices: Vec::new(),
            area_priority_offsets: [0; 11],
            line_batches: Vec::new(),
            symbol_instances: Vec::new(),
            symbol_priority_offsets: [0; 11],
            text_instances: Vec::new(),
            label_candidates: Vec::new(),
            pattern_vertices: Vec::new(),
            pattern_priority_offsets: [0; 11],
            byte_size: 0,
            coverage_vertices: Vec::new(),
            bg_area_vertices: Vec::new(),
            bg_area_priority_offsets: [0; 11],
            bg_pattern_vertices: Vec::new(),
            bg_pattern_priority_offsets: [0; 11],
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
        let coverage_bytes = self.coverage_vertices.len() * std::mem::size_of::<[f32; 2]>();
        let bg_area_bytes = self.bg_area_vertices.len() * std::mem::size_of::<AreaVertex>();
        let bg_pattern_bytes =
            self.bg_pattern_vertices.len() * std::mem::size_of::<crate::render::PatternVertex>();
        self.byte_size = self.area_vertices.len() * std::mem::size_of::<AreaVertex>()
            + line_bytes
            + symbol_bytes
            + text_bytes
            + label_bytes
            + pattern_bytes
            + coverage_bytes
            + bg_area_bytes
            + bg_pattern_bytes;
    }

    /// Check if packet is empty
    pub fn is_empty(&self) -> bool {
        self.area_vertices.is_empty()
            && self.line_batches.is_empty()
            && self.symbol_instances.is_empty()
            && self.text_instances.is_empty()
            && self.label_candidates.is_empty()
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
    chart_cache: &'a Mutex<HashMap<u64, Arc<ChartData>>>,
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
}

fn instruction_summary(instructions: &[RenderInstruction]) -> String {
    let mut parts = Vec::new();
    for instr in instructions.iter().take(4) {
        let part = match instr {
            RenderInstruction::LineStyle { pattern, width, color } => {
                format!("LS({pattern:?},{width},{color})")
            }
            RenderInstruction::AreaColor { color } => format!("AC({color})"),
            RenderInstruction::AreaPattern { pattern } => format!("AP({pattern})"),
            RenderInstruction::Symbol { name } => format!("SY({name})"),
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
    coverage_polygons: Vec<Vec<[f64; 2]>>,
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

    /// Safety cap to prevent LC stamping from exploding vertex counts on dense tiles.
    /// This is an approximation path; missing some stamps is preferable to stalling.
    const MAX_LC_STAMPS_PER_TILE: usize = 5000;

    /// Create a new tile builder with its own internal cache (for standalone use)
    pub fn new(
        catalog: &'a ChartCatalog,
        keys: &'a KeyStore,
        decryptor: &'a Mutex<CachedDecryptor>,
        chart_cache: &'a Mutex<HashMap<u64, Arc<ChartData>>>,
    ) -> Self {
        Self {
            catalog,
            chart_cache,
            keys,
            decryptor,
            s52_engine: None,
            view_meters_per_pixel: 1.0,
            view_ppmm: 4.0, // 96 DPI baseline
            view_width_px: 0.0,
            view_height_px: 0.0,
            view_center_x: 0.0,
            view_center_y: 0.0,
        }
    }

    /// Create builder with external chart cache (for persistent use across frames)
    pub fn with_cache(
        catalog: &'a ChartCatalog,
        keys: &'a KeyStore,
        decryptor: &'a Mutex<CachedDecryptor>,
        chart_cache: &'a Mutex<HashMap<u64, Arc<ChartData>>>,
    ) -> Self {
        Self {
            catalog,
            chart_cache,
            keys,
            decryptor,
            s52_engine: None,
            view_meters_per_pixel: 1.0,
            view_ppmm: 4.0, // 96 DPI baseline
            view_width_px: 0.0,
            view_height_px: 0.0,
            view_center_x: 0.0,
            view_center_y: 0.0,
        }
    }

    /// Set the S-52 presentation engine for display category filtering
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
        // So N = meters_per_pixel / (0.001/ppmm) = meters_per_pixel * ppmm * 1000.
        let mpp = self.view_meters_per_pixel.max(1e-6) as f64;
        let ppmm = self.view_ppmm.max(1e-6) as f64;
        mpp * ppmm * 1000.0
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
            self.ensure_chart_loaded(info)?;
            let chart = self.chart_cache.lock().unwrap().get(&info.id).cloned();
            if let Some(chart) = chart {
                let (ref_mx, ref_my) = crate::tiles::latlon_to_mercator(info.ref_lat, info.ref_lon);
                let coverage_polygons =
                    extract_coverage_polygons(&chart, info, ref_mx, ref_my);
                loaded_charts.push(LoadedChartContext {
                    chart,
                    info,
                    ref_mx,
                    ref_my,
                    coverage_polygons,
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

        for (chart_index, ctx) in loaded_charts.iter().enumerate() {
            let detailed_coverages = collect_more_detailed_coverages(&loaded_charts, chart_index);
            writeln!(
                &mut report,
                "- chart {} (1:{}): {} coverage polygons, {} more-detailed masks",
                chart_debug_name(ctx.info),
                ctx.info.native_scale,
                ctx.coverage_polygons.len(),
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
                    && point_in_any_polygon([cx, cy], &detailed_coverages)
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
                if point_in_any_polygon([cx, cy], &detailed_coverages) && resolved.is_some() {
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
                if point_in_any_polygon([mx, my], &detailed_coverages) && resolved.is_some() {
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
                    if point_in_any_polygon(merc, &detailed_coverages) {
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
        let tile_scale_denom =
            super::meters_per_pixel(tile_id.z) * (self.view_ppmm as f64) * 1000.0;
        let charts = self.catalog.charts_for_tile_scaled(&bounds, tile_scale_denom);

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
        #[allow(clippy::type_complexity)]
        let mut lc_patterns: Vec<(
            String,
            String,
            u8,
            u8,
            u32,
            String,
            [f32; 2],
            Vec<Vec<[f32; 2]>>,
            bool, // is_background
        )> = Vec::new();
        let mut line_stats_total = LineBuildStats::default();

        let mut loaded_charts: Vec<LoadedChartContext<'_>> = Vec::new();

        // Process charts in scale order (small scale first = background)
        // Skip charts that fail to load - some may have parsing issues
        for info in charts.into_iter().take(Self::MAX_CHARTS_PER_TILE) {
            if let Err(e) = self.ensure_chart_loaded(info) {
                // Log but continue - don't fail entire tile for one bad chart
                eprintln!("Warning: Skipping chart {}: {}", info.name, e);
                continue;
            }

            // Clone Arc to release lock quickly — chart data is shared, not copied
            let chart = self.chart_cache.lock().unwrap().get(&info.id).cloned();
            if let Some(chart) = chart {
                let (ref_mx, ref_my) = crate::tiles::latlon_to_mercator(info.ref_lat, info.ref_lon);
                let coverage_polygons =
                    extract_coverage_polygons(&chart, info, ref_mx, ref_my);
                loaded_charts.push(LoadedChartContext {
                    chart,
                    info,
                    ref_mx,
                    ref_my,
                    coverage_polygons,
                });
            }
        }

        for (chart_index, ctx) in loaded_charts.iter().enumerate() {
            let detailed_coverages = collect_more_detailed_coverages(&loaded_charts, chart_index);

            if chart_index + 1 < loaded_charts.len()
                && tile_region_fully_covered(bounds, &detailed_coverages)
            {
                continue;
            }

            // A chart is "background" when a more-detailed chart overlaps it
            let is_background = !detailed_coverages.is_empty();

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
                    &mut lc_patterns,
                    &detailed_coverages,
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
                        &mut lc_patterns,
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
                            "    S-52: ls={} cs={} cs_fb={} lc={} lc_miss={} lc_fb={} lc_stamps={} skip_other={} no_lookup={}",
                            line_stats.styled_by_ls,
                            line_stats.styled_by_cs,
                            line_stats.styled_by_cs_fallback,
                            line_stats.styled_by_lc,
                            line_stats.styled_by_lc_missing,
                            line_stats.styled_by_lc_fallback,
                            line_stats.lc_stamps_emitted,
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

            // Generate LC pattern stamps from collected LC polylines
            if !lc_patterns.is_empty() {
                if let (Some(lc_table), Some(_pattern_table)) =
                    (get_line_style_table(), get_lc_pattern_table())
                {
                    let dpi = self.view_ppmm * 25.4;
                    let config = LcRenderConfig::new(dpi, self.view_meters_per_pixel);

                    for (
                        symbol_name,
                        color_ref,
                        disp_prio,
                        pass,
                        lookup_id,
                        acronym,
                        first_point,
                        polylines,
                        lc_is_background,
                    ) in lc_patterns
                    {
                        if let Some(symbol) = lc_table.get(&symbol_name) {
                            let color = if let Some(engine) = self.s52_engine {
                                engine.get_color(&color_ref).unwrap_or([0.0, 0.0, 0.0, 1.0])
                            } else {
                                [0.0, 0.0, 0.0, 1.0]
                            };

                            let advance_px = symbol.width_pixels(config.ppmm);
                            let advance_meters = advance_px * config.meters_per_pixel;

                            let phase_offset = compute_phase_offset(
                                lookup_id,
                                first_point,
                                &acronym,
                                advance_meters,
                            );

                            let mut pattern_polylines_by_width: BTreeMap<u8, Vec<Vec<[f32; 2]>>> =
                                BTreeMap::new();
                            for polyline in &polylines {
                                if line_stats_total.lc_stamps_emitted
                                    >= Self::MAX_LC_STAMPS_PER_TILE
                                {
                                    break;
                                }
                                let stamps = generate_stamps_along_polyline_with_phase(
                                    polyline,
                                    symbol,
                                    &config,
                                    color,
                                    phase_offset,
                                );
                                if !stamps.is_empty() {
                                    for (width, stamp_polylines) in
                                        stamps_to_polylines_by_width(&stamps, symbol, &config)
                                    {
                                        pattern_polylines_by_width
                                            .entry(width)
                                            .or_default()
                                            .extend(stamp_polylines);
                                    }
                                    line_stats_total.lc_stamps_emitted += stamps.len();
                                }
                            }

                            for (width, pattern_polylines) in pattern_polylines_by_width {
                                let (vertices, indices) =
                                    build_line_vertices_multi_indexed(&pattern_polylines);
                                if !vertices.is_empty() {
                                    line_stats_total.vertices_added += vertices.len();
                                    let lc_key = LineBatchKey::new_with_priority(
                                        disp_prio,
                                        pass,
                                        LineStyleKey::new(LinePattern::Solid, width, &color_ref),
                                        lookup_id,
                                        lc_is_background,
                                    );
                                    packet.line_batches.push(LineBatch {
                                        key: lc_key,
                                        vertices,
                                        indices,
                                    });
                                }
                            }
                        }
                    }
                }
            }

            if log::log_enabled!(log::Level::Debug) {
                log::debug!(
                    "  line totals: features={} batches={} verts={} lc_stamps={}",
                    line_stats_total.total_features,
                    packet.line_batches.len(),
                    line_stats_total.vertices_added,
                    line_stats_total.lc_stamps_emitted
                );
            }
        }

        // Sort ALL area vertices globally by priority and split into fg/bg buffers.
        // This ensures correct S-52 draw order across multi-chart tiles.
        // Background geometry is drawn with stencil test; foreground without.
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

        // Two-phase label decluttering: cheap AABB pre-check then full glyph layout (C4a).
        // Only ~20% of labels survive decluttering, so we save ~80% of layout_text() calls.
        packet.label_candidates = label_candidates;
        // Text layout and decluttering deferred to global pass

        // Triangulate coverage polygons from more-detailed charts for GPU stencil masking.
        // All charts except the first (least-detailed) contribute their coverage polygons.
        if loaded_charts.len() > 1 {
            for ctx in &loaded_charts[1..] {
                for polygon in &ctx.coverage_polygons {
                    // Fan triangulation from first vertex — works for convex/near-convex coverage polygons
                    if polygon.len() >= 3 {
                        let v0 = [polygon[0][0] as f32, polygon[0][1] as f32];
                        for i in 1..polygon.len() - 1 {
                            let v1 = [polygon[i][0] as f32, polygon[i][1] as f32];
                            let v2 = [polygon[i + 1][0] as f32, polygon[i + 1][1] as f32];
                            packet.coverage_vertices.push(v0);
                            packet.coverage_vertices.push(v1);
                            packet.coverage_vertices.push(v2);
                        }
                    }
                }
            }
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
    fn ensure_chart_loaded(&self, info: &ChartInfo) -> Result<(), BuildError> {
        {
            let cache = self.chart_cache.lock().unwrap();
            if cache.contains_key(&info.id) {
                return Ok(());
            }
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

        let chart = ChartData::parse(senc_bytes).map_err(BuildError::Parse)?;

        self.chart_cache
            .lock()
            .unwrap()
            .insert(info.id, Arc::new(chart));
        Ok(())
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
        lc_patterns: &mut Vec<(
            String,
            String,
            u8,
            u8,
            u32,
            String,
            [f32; 2],
            Vec<Vec<[f32; 2]>>,
            bool, // is_background
        )>,
        detailed_coverages: &[Vec<[f64; 2]>],
        is_background: bool,
    ) -> AreaBuildStats {
        let mut stats = AreaBuildStats::default();
        let mut logged_first_bbox = false;
        let before_verts = packet.area_vertices.len();

        // Edges owned by the high-priority coastline-formers (land area, coastline,
        // shoreline construction). Lower-priority area boundaries (e.g. the
        // magenta CTNARE caution boundary, prio 2) that share these edges are
        // suppressed so the coast is drawn once as the coastline — OpenCPN's
        // PrioritizeLineFeature shared-edge rule. Edge indices are per-chart.
        let coast_edges = collect_coast_edges(chart);

        for feature in chart.areas() {
            stats.total_features += 1;

            // SCAMIN/SCAMAX filtering: bypass SCAMIN for DISPLAYBASE features
            // (coastline, depth areas, safety contours must always be visible)
            let bypass_scamin = self
                .s52_engine
                .and_then(|e| e.get_display_category(feature.type_code, GeometryType::Area))
                .map(|cat| cat == DisplayCategory::Displaybase)
                .unwrap_or(false);
            let scamin_scale = should_render_at_scale_ex(
                feature,
                self.view_scale_denominator(),
                bypass_scamin,
                false,
                Some(info.native_scale),
            );
            if scamin_scale == 0.0 {
                continue;
            }

            // Resolve all S-52 info in ONE call (replaces 5-6 separate lookups)
            let engine = match self.s52_engine {
                Some(e) => e,
                None => continue,
            };
            let resolved = match engine.resolve_feature(feature, GeometryType::Area) {
                Some(r) => r,
                None => continue, // Filtered by display category
            };

            let has_nodata_fill = resolved.instructions.iter().any(|instr| {
                matches!(
                    instr,
                    RenderInstruction::AreaColor { color } if color == "NODTA"
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
                    && point_in_any_polygon([cx, cy], &detailed_coverages)
                {
                    continue;
                }
                // CPU coverage masking removed — GPU stencil now handles this.
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

                // Skip fill for unknown areas (no color index),
                // but still process boundary lines and text labels below.
                if let Some(color_index) = color_index_opt {
                    if relation == BBoxTileRelation::FullyInside {
                        // FAST PATH: feature bbox fully inside tile — emit vertices directly
                        // No clipping, no f64 conversion, no heap allocations
                        // (Coverage masking now handled by GPU stencil, not CPU filtering)
                        stats.fast_path_features += 1;
                        geom.for_each_vertex_direct(ref_mx as f32, ref_my as f32, |pos| {
                            packet.area_vertices.push(AreaVertex {
                                position: pos,
                                color_index: color_index as u32,
                                disp_prio: feature_priority as u32,
                            });
                            if let Some(pid) = pattern_id {
                                packet.pattern_vertices.push(crate::render::PatternVertex {
                                    position: pos,
                                    pattern_id: pid,
                                    offset: [0.0, 0.0],
                                    disp_prio: feature_priority as u32,
                                });
                            }
                        });
                    } else {
                        // SLOW PATH: feature straddles tile boundary — per-triangle clipping
                        stats.slow_path_features += 1;
                        geom.for_each_triangle_global(info.ref_lat, info.ref_lon, |tri| {
                            stats.triangles_seen += 1;

                            // CPU triangle masking removed — GPU stencil handles coverage masking.

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
                                    packet.area_vertices.push(AreaVertex {
                                        position: local,
                                        color_index: color_index as u32,
                                        disp_prio: feature_priority as u32,
                                    });
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
                                    packet.area_vertices.push(AreaVertex {
                                        position: local,
                                        color_index: color_index as u32,
                                        disp_prio: feature_priority as u32,
                                    });
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

                    if packet.area_vertices.len() == vert_start {
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
                        let min_x = info.extent_mercator.min_x as f32;
                        let min_y = info.extent_mercator.min_y as f32;
                        let max_x = info.extent_mercator.max_x as f32;
                        let max_y = info.extent_mercator.max_y as f32;
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
                    let first_point: [f32; 2] = rings
                        .iter()
                        .filter_map(|ring| ring.first())
                        .map(|p| [(ref_mx + p[0] as f64) as f32, (ref_my + p[1] as f64) as f32])
                        .min_by(|a, b| {
                            a[0].partial_cmp(&b[0])
                                .unwrap_or(std::cmp::Ordering::Equal)
                                .then(a[1].partial_cmp(&b[1]).unwrap_or(std::cmp::Ordering::Equal))
                        })
                        .unwrap_or([0.0, 0.0]);
                    let mut lc_polylines_by_pass: HashMap<u8, Vec<Vec<[f32; 2]>>> = HashMap::new();

                    for ring in &rings {
                        let mut global: Vec<[f64; 2]> = ring
                            .iter()
                            .map(|p| [ref_mx + p[0] as f64, ref_my + p[1] as f64])
                            .collect();
                        close_ring_if_needed(&mut global);
                        let clipped = clip_polyline(&global, bounds);
                        for segment in clipped {
                            if segment.len() < 2 {
                                continue;
                            }
                            let pts: Vec<[f32; 2]> = segment
                                .iter()
                                .map(|p| self.global_to_vertex(*p, bounds))
                                .collect();
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
                            for (pass_idx, _, _) in &lc_style_ops {
                                lc_polylines_by_pass
                                    .entry(*pass_idx)
                                    .or_default()
                                    .push(pts.clone());
                            }
                        }
                    }

                    if !lc_style_ops.is_empty() {
                        let acronym = s57_code_to_acronym(feature.type_code).to_string();
                        for (pass_idx, symbol_name, color_ref) in &lc_style_ops {
                            if let Some(polylines) = lc_polylines_by_pass.get(pass_idx) {
                                if !polylines.is_empty() {
                                    lc_patterns.push((
                                        symbol_name.clone(),
                                        color_ref.clone(),
                                        boundary_priority,
                                        *pass_idx,
                                        0,
                                        acronym.clone(),
                                        first_point,
                                        polylines.clone(),
                                        is_background,
                                    ));
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
	                                    position: [cx as f32, cy as f32],
	                                    text,
	                                    color,
	                                    color_index,
	                                    scale: s52_text_scale(*bsize, *weight),
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
        lc_patterns: &mut Vec<(
            String,
            String,
            u8,
            u8,
            u32,
            String,
            [f32; 2],
            Vec<Vec<[f32; 2]>>,
            bool, // is_background
        )>,
        _detailed_coverages: &[Vec<[f64; 2]>],
        is_background: bool,
        label_candidates: &mut Vec<TextParams>,
    ) -> LineBuildStats {
        let mut stats = LineBuildStats::default();

        for feature in chart.lines() {
            stats.total_features += 1;

            // SCAMIN/SCAMAX filtering: bypass SCAMIN for DISPLAYBASE features
            let bypass_scamin = self
                .s52_engine
                .and_then(|e| e.get_display_category(feature.type_code, GeometryType::Line))
                .map(|cat| cat == DisplayCategory::Displaybase)
                .unwrap_or(false);
            let scamin_scale = should_render_at_scale_ex(
                feature,
                self.view_scale_denominator(),
                bypass_scamin,
                false,
                Some(info.native_scale),
            );
            if scamin_scale == 0.0 {
                continue;
            }

            if let Some(ref geom) = feature.line_geometry {
                stats.with_geometry += 1;

                // CPU line centroid masking removed — GPU stencil now handles this
                // at pixel granularity via bg/fg buffer split + stencil test.

                let acronym = s57_code_to_acronym(feature.type_code);

                // === Unified S-52 resolution via resolve_feature() ===
                // Uses lookup_best_fast (skips attrs alloc for generic entries),
                // handles display category filtering, and CS expansion in one call.
                let lookup_result: Option<(
                    u8,
                    u32,
                    Vec<(u8, LineStyleKey, Option<(String, String)>)>,
                )> = if let Some(engine) = self.s52_engine {
                    let resolved = match engine.resolve_feature(feature, GeometryType::Line) {
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
                                        position: [cx as f32, cy as f32],
	                                        text: text.clone(),
	                                        color,
	                                        color_index,
	                                        scale: s52_text_scale(*bsize, *weight),
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
                    let fallback_key = legacy_line_style(feature, self.s52_engine);
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

                // Track LC polylines for this feature
                // LC info: (symbol_name, color_ref, pass)
                let mut feature_lc_polylines: Vec<Vec<[f32; 2]>> = Vec::new();
                let mut feature_lc_info: Option<(String, String, u8)> = None;
                // Capture a deterministic anchor point for phase hashing.
                // Use the minimum (x,y) among the first point of each polyline, so the
                // anchor is stable even if polyline ordering differs across loads/tiles.
                let first_point: [f32; 2] = polylines
                    .iter()
                    .filter_map(|pl| pl.first().copied())
                    .min_by(|a, b| {
                        a[0].partial_cmp(&b[0])
                            .unwrap_or(std::cmp::Ordering::Equal)
                            .then(a[1].partial_cmp(&b[1]).unwrap_or(std::cmp::Ordering::Equal))
                    })
                    .map(|p| [p[0] as f32, p[1] as f32])
                    .unwrap_or([0.0, 0.0]);

                // LOD: simplification tolerance = half a screen pixel in Mercator meters
                let simplify_eps = self.view_meters_per_pixel as f64 * 0.5;

                for polyline in polylines {
                    let clipped_segments = clip_polyline(&polyline, bounds);

                    for segment in clipped_segments {
                        // CPU polyline coverage masking removed — GPU stencil handles this.

                        // Douglas-Peucker simplification: drop sub-pixel detail
                        let segment = super::clip::simplify_polyline(&segment, simplify_eps);
                        if segment.len() < 2 {
                            continue;
                        }
                        stats.segments_out += segment.len() - 1;

                        let mut pts: Vec<[f32; 2]> = Vec::with_capacity(segment.len());
                        for p in segment {
                            pts.push(self.global_to_vertex(p, bounds));
                        }

                        // Collect LC info first (doesn't consume pts)
                        for (_pass, _key, lc_info) in &style_ops {
                            if let Some((sym, col)) = lc_info {
                                if feature_lc_info.is_none() {
                                    feature_lc_info = Some((sym.clone(), col.clone(), *_pass));
                                }
                            }
                        }

                        // Distribute pts to non-LC batch keys.
                        // Use move for the last consumer to avoid unnecessary clone.
                        let needs_for_lc = feature_lc_info.is_some();
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

                        if let Some((last, rest)) = non_lc_keys.split_last() {
                            for batch_key in rest {
                                polylines_by_key
                                    .entry(batch_key.clone())
                                    .or_default()
                                    .push(pts.clone());
                            }
                            if needs_for_lc {
                                // LC also needs pts, so clone for the last non-LC key
                                polylines_by_key
                                    .entry(last.clone())
                                    .or_default()
                                    .push(pts.clone());
                            } else {
                                // Last consumer: move instead of clone
                                polylines_by_key
                                    .entry(last.clone())
                                    .or_default()
                                    .push(std::mem::take(&mut pts));
                            }
                        }

                        // Collect polylines for LC pattern generation
                        if needs_for_lc {
                            feature_lc_polylines.push(pts);
                        }
                    }
                }

                // If this feature has LC pattern info, store for later processing
                if let Some((symbol_name, color_ref, pass)) = feature_lc_info {
                    if !feature_lc_polylines.is_empty() {
                        lc_patterns.push((
                            symbol_name,
                            color_ref,
                            disp_prio,
                            pass,
                            lookup_id,
                            acronym.to_string(),
                            first_point,
                            feature_lc_polylines,
                            is_background,
                        ));
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
        detailed_coverages: &[Vec<[f64; 2]>],
    ) -> SymbolBuildStats {
        use crate::senc::FeatureType;

        let mut stats = SymbolBuildStats::default();

        // Track last light position to avoid duplicate text at co-located lights
        let mut last_light_pos: Option<(f64, f64)> = None;

        for feature in &chart.features {
            // Only process point features
            if feature.feature_type != FeatureType::Point {
                continue;
            }
            stats.total_features += 1;

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
                Some(info.native_scale),
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

            let resolved = match engine.resolve_feature(feature, GeometryType::Point) {
                Some(r) => r,
                None => {
                    log::debug!("no LUP for point class={:?}", feature.object_class);
                    continue;
                }
            };

            // Emit every SY() instruction in the resolved (CS-expanded) list.
            // OpenCPN treats point portrayal as an ordered sequence of operations;
            // stopping at the first symbol drops supplementary marks from CS/LUPs.
            let mut symbol_ids: Vec<u32> = Vec::new();
            for instr in &resolved.instructions {
                if let RenderInstruction::Symbol { name } = instr {
                    if let Some(id) = crate::render::symbols::symbol_id_from_s52_name(name) {
                        symbol_ids.push(id);
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
            let (symbol_rotation_deg, orient_text) = if feature.object_class == ObjectClass::Light {
                let info = light_render_info(feature, &engine.settings);
                // If resolve_feature didn't find a symbol, use lights CS symbol
                if symbol_ids.is_empty() {
                    if let Some(id) = crate::render::symbols::symbol_id_from_s52_name(info.symbol_name)
                    {
                        symbol_ids.push(id);
                    }
                }
                (info.rotation_deg, info.orient_text)
            } else {
                (None, None)
            };

            if symbol_ids.is_empty() {
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

            if point_in_any_polygon([mx, my], detailed_coverages) {
                continue;
            }

            // Spatial prefilter: skip symbols outside tile bounds
            if mx < bounds.min_x || mx > bounds.max_x || my < bounds.min_y || my > bounds.max_y {
                continue;
            }
            stats.bbox_pass += 1;

            let rotation = symbol_rotation_deg
                .map(|deg| (deg as f32) * (std::f32::consts::PI / 180.0))
                .unwrap_or(0.0);

            // Use priority from resolved feature (already computed by resolve_feature)
            let sym_priority = resolved.priority;

            for symbol_id in symbol_ids {
                let sym_idx = packet.symbol_instances.len();
                packet.symbol_instances.push(SymbolInstance {
                    position: [mx as f32, my as f32],
                    symbol_id,
                    rotation,
                    disp_prio: sym_priority as u32,
                    scale: scamin_scale,
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
                let sector = light_sector_info(feature);
                if let Some(ref sector) = sector {
                    self.add_light_sector_lines(packet, [mx as f32, my as f32], sector);
                }

                // Generate light description text (only for first light at each position).
                // Sector lights carry unique directional information and must never be
                // deduped against a nearby all-round light — otherwise the sector arc is
                // drawn but the "Fl 3s 4m 3Nm" label silently disappears.
                let pos = (mx, my);
                let dedupe_radius_m = 15.0_f64;
                let is_first_at_pos = if sector.is_some() {
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
	                                position: [mx as f32, my as f32],
	                                text: desc,
	                                color,
	                                color_index,
	                                scale: s52_text_scale(11, 5),
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
	                            position: [mx as f32, my as f32],
	                            text,
	                            color,
	                            color_index,
	                            scale: s52_text_scale(10, 5),
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
	                            let color = engine
	                                .get_color(color_token)
	                                .unwrap_or([0.0, 0.0, 0.0, 1.0]);
	                            let color_index =
	                                engine.get_color_index(color_token).unwrap_or(0) as u32;
	                            label_candidates.push(TextParams {
	                                position: [mx as f32, my as f32],
	                                text,
	                                color,
	                                color_index,
	                                scale: s52_text_scale(*bsize, *weight),
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
        detailed_coverages: &[Vec<[f64; 2]>],
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
        let tile_mpp = super::meters_per_pixel(tile_id.z);
        let tile_view_scale = tile_mpp * (self.view_ppmm as f64) * 1000.0;

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

                if point_in_any_polygon([x, y], detailed_coverages) {
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
                    position: [x as f32, y as f32],
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

    fn add_light_sector_lines(
        &self,
        packet: &mut TilePacket,
        center: [f32; 2],
        sector: &crate::s52::LightSectorInfo,
    ) {
        let sectr1 = sector.sectr1;
        let sectr2 = if sector.sectr2 <= sectr1 {
            sector.sectr2 + 360.0
        } else {
            sector.sectr2
        };
        let sweep = sectr2 - sectr1;
        if sweep < 1.0 || sweep == 360.0 {
            return;
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

        let mm_to_meters =
            |mm: f64| mm * (self.view_ppmm as f64) * (self.view_meters_per_pixel as f64);
        let arc_radius_m = mm_to_meters(sector.arc_radius_mm);
        let sector_radius_m = mm_to_meters(sector.sector_radius_mm);

        let arc_points = light_sector_arc_points(center, arc_radius_m, s1, s2);
        if arc_points.len() < 2 {
            return;
        }

        let (outline_color, arc_color, arc_width) = if sector.faint {
            ("CHBLK", "CHBRN", 1u8)
        } else {
            ("OUTLW", sector.arc_color_token, 2u8)
        };

        let outline_key = LineStyleKey::new(LinePattern::Solid, 4, outline_color);
        let arc_key = LineStyleKey::new(LinePattern::Solid, arc_width, arc_color);
        let legs_key = LineStyleKey::new(LinePattern::Dashed, 2, "CHBLK");

        let priority = 4u8; // SYMB_POINT
        let lookup_id = 0u32;

        // Outline arc
        let (vertices, indices) = build_line_vertices_multi_indexed(&[arc_points.clone()]);
        if !vertices.is_empty() {
            let key = LineBatchKey::new_with_priority(priority, 0, outline_key, lookup_id, false);
            packet.line_batches.push(LineBatch {
                key,
                vertices,
                indices,
            });
        }

        // Color arc
        let (vertices, indices) = build_line_vertices_multi_indexed(&[arc_points]);
        if !vertices.is_empty() {
            let key = LineBatchKey::new_with_priority(priority, 1, arc_key, lookup_id, false);
            packet.line_batches.push(LineBatch {
                key,
                vertices,
                indices,
            });
        }

        // Sector legs
        let leg_lines = light_sector_leg_lines(center, sector_radius_m, s1, s2);
        if !leg_lines.is_empty() {
            let (vertices, indices) = build_line_vertices_multi_indexed(&leg_lines);
            if !vertices.is_empty() {
                let key = LineBatchKey::new_with_priority(priority, 2, legs_key, lookup_id, false);
                packet.line_batches.push(LineBatch {
                    key,
                    vertices,
                    indices,
                });
            }
        }
    }

    /// Convert global Mercator coordinates to f32 for GPU
    /// Keeps coordinates in global Mercator meters - camera view-projection handles transform
    fn global_to_vertex(&self, global: [f64; 2], _bounds: &TileBounds) -> [f32; 2] {
        // Keep in global Mercator - camera view-projection matrix does the transform
        [global[0] as f32, global[1] as f32]
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
            if let RenderInstruction::AreaColor { color } = instr {
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

fn sanitize_bitmap_text(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            'Æ' | 'Ǽ' => 'A',
            'æ' | 'ǽ' => 'a',
            'Ø' => 'O',
            'ø' => 'o',
            'Å' => 'A',
            'å' => 'a',
            'Ä' => 'A',
            'ä' => 'a',
            'Ö' => 'O',
            'ö' => 'o',
            'Ü' => 'U',
            'ü' => 'u',
            'É' | 'È' | 'Ê' | 'Ë' => 'E',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'Á' | 'À' | 'Â' => 'A',
            'á' | 'à' | 'â' => 'a',
            'Ó' | 'Ò' | 'Ô' => 'O',
            'ó' | 'ò' | 'ô' => 'o',
            'Í' | 'Ì' | 'Î' => 'I',
            'í' | 'ì' | 'î' => 'i',
            'Ú' | 'Ù' | 'Û' => 'U',
            'ú' | 'ù' | 'û' => 'u',
            'Ñ' => 'N',
            'ñ' => 'n',
            'Ç' => 'C',
            'ç' => 'c',
            _ if c.is_ascii() => c,
            _ => ' ',
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

fn point_in_polygon(point: [f64; 2], polygon: &[[f64; 2]]) -> bool {
    if polygon.len() < 3 {
        return false;
    }

    let mut inside = false;
    let mut j = polygon.len() - 1;
    for i in 0..polygon.len() {
        let xi = polygon[i][0];
        let yi = polygon[i][1];
        let xj = polygon[j][0];
        let yj = polygon[j][1];

        let intersects = ((yi > point[1]) != (yj > point[1]))
            && (point[0] < (xj - xi) * (point[1] - yi) / ((yj - yi) + 1e-12) + xi);
        if intersects {
            inside = !inside;
        }
        j = i;
    }

    inside
}

fn point_in_any_polygon(point: [f64; 2], polygons: &[Vec<[f64; 2]>]) -> bool {
    polygons.iter().any(|polygon| point_in_polygon(point, polygon))
}

// triangle_centroid and triangle_masked_by_coverage removed —
// GPU stencil masking replaces CPU per-triangle coverage checks.

fn relation_all_vertices_covered(
    geom: &crate::senc::AreaGeometry,
    ref_lat: f64,
    ref_lon: f64,
    polygons: &[Vec<[f64; 2]>],
) -> bool {
    if polygons.is_empty() {
        return false;
    }

    let mut any_vertex = false;
    let mut all_covered = true;
    geom.for_each_triangle_global(ref_lat, ref_lon, |tri| {
        for vertex in tri {
            any_vertex = true;
            if !point_in_any_polygon(vertex, polygons) {
                all_covered = false;
                break;
            }
        }
    });
    any_vertex && all_covered
}

fn mask_polyline_by_coverage(
    polyline: &[[f64; 2]],
    polygons: &[Vec<[f64; 2]>],
) -> Vec<Vec<[f64; 2]>> {
    if polygons.is_empty() || polyline.len() < 2 {
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

        if point_in_any_polygon(a, polygons)
            || point_in_any_polygon(b, polygons)
            || point_in_any_polygon(midpoint, polygons)
            || point_in_any_polygon(quarter_a, polygons)
            || point_in_any_polygon(quarter_b, polygons)
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

fn tile_region_fully_covered(bounds: TileBounds, polygons: &[Vec<[f64; 2]>]) -> bool {
    if polygons.is_empty() {
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
        .all(|point| point_in_any_polygon(point, polygons))
}

fn collect_more_detailed_coverages(
    loaded_charts: &[LoadedChartContext<'_>],
    chart_index: usize,
) -> Vec<Vec<[f64; 2]>> {
    loaded_charts[chart_index + 1..]
        .iter()
        .flat_map(|ctx| ctx.coverage_polygons.iter().cloned())
        .collect()
}

fn extract_coverage_polygons(
    chart: &ChartData,
    info: &ChartInfo,
    ref_mx: f64,
    ref_my: f64,
) -> Vec<Vec<[f64; 2]>> {
    let mut polygons = Vec::new();

    // Collect actual M_COVR polygons (CATCOV=1 means "coverage available")
    for feature in chart.areas() {
        if !feature.is_coverage() || feature.attribute_int("CATCOV") != Some(1) {
            continue;
        }

        let Some(geom) = &feature.area_geometry else {
            continue;
        };

        for ring in geom.resolve_rings(&chart.edge_table) {
            let mut polygon: Vec<[f64; 2]> = ring
                .iter()
                .map(|p| [ref_mx + p[0] as f64, ref_my + p[1] as f64])
                .collect();
            close_ring_if_needed(&mut polygon);
            if polygon.len() >= 4 {
                polygons.push(polygon);
            }
        }
    }

    // Only fall back to extent rectangle if no M_COVR features found
    if polygons.is_empty() {
        polygons.push(vec![
            [info.extent_mercator.min_x, info.extent_mercator.min_y],
            [info.extent_mercator.max_x, info.extent_mercator.min_y],
            [info.extent_mercator.max_x, info.extent_mercator.max_y],
            [info.extent_mercator.min_x, info.extent_mercator.max_y],
            [info.extent_mercator.min_x, info.extent_mercator.min_y],
        ]);
    }

    polygons
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
/// Until the TTF-atlas font lands (Gap 1), the 8x8 procedural bitmap cell is
/// scaled linearly by bsize. Weight 6+ ("bold") nudges the scale up slightly so
/// OBJNAM labels rendered bold in openCPN stay visually dominant here too.
fn s52_text_scale(bsize: u8, weight: u8) -> f32 {
    let base = (bsize as f32 / 8.0).clamp(0.9, 4.0);
    if weight >= 6 { base * 1.15 } else { base }
}

#[allow(dead_code)]
fn s52_text_scale_simple(bsize: u8) -> f32 {
    s52_text_scale(bsize, 5)
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

fn light_sector_arc_points(
    center: [f32; 2],
    radius_m: f64,
    start_deg: f64,
    end_deg: f64,
) -> Vec<[f32; 2]> {
    let sweep = end_deg - start_deg;
    if sweep <= 0.0 {
        return Vec::new();
    }

    let steps = (sweep / 3.0).ceil().clamp(8.0, 180.0) as usize;
    let step = sweep / steps as f64;

    let mut points = Vec::with_capacity(steps + 1);
    for i in 0..=steps {
        let angle_deg = start_deg + (i as f64 * step);
        let rad = angle_deg.to_radians();
        let dx = rad.sin() * radius_m;
        let dy = rad.cos() * radius_m;
        points.push([center[0] + dx as f32, center[1] + dy as f32]);
    }

    points
}

fn light_sector_leg_lines(
    center: [f32; 2],
    radius_m: f64,
    start_deg: f64,
    end_deg: f64,
) -> Vec<Vec<[f32; 2]>> {
    let mut lines = Vec::with_capacity(2);
    for angle_deg in [start_deg, end_deg] {
        let rad = angle_deg.to_radians();
        let dx = rad.sin() * radius_m;
        let dy = rad.cos() * radius_m;
        let end = [center[0] + dx as f32, center[1] + dy as f32];
        lines.push(vec![center, end]);
    }
    lines
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
    lc_stamps_emitted: usize,     // LC pattern stamps generated
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
        self.lc_stamps_emitted += other.lc_stamps_emitted;
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
fn legacy_line_style(feature: &Feature, s52_engine: Option<&S52Engine>) -> Option<LineStyleKey> {
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
                let style = depcnt02(feature, &engine.settings);
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
    use std::collections::HashMap;

    #[test]
    fn area_vertex_layout_matches_shader() {
        // chart.wgsl expects:
        // @location(0) position: vec2<f32>
        // @location(1) color_index: u32
        // @location(2) disp_prio: u32
        assert_eq!(std::mem::size_of::<AreaVertex>(), 16);
        assert_eq!(std::mem::align_of::<AreaVertex>(), 4);
    }

    #[test]
    fn tile_packet_byte_size() {
        let mut packet = TilePacket::new(TileId::new(10, 512, 512));
        packet.area_vertices.push(AreaVertex {
            position: [0.0, 0.0],
            color_index: 0,
            disp_prio: 0,
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
            symbol.color_ref, "ACSTLN",
            "LOWACC21 should use ACSTLN color"
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
        let mut attributes = HashMap::new();
        attributes.insert(
            "OBJNAM".to_string(),
            AttributeValue::String("Brondby".to_string()),
        );
        attributes.insert(
            "NOBJNM".to_string(),
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

        assert_eq!(
            get_text_for_attribute(&feature, "OBJNAM", None),
            Some("Brondby".to_string())
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
    fn point_in_polygon_detects_coverage_hits() {
        let polygon = vec![
            [0.0, 0.0],
            [10.0, 0.0],
            [10.0, 10.0],
            [0.0, 10.0],
            [0.0, 0.0],
        ];

        assert!(point_in_polygon([5.0, 5.0], &polygon));
        assert!(!point_in_polygon([15.0, 5.0], &polygon));
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
