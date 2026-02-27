//! Tile builder - clips geometry to tile bounds for rendering.
//!
//! Builds TilePackets (CPU-side) from chart data, ready for GPU upload.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use bytemuck::{Pod, Zeroable};

use crate::cache::CachedDecryptor;
use crate::decrypt::KeyStore;
use crate::render::{build_line_vertices_multi_indexed, LineVertex};
use crate::render::lc_pattern::{LcPatternTable, LcRenderConfig, compute_phase_offset, generate_stamps_along_polyline_with_phase, stamps_to_polylines};
use crate::render::symbols::SymbolInstance;
use crate::render::text::SoundingInstance;
use crate::s52::{
    depare02_color_token, depcnt02, light_render_info, light_sector_info,
    litdsn01, sndfrm02, DisplayCategory, GeometryType,
    LinePattern, LineStyleKey, LineStyleTable, RenderInstruction, S52Engine,
};
use crate::render::{declutter_and_layout_labels, HJust, LabelGlyphInstance, TextParams, VJust};

use crate::senc::{ChartCatalog, ChartData, ChartInfo, Feature, SencError, ObjectClass};
use super::{LineBatchKey, TileBounds, TileId};
use super::clip::{clip_triangle, clip_polyline, triangulate_fan, outcode};
use crate::senc::BBoxTileRelation;


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
    LINE_STYLE_TABLE.get_or_init(|| {
        match LineStyleTable::load_from_xml(CHARTSYMBOLS_PATH) {
            Ok(table) => {
                log::info!(
                    "Loaded S-52 line style table: {} LC symbols",
                    table.len()
                );
                Some(table)
            }
            Err(e) => {
                log::warn!("Failed to load S-52 line style table: {}", e);
                None
            }
        }
    }).as_ref()
}

/// Shared LC pattern table (precomputed HPGL primitives), loaded once on first access
static LC_PATTERN_TABLE: OnceLock<Option<LcPatternTable>> = OnceLock::new();

/// Get the shared LC pattern table, loading it on first access.
pub(crate) fn get_lc_pattern_table() -> Option<&'static LcPatternTable> {
    LC_PATTERN_TABLE.get_or_init(|| {
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
    }).as_ref()
}

/// Check if a feature should be rendered at the given view scale.
///
/// Uses SCAMIN/SCAMAX attributes from S-57 to filter features:
/// - SCAMIN: minimum scale (largest denominator) at which feature should display
/// - SCAMAX: maximum scale (smallest denominator) at which feature should display
///
/// Scale is the denominator in 1:N (e.g., 50000 for 1:50000).
/// If `bypass_scamin` is true, SCAMIN check is skipped (for DISPLAYBASE/GROUP1 features).
/// Returns true if feature should be rendered, false to skip.
fn should_render_at_scale(feature: &Feature, view_scale: f64) -> bool {
    should_render_at_scale_ex(feature, view_scale, false)
}

fn should_render_at_scale_ex(feature: &Feature, view_scale: f64, bypass_scamin: bool) -> bool {
    // SCAMIN check: feature should not display at scales smaller than SCAMIN
    // (i.e., when zoomed out beyond SCAMIN)
    if !bypass_scamin {
        if let Some(scamin) = feature.scamin() {
            if view_scale > scamin {
                return false;
            }
        }
    }

    // SCAMAX check: feature should not display at scales larger than SCAMAX
    // (i.e., when zoomed in beyond SCAMAX)
    if let Some(scamax) = feature.scamax() {
        if view_scale < scamax {
            return false;
        }
    }

    true
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
    /// Label glyph instances (light text, etc.)
    pub label_glyphs: Vec<LabelGlyphInstance>,
    /// Pattern-filled area vertices (sorted by priority)
    pub pattern_vertices: Vec<crate::render::PatternVertex>,
    /// Pattern vertex offsets per priority level
    pub pattern_priority_offsets: [u32; 11],
    /// Estimated byte size for cache budgeting
    pub byte_size: usize,
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
            label_glyphs: Vec::new(),
            pattern_vertices: Vec::new(),
            pattern_priority_offsets: [0; 11],
            byte_size: 0,
        }
    }

    /// Compute byte size for cache budgeting
    pub fn compute_byte_size(&mut self) {
        let line_bytes: usize = self.line_batches.iter()
            .map(|b| b.vertices.len() * std::mem::size_of::<LineVertex>()
                   + b.indices.len() * std::mem::size_of::<u32>())
            .sum();
        let symbol_bytes = self.symbol_instances.len() * std::mem::size_of::<SymbolInstance>();
        let text_bytes = self.text_instances.len() * std::mem::size_of::<SoundingInstance>();
        let label_bytes = self.label_glyphs.len() * std::mem::size_of::<LabelGlyphInstance>();
        let pattern_bytes = self.pattern_vertices.len() * std::mem::size_of::<crate::render::PatternVertex>();
        self.byte_size = self.area_vertices.len() * std::mem::size_of::<AreaVertex>()
            + line_bytes + symbol_bytes + text_bytes + label_bytes + pattern_bytes;
    }

    /// Check if packet is empty
    pub fn is_empty(&self) -> bool {
        self.area_vertices.is_empty()
            && self.line_batches.is_empty()
            && self.symbol_instances.is_empty()
            && self.text_instances.is_empty()
            && self.label_glyphs.is_empty()
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

    fn build_cpu_impl(&self, tile_id: TileId, include_lines: bool) -> Result<TilePacket, BuildError> {
        let bounds = tile_id.bounds();
        let charts = self.catalog.charts_for_tile(&bounds);

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
        // Deferred label candidates for two-phase decluttering (C4a).
        // Collected during build_areas/build_symbols, laid out only after AABB pre-check.
        let mut label_candidates: Vec<TextParams> = Vec::new();

        // Shared line collections across all charts — merged batches reduce GPU buffer count
        let mut polylines_by_key: HashMap<LineBatchKey, Vec<Vec<[f32; 2]>>> = HashMap::with_capacity(32);
        #[allow(clippy::type_complexity)]
        let mut lc_patterns: Vec<(String, String, u8, u8, u32, String, [f32; 2], Vec<Vec<[f32; 2]>>)> = Vec::new();
        let mut line_stats_total = LineBuildStats::default();

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
                // Pre-compute reference point Mercator once per chart (avoids 3x redundant tan+ln)
                let (ref_mx, ref_my) = crate::tiles::latlon_to_mercator(info.ref_lat, info.ref_lon);

                // Process area features with spatial prefilter
                let area_stats = self.build_areas(&chart, info, &bounds, &mut packet, ref_mx, ref_my, &mut label_candidates);
                if log::log_enabled!(log::Level::Debug) {
                    log::debug!(
                        "  chart '{}' areas: total={} with_geom={} bbox_pass={} fast={} slow={} tris_seen={} trivial_accept={} trivial_reject={} tris_clipped={} verts_added={}",
                        info.name,
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
                    let line_stats = self.build_lines(&chart, info, &bounds, &mut polylines_by_key, &mut lc_patterns);
                    if log::log_enabled!(log::Level::Debug) {
                        log::debug!(
                            "  chart '{}' lines: total={} with_geom={} bbox_pass={} segments_out={} verts_added={}",
                            info.name,
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
                let symbol_stats = self.build_symbols(&chart, info, &bounds, &mut packet, &mut all_symbol_priorities, &mut label_candidates);
                if log::log_enabled!(log::Level::Debug) {
                    log::debug!(
                        "  chart '{}' symbols: total_pts={} matched={} bbox_pass={} added={}",
                        info.name,
                        symbol_stats.total_features,
                        symbol_stats.matched_symbol,
                        symbol_stats.bbox_pass,
                        symbol_stats.instances_added
                    );
                }

                let soundings_added = self.build_soundings(&chart, info, tile_id, &bounds, &mut packet, ref_mx, ref_my);
                if log::log_enabled!(log::Level::Debug) && soundings_added > 0 {
                    log::debug!(
                        "  chart '{}' soundings: added={}",
                        info.name,
                        soundings_added
                    );
                }

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
                if let (Some(lc_table), Some(_pattern_table)) = (get_line_style_table(), get_lc_pattern_table()) {
                    let dpi = self.view_ppmm * 25.4;
                    let config = LcRenderConfig::new(dpi, self.view_meters_per_pixel);

                    for (symbol_name, color_ref, disp_prio, pass, lookup_id, acronym, first_point, polylines) in lc_patterns {
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

                            let mut pattern_polylines: Vec<Vec<[f32; 2]>> = Vec::new();
                            for polyline in &polylines {
                                if line_stats_total.lc_stamps_emitted >= Self::MAX_LC_STAMPS_PER_TILE {
                                    break;
                                }
                                let stamps = generate_stamps_along_polyline_with_phase(
                                    polyline, symbol, &config, color, phase_offset
                                );
                                if !stamps.is_empty() {
                                    let stamp_polylines = stamps_to_polylines(&stamps, symbol, &config);
                                    pattern_polylines.extend(stamp_polylines);
                                    line_stats_total.lc_stamps_emitted += stamps.len();
                                }
                            }

                            if !pattern_polylines.is_empty() {
                                let (vertices, indices) = build_line_vertices_multi_indexed(&pattern_polylines);
                                if !vertices.is_empty() {
                                    line_stats_total.vertices_added += vertices.len();
                                    let lc_key = LineBatchKey::new_with_priority(
                                        disp_prio,
                                        pass,
                                        LineStyleKey::new(LinePattern::Solid, 1, &color_ref),
                                        lookup_id,
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
        packet.label_glyphs = declutter_and_layout_labels(&label_candidates);
        crate::render::text_layout::declutter_soundings(&mut packet.text_instances);

        packet.compute_byte_size();
        if log::log_enabled!(log::Level::Debug) {
            log::debug!(
                "  tile {:?} packet: area_verts={} line_batches={} line_verts={} symbols={} labels={} soundings={} bytes={}",
                tile_id,
                packet.area_vertices.len(),
                packet.line_batches.len(),
                packet.total_line_vertices(),
                packet.symbol_instances.len(),
                packet.label_glyphs.len(),
                packet.text_instances.len(),
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
        let chart_name = info.path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_default();

        let install_key = self.keys.lookup(&chart_name)
            .ok_or_else(|| BuildError::NoKey(chart_name.clone()))?
            .to_string();

        let senc_bytes = self.decryptor.lock().unwrap().decrypt_chart(&info.path, &install_key)
            .map_err(BuildError::Decrypt)?;

        let chart = ChartData::parse(senc_bytes)
            .map_err(BuildError::Parse)?;

        self.chart_cache.lock().unwrap().insert(info.id, Arc::new(chart));
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
    ) -> AreaBuildStats {
        let mut stats = AreaBuildStats::default();
        let mut logged_first_bbox = false;
        let before_verts = packet.area_vertices.len();
        let before_patterns = packet.pattern_vertices.len();
        // Track (priority, vert_start, vert_end, pat_start, pat_end) per feature for priority sorting
        let mut feature_ranges: Vec<(u8, usize, usize, usize, usize)> = Vec::new();

        for feature in chart.areas() {
            stats.total_features += 1;

            // SCAMIN/SCAMAX filtering: bypass SCAMIN for DISPLAYBASE features
            // (coastline, depth areas, safety contours must always be visible)
            let bypass_scamin = self.s52_engine
                .and_then(|e| e.get_display_category(feature.object_class, GeometryType::Area))
                .map(|cat| cat == DisplayCategory::Displaybase)
                .unwrap_or(false);
            if !should_render_at_scale_ex(feature, self.view_scale_denominator(), bypass_scamin) {
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

            if let Some(ref geom) = feature.area_geometry {
                stats.with_geometry += 1;

                if log::log_enabled!(log::Level::Debug) && !logged_first_bbox {
                    let bbox = geom.extent;
                    let (min_x_m, min_y_m) =
                        crate::tiles::latlon_to_mercator(bbox.min_y, bbox.min_x);
                    let (max_x_m, max_y_m) =
                        crate::tiles::latlon_to_mercator(bbox.max_y, bbox.max_x);
                    log::debug!(
                        "  first area bbox (WGS84): lon=({:.4},{:.4}) lat=({:.4},{:.4})",
                        bbox.min_x,
                        bbox.max_x,
                        bbox.min_y,
                        bbox.max_y
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
                let relation = geom.extent.tile_relation(bounds);
                if relation == BBoxTileRelation::Outside {
                    continue;
                }
                stats.bbox_pass += 1;

                let feature_priority = resolved.priority;
                let vert_start = packet.area_vertices.len();
                let pat_start = packet.pattern_vertices.len();

                // Extract color index from resolved instructions (first AC instruction)
                let color_index_opt = self.extract_area_color_index(feature, engine, &resolved.instructions);

                // Extract AP() pattern id from resolved instructions
                let pattern_id = resolved.instructions.iter().find_map(|instr| {
                    if let RenderInstruction::AreaPattern { pattern } = instr {
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
                        stats.fast_path_features += 1;
                        geom.for_each_vertex_direct(ref_mx as f32, ref_my as f32, |pos| {
                            packet.area_vertices.push(AreaVertex {
                                position: pos,
                                color_index: color_index as u32,
                            });
                            if let Some(pid) = pattern_id {
                                packet.pattern_vertices.push(crate::render::PatternVertex {
                                    position: pos,
                                    pattern_id: pid,
                                    offset: [0.0, 0.0],
                                });
                            }
                        });
                    } else {
                        // SLOW PATH: feature straddles tile boundary — per-triangle clipping
                        stats.slow_path_features += 1;
                        geom.for_each_triangle_global(info.ref_lat, info.ref_lon, |tri| {
                            stats.triangles_seen += 1;

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
                                    });
                                    if let Some(pid) = pattern_id {
                                        packet.pattern_vertices.push(crate::render::PatternVertex {
                                            position: local,
                                            pattern_id: pid,
                                            offset: [0.0, 0.0],
                                        });
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
                                    });
                                    if let Some(pid) = pattern_id {
                                        packet.pattern_vertices.push(crate::render::PatternVertex {
                                            position: local,
                                            pattern_id: pid,
                                            offset: [0.0, 0.0],
                                        });
                                    }
                                }
                            }
                        });
                    }
                } // end color_index gate

                // Track vertex range and priority for this feature
                let vert_end = packet.area_vertices.len();
                let pat_end = packet.pattern_vertices.len();
                if vert_end > vert_start || pat_end > pat_start {
                    feature_ranges.push((feature_priority, vert_start, vert_end, pat_start, pat_end));
                }

                // Extract boundary line styles from resolved instructions (already CS-expanded)
                let mut line_styles: Vec<LineStyleKey> = Vec::new();
                for instr in &resolved.instructions {
                    if let RenderInstruction::LineStyle { pattern, width, color } = instr {
                        line_styles.push(LineStyleKey::new(*pattern, *width, color));
                    }
                }

                if !line_styles.is_empty() {
                    let rings = geom.resolve_rings(&chart.edge_table);
                    for ring in &rings {
                        let global: Vec<[f64; 2]> = ring.iter()
                            .map(|p| [ref_mx + p[0] as f64, ref_my + p[1] as f64])
                            .collect();
                        let clipped = clip_polyline(&global, bounds);
                        for segment in clipped {
                            if segment.len() < 2 { continue; }
                            let pts: Vec<[f32; 2]> = segment.iter()
                                .map(|p| self.global_to_vertex(*p, bounds))
                                .collect();
                            for style in &line_styles {
                                let key = LineBatchKey::new_with_priority(4, 0, style.clone(), 0);
                                let (vertices, indices) = build_line_vertices_multi_indexed(&[pts.clone()]);
                                if !vertices.is_empty() {
                                    packet.line_batches.push(LineBatch { key, vertices, indices });
                                }
                            }
                        }
                    }
                }

                // Collect text label candidates from resolved instructions (deferred layout)
                for instr in &resolved.instructions {
                    if let RenderInstruction::Text { attribute, format, hjust, vjust, xoffs, yoffs, color: color_token } = instr {
                        let text_value = get_text_for_attribute(feature, attribute, format.as_deref());
                        if let Some(text) = text_value {
                            let bbox = geom.extent;
                            let (cx, cy) = crate::tiles::latlon_to_mercator(
                                (bbox.min_y + bbox.max_y) / 2.0,
                                (bbox.min_x + bbox.max_x) / 2.0,
                            );
                            if cx >= bounds.min_x && cx <= bounds.max_x
                                && cy >= bounds.min_y && cy <= bounds.max_y
                            {
                                let color = engine.get_color(color_token)
                                    .unwrap_or([0.0, 0.0, 0.0, 1.0]);
                                label_candidates.push(TextParams {
                                    position: [cx as f32, cy as f32],
                                    text,
                                    color,
                                    scale: 1.0,
                                    hjust: HJust::from(*hjust),
                                    vjust: VJust::from(*vjust),
                                    xoffs: *xoffs as i32,
                                    yoffs: *yoffs as i32,
                                });
                            }
                        }
                    }
                }
            }
        }

        stats.vertices_added = packet.area_vertices.len().saturating_sub(before_verts);

        // Sort area vertices by priority and compute priority offsets.
        // This ensures correct S-52 draw order (low priority areas drawn first).
        if !feature_ranges.is_empty() {
            // Sort feature ranges by priority (stable sort preserves within-priority order)
            feature_ranges.sort_by_key(|r| r.0);

            // Rebuild area_vertices and pattern_vertices in priority order
            let old_areas = packet.area_vertices[before_verts..].to_vec();
            let old_patterns = packet.pattern_vertices[before_patterns..].to_vec();
            packet.area_vertices.truncate(before_verts);
            packet.pattern_vertices.truncate(before_patterns);

            let mut area_offsets = [0u32; 11];
            let mut pattern_offsets = [0u32; 11];

            for &(prio, vs, ve, ps, pe) in &feature_ranges {
                let _p = (prio as usize).min(9);
                // Copy area vertices for this feature
                let adjusted_vs = vs - before_verts;
                let adjusted_ve = ve - before_verts;
                packet.area_vertices.extend_from_slice(&old_areas[adjusted_vs..adjusted_ve]);
                // Copy pattern vertices for this feature
                let adjusted_ps = ps - before_patterns;
                let adjusted_pe = pe - before_patterns;
                packet.pattern_vertices.extend_from_slice(&old_patterns[adjusted_ps..adjusted_pe]);
            }

            // Compute priority offsets by scanning the sorted feature ranges
            let mut area_pos = before_verts as u32;
            let mut pattern_pos = before_patterns as u32;
            let mut current_prio = 0usize;
            for &(prio, vs, ve, ps, pe) in &feature_ranges {
                let p = (prio as usize).min(9);
                // Fill offsets for priorities we skipped
                while current_prio <= p {
                    area_offsets[current_prio] = area_pos;
                    pattern_offsets[current_prio] = pattern_pos;
                    current_prio += 1;
                }
                area_pos += (ve - vs) as u32;
                pattern_pos += (pe - ps) as u32;
            }
            // Fill remaining priorities
            while current_prio <= 10 {
                area_offsets[current_prio] = area_pos;
                pattern_offsets[current_prio] = pattern_pos;
                current_prio += 1;
            }

            packet.area_priority_offsets = area_offsets;
            packet.pattern_priority_offsets = pattern_offsets;
        }

        stats
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
        lc_patterns: &mut Vec<(String, String, u8, u8, u32, String, [f32; 2], Vec<Vec<[f32; 2]>>)>,
    ) -> LineBuildStats {
        let mut stats = LineBuildStats::default();

        for feature in chart.lines() {
            stats.total_features += 1;

            // SCAMIN/SCAMAX filtering: bypass SCAMIN for DISPLAYBASE features
            let bypass_scamin = self.s52_engine
                .and_then(|e| e.get_display_category(feature.object_class, GeometryType::Line))
                .map(|cat| cat == DisplayCategory::Displaybase)
                .unwrap_or(false);
            if !should_render_at_scale_ex(feature, self.view_scale_denominator(), bypass_scamin) {
                continue;
            }

            if let Some(ref geom) = feature.line_geometry {
                stats.with_geometry += 1;

                let acronym = feature.object_class.acronym();

                // === Unified S-52 resolution via resolve_feature() ===
                // Uses lookup_best_fast (skips attrs alloc for generic entries),
                // handles display category filtering, and CS expansion in one call.
                let lookup_result: Option<(u8, u32, Vec<(u8, LineStyleKey, Option<(String, String)>)>)> = if let Some(engine) = self.s52_engine {
                    let resolved = match engine.resolve_feature(feature, GeometryType::Line) {
                        Some(r) => r,
                        None => {
                            // No LUP match or filtered by display category
                            stats.skipped_other += 1;
                            continue;
                        }
                    };

                    let disp_prio = resolved.priority;

                    // Extract line style ops from resolved (CS-expanded) instructions
                    let mut style_ops: Vec<(u8, LineStyleKey, Option<(String, String)>)> = Vec::new();
                    let mut pass_idx: u8 = 0;

                    for instr in &resolved.instructions {
                        match instr {
                            RenderInstruction::LineStyle { pattern, width, color } => {
                                stats.styled_by_ls += 1;
                                style_ops.push((pass_idx, LineStyleKey::new(*pattern, *width, color), None));
                                pass_idx += 1;
                            }
                            RenderInstruction::LineComplex { name } => {
                                if let Some(lc_table) = get_line_style_table() {
                                    if let Some(symbol) = lc_table.get(name) {
                                        stats.styled_by_lc += 1;
                                        log::debug!("LC({}) for {} -> color={}", name, acronym, symbol.color_ref);
                                        style_ops.push((
                                            pass_idx,
                                            LineStyleKey::new(LinePattern::Solid, 1, &symbol.color_ref),
                                            Some((name.clone(), symbol.color_ref.clone())),
                                        ));
                                    } else {
                                        stats.styled_by_lc_missing += 1;
                                        log::trace!("LC({}) symbol not found for {}", name, acronym);
                                        style_ops.push((pass_idx, LineStyleKey::new(LinePattern::Solid, 1, "CHBLK"), None));
                                    }
                                } else {
                                    stats.styled_by_lc_fallback += 1;
                                    log::trace!("LC({}) no table for {}", name, acronym);
                                    style_ops.push((pass_idx, LineStyleKey::new(LinePattern::Solid, 1, "CHBLK"), None));
                                }
                                pass_idx += 1;
                            }
                            _ => {} // Skip non-line instructions (SY, TX, AC, etc.)
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
                        a[0]
                            .partial_cmp(&b[0])
                            .unwrap_or(std::cmp::Ordering::Equal)
                            .then(
                                a[1]
                                    .partial_cmp(&b[1])
                                    .unwrap_or(std::cmp::Ordering::Equal),
                            )
                    })
                    .map(|p| [p[0] as f32, p[1] as f32])
                    .unwrap_or([0.0, 0.0]);

                // LOD: simplification tolerance = half a screen pixel in Mercator meters
                let simplify_eps = self.view_meters_per_pixel as f64 * 0.5;

                for polyline in polylines {
                    let clipped_segments = clip_polyline(&polyline, bounds);

                    for segment in clipped_segments {
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
                        let non_lc_keys: Vec<LineBatchKey> = style_ops.iter()
                            .filter(|(_, _, lc)| lc.is_none())
                            .map(|(pass, key, _)| LineBatchKey::new_with_priority(disp_prio, *pass, key.clone(), lookup_id))
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
        _info: &ChartInfo,
        bounds: &TileBounds,
        packet: &mut TilePacket,
        all_symbol_priorities: &mut Vec<(u8, usize)>,
        label_candidates: &mut Vec<TextParams>,
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
            let bypass_scamin = self.s52_engine
                .and_then(|e| e.get_display_category(feature.object_class, GeometryType::Point))
                .map(|cat| cat == DisplayCategory::Displaybase)
                .unwrap_or(false);
            if !should_render_at_scale_ex(feature, self.view_scale_denominator(), bypass_scamin) {
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

            // Find first SY() instruction in the resolved (CS-expanded) instructions
            let mut symbol_id: Option<u32> = None;
            for instr in &resolved.instructions {
                if let RenderInstruction::Symbol { name } = instr {
                    if let Some(id) = crate::render::symbols::symbol_id_from_s52_name(name) {
                        symbol_id = Some(id);
                        break;
                    } else {
                        log::debug!("symbol '{}' not in atlas for class={:?}", name, feature.object_class);
                    }
                }
            }

            // LIGHTS special handling: rotation and sector info
            let (symbol_rotation_deg, orient_text) = if feature.object_class == ObjectClass::Light {
                let info = light_render_info(feature, &engine.settings);
                // If resolve_feature didn't find a symbol, use lights CS symbol
                if symbol_id.is_none() {
                    symbol_id = crate::render::symbols::symbol_id_from_s52_name(info.symbol_name);
                }
                (info.rotation_deg, info.orient_text)
            } else {
                (None, None)
            };

            let symbol_id = match symbol_id {
                Some(id) => id,
                None => {
                    log::debug!("no atlas symbol for class={:?}", feature.object_class);
                    continue;
                }
            };
            stats.matched_symbol += 1;

            // Get point geometry
            let point_geom = match &feature.point_geometry {
                Some(pg) => pg,
                None => continue,
            };
            stats.with_geometry += 1;

            // Convert WGS84 (lat, lon) to global Mercator
            // OSENC stores: pg.x = latitude, pg.y = longitude (opposite of typical convention)
            let (mx, my) = super::latlon_to_mercator(point_geom.x, point_geom.y);

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

            let sym_idx = packet.symbol_instances.len();
            packet.symbol_instances.push(SymbolInstance {
                position: [mx as f32, my as f32],
                symbol_id,
                rotation,
            });
            all_symbol_priorities.push((sym_priority, sym_idx));
            stats.instances_added += 1;

            if feature.object_class == ObjectClass::Light {
                log::trace!("light feature at ({:.1},{:.1}), is_first will be checked next", mx, my);
                if let Some(sector) = light_sector_info(feature) {
                    self.add_light_sector_lines(packet, [mx as f32, my as f32], &sector);
                }

                // Generate light description text (only for first light at each position)
                let pos = (mx, my);
                let is_first_at_pos = match last_light_pos {
                    Some(last) => (last.0 - pos.0).abs() > 0.01 || (last.1 - pos.1).abs() > 0.01,
                    None => true,
                };

                if is_first_at_pos {
                    let desc = litdsn01(feature);
                    log::trace!("litdsn01 result for light at ({:.1},{:.1}): {:?}", mx, my, desc);
                    if let Some(desc) = desc {
                        // Collect light description as candidate (deferred layout)
                        let color = self.s52_engine
                            .and_then(|e| e.get_color("CHBLK"))
                            .unwrap_or([0.0, 0.0, 0.0, 1.0]);
                        label_candidates.push(TextParams {
                            position: [mx as f32, my as f32],
                            text: desc,
                            color,
                            scale: 1.0,
                            hjust: HJust::Left,
                            vjust: VJust::Center,
                            xoffs: 2, // 2 char widths right of symbol
                            yoffs: 0,
                        });
                    }
                    if let Some(text) = orient_text {
                        let color = self.s52_engine
                            .and_then(|e| e.get_color("CHBLK"))
                            .unwrap_or([0.0, 0.0, 0.0, 1.0]);
                        label_candidates.push(TextParams {
                            position: [mx as f32, my as f32],
                            text,
                            color,
                            scale: 1.0,
                            hjust: HJust::Left,
                            vjust: VJust::Top,
                            xoffs: 3,
                            yoffs: 1,
                        });
                    }
                    log::trace!("label_candidates after light text: {}", label_candidates.len());
                }
                last_light_pos = Some(pos);
            } else {
                // Collect text label candidates from TX/TE instructions (deferred layout)
                for instr in &resolved.instructions {
                    if let RenderInstruction::Text { attribute, format, hjust, vjust, xoffs, yoffs, color: color_token } = instr {
                        let text_value = get_text_for_attribute(feature, attribute, format.as_deref());
                        if let Some(text) = text_value {
                            let color = engine.get_color(color_token)
                                .unwrap_or([0.0, 0.0, 0.0, 1.0]);
                            label_candidates.push(TextParams {
                                position: [mx as f32, my as f32],
                                text,
                                color,
                                scale: 1.0,
                                hjust: HJust::from(*hjust),
                                vjust: VJust::from(*vjust),
                                xoffs: *xoffs as i32,
                                yoffs: *yoffs as i32,
                            });
                        }
                    }
                }
            }
        }

        log::trace!("build_symbols total label_glyphs: {}", packet.label_glyphs.len());

        stats
    }

    /// Build sounding instances (numeric text) from SOUNDG multipoint features.
    fn build_soundings(
        &self,
        chart: &ChartData,
        _info: &ChartInfo,
        tile_id: TileId,
        bounds: &TileBounds,
        packet: &mut TilePacket,
        ref_mx: f64,
        ref_my: f64,
    ) -> usize {
        let default_settings = crate::s52::MarinerSettings::default();
        let settings = self.s52_engine.map(|e| &e.settings).unwrap_or(&default_settings);

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
            if !should_render_at_scale(feature, tile_view_scale) {
                if skipped_scale == 0 {
                    // Log first skipped sounding's SCAMIN for debugging
                    log::debug!("  first skipped sounding SCAMIN={:?} (tile_scale=1:{})",
                        feature.scamin(), tile_view_scale as u64);
                }
                skipped_scale += 1;
                continue;
            }

            // NOTE: We intentionally skip S-52 display category filtering for soundings.
            // SOUNDG is in the "Other" category (show_other=false by default), but soundings
            // have their own dedicated `show_soundings` setting that we've already checked above.
            // This matches OpenCPN behavior where soundings are controlled separately.

            let Some(ref mp) = feature.multipoint_geometry else { continue };

            for point in &mp.points {
                // SM meters → global Mercator (add reference point offset)
                let x = ref_mx + point[0] as f64;
                let y = ref_my + point[1] as f64;
                let depth = point[2] as f64;

                if x < bounds.min_x || x > bounds.max_x || y < bounds.min_y || y > bounds.max_y {
                    skipped_bounds += 1;
                    continue;
                }

                if depth.abs() < 0.01 || depth.is_nan() {
                    log::warn!("sounding edge case: depth={} at ({:.1},{:.1})", depth, x, y);
                }
                let render_info = sndfrm02(depth, feature, settings);
                packet.text_instances.push(SoundingInstance {
                    position: [x as f32, y as f32],
                    depth: render_info.whole_part as f32,
                    flags: render_info.to_flags(),
                });
                added += 1;
            }
        }

        if sounding_count > 0 {
            log::debug!("build_soundings: added={}, skipped_scale={}, skipped_bounds={}",
                added, skipped_scale, skipped_bounds);
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
        let sectr2 = if sector.sectr2 <= sectr1 { sector.sectr2 + 360.0 } else { sector.sectr2 };
        let sweep = sectr2 - sectr1;
        if sweep < 1.0 || sweep == 360.0 {
            return;
        }

        let to_chart = |angle: f64| if angle > 180.0 { angle - 180.0 } else { angle + 180.0 };
        let s1 = to_chart(sectr1);
        let s2 = {
            let raw = to_chart(sectr2);
            if raw <= s1 { raw + 360.0 } else { raw }
        };

        let mm_to_meters = |mm: f64| mm * (self.view_ppmm as f64) * (self.view_meters_per_pixel as f64);
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
            let key = LineBatchKey::new_with_priority(priority, 0, outline_key, lookup_id);
            packet.line_batches.push(LineBatch { key, vertices, indices });
        }

        // Color arc
        let (vertices, indices) = build_line_vertices_multi_indexed(&[arc_points]);
        if !vertices.is_empty() {
            let key = LineBatchKey::new_with_priority(priority, 1, arc_key, lookup_id);
            packet.line_batches.push(LineBatch { key, vertices, indices });
        }

        // Sector legs
        let leg_lines = light_sector_leg_lines(center, sector_radius_m, s1, s2);
        if !leg_lines.is_empty() {
            let (vertices, indices) = build_line_vertices_multi_indexed(&leg_lines);
            if !vertices.is_empty() {
                let key = LineBatchKey::new_with_priority(priority, 2, legs_key, lookup_id);
                packet.line_batches.push(LineBatch { key, vertices, indices });
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
fn get_text_for_attribute(feature: &Feature, attribute: &str, format: Option<&str>) -> Option<String> {
    use crate::senc::AttributeValue;

    let attr_val = feature.attributes.get(attribute)?;
    match attr_val {
        AttributeValue::String(s) => {
            if s.is_empty() { return None; }
            Some(s.clone())
        }
        AttributeValue::Integer(v) => {
            if let Some(fmt) = format {
                // Simple format handling for common patterns
                Some(format_s52_text(fmt, *v as f64))
            } else {
                Some(v.to_string())
            }
        }
        AttributeValue::Float(v) => {
            if let Some(fmt) = format {
                Some(format_s52_text(fmt, *v))
            } else {
                Some(format!("{:.1}", v))
            }
        }
    }
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
    styled_by_ls: usize,           // Features styled via LS() instruction
    styled_by_cs: usize,           // Features styled via CS() procedure execution
    styled_by_cs_fallback: usize,  // Features needing CS() - using fallback (unknown proc)
    styled_by_lc: usize,           // Features styled via LC() with valid symbol
    styled_by_lc_missing: usize,   // Features with LC() but symbol not found in table
    styled_by_lc_fallback: usize,  // Features needing LC() - using LS fallback style
    lc_stamps_emitted: usize,      // LC pattern stamps generated
    skipped_other: usize,          // Skipped: display_cat == "Other"
    skipped_no_lookup: usize,      // Skipped: no S-52 lookup for object class
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
    instances_added: usize,
}

/// Legacy line style lookup for when S-52 lookup table is unavailable.
/// Uses ObjectClass matching as fallback.
fn legacy_line_style(feature: &Feature, s52_engine: Option<&S52Engine>) -> Option<LineStyleKey> {
    match feature.object_class {
        ObjectClass::Coastline => Some(LineStyleKey::new(LinePattern::Dashed, 1, "CSTLN")),
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
        ObjectClass::TrafficSeparationLine => Some(LineStyleKey::new(LinePattern::Solid, 6, "TRFCF")),
        ObjectClass::Road => Some(LineStyleKey::new(LinePattern::Solid, 2, "LANDF")),
        ObjectClass::RiverBank => Some(LineStyleKey::new(LinePattern::Dotted, 2, "CSTLN")),
        ObjectClass::Pipeline => Some(LineStyleKey::new(LinePattern::Solid, 2, "CHGRD")),
        _ => None, // Unknown class
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn area_vertex_layout_matches_shader() {
        // chart.wgsl expects:
        // @location(0) position: vec2<f32>
        // @location(1) color_index: u32
        assert_eq!(std::mem::size_of::<AreaVertex>(), 12);
        assert_eq!(std::mem::align_of::<AreaVertex>(), 4);
    }

    #[test]
    fn tile_packet_byte_size() {
        let mut packet = TilePacket::new(TileId::new(10, 512, 512));
        packet.area_vertices.push(AreaVertex {
            position: [0.0, 0.0],
            color_index: 0,
        });
        packet.compute_byte_size();
        assert_eq!(packet.byte_size, std::mem::size_of::<AreaVertex>());
    }

    // test_scamin_and_scamax_combined: removed (missing make_feature_with_scamin helper)

    // LC symbol lookup tests

    #[test]
    fn test_line_style_table_loads() {
        let table = get_line_style_table();
        assert!(table.is_some(), "LineStyleTable should load from chartsymbols.xml");
        let table = table.unwrap();
        assert!(table.len() > 0, "LineStyleTable should have symbols");
    }

    #[test]
    fn test_line_style_table_has_lowacc21() {
        // LOWACC21 is used by COALNE features with low accuracy positions
        let table = get_line_style_table().expect("table should load");
        let symbol = table.get("LOWACC21");
        assert!(symbol.is_some(), "LOWACC21 should exist in line style table");
        let symbol = symbol.unwrap();
        assert_eq!(symbol.color_ref, "ACSTLN", "LOWACC21 should use ACSTLN color");
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
        let key1 = LineBatchKey::new_with_priority(2, 0, LineStyleKey::new(LinePattern::Solid, 1, "CHBLK"), 1);
        let key2 = LineBatchKey::new_with_priority(4, 0, LineStyleKey::new(LinePattern::Solid, 1, "CHBLK"), 2);
        assert!(key1 < key2, "disp_prio=2 should sort before disp_prio=4");

        // Same priority, lower pass should sort first
        let key3 = LineBatchKey::new_with_priority(4, 0, LineStyleKey::new(LinePattern::Solid, 1, "CHBLK"), 3);
        let key4 = LineBatchKey::new_with_priority(4, 1, LineStyleKey::new(LinePattern::Solid, 1, "CHBLK"), 4);
        assert!(key3 < key4, "pass=0 should sort before pass=1");
    }
}
