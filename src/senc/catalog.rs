//! Chart catalog for multi-chart loading.
//!
//! Stores lightweight metadata (no geometry) for all charts in a directory.
//! Enables fast spatial queries to find charts intersecting a tile.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::cache::CachedDecryptor;
use crate::decrypt::{DecryptError, KeyStore};
use crate::senc::reader::{SencError, SencReader};
use crate::senc::records::{CellExtent, SencHeader};
use crate::tiles::{latlon_to_mercator, TileBounds};

/// Error type for catalog operations
#[derive(Debug)]
pub enum CatalogError {
    Io(std::io::Error),
    Decrypt(DecryptError),
    Parse(SencError),
    NoKey(String),
    EmptyDirectory,
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CatalogError::Io(e) => write!(f, "IO error: {}", e),
            CatalogError::Decrypt(e) => write!(f, "Decrypt error: {}", e),
            CatalogError::Parse(e) => write!(f, "Parse error: {:?}", e),
            CatalogError::NoKey(name) => write!(f, "No key found for chart: {}", name),
            CatalogError::EmptyDirectory => write!(f, "No charts found in directory"),
        }
    }
}

impl std::error::Error for CatalogError {}

impl From<std::io::Error> for CatalogError {
    fn from(e: std::io::Error) -> Self {
        CatalogError::Io(e)
    }
}

impl From<DecryptError> for CatalogError {
    fn from(e: DecryptError) -> Self {
        CatalogError::Decrypt(e)
    }
}

impl From<SencError> for CatalogError {
    fn from(e: SencError) -> Self {
        CatalogError::Parse(e)
    }
}

/// Lightweight chart metadata (no geometry)
#[derive(Debug, Clone)]
pub struct ChartInfo {
    /// Stable unique ID (path hash, not name)
    pub id: u64,
    /// Full path to the .oesu file
    pub path: PathBuf,
    /// Chart cell name (e.g., "DK4KATKS")
    pub name: String,
    /// Native scale (1:N - larger N = less detail)
    pub native_scale: u32,
    /// Extent in WGS84 (for debug/UI)
    pub extent_wgs84: CellExtent,
    /// Extent in Mercator (for fast intersection)
    pub extent_mercator: TileBounds,
    /// Reference latitude for SM→Mercator transform
    pub ref_lat: f64,
    /// Reference longitude for SM→Mercator transform
    pub ref_lon: f64,
}

impl ChartInfo {
    /// Generate stable unique ID from canonical path
    pub fn id_from_path(path: &Path) -> u64 {
        let mut h = DefaultHasher::new();
        // Use canonical path if available, otherwise original path
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        canonical.hash(&mut h);
        h.finish()
    }

    /// Create ChartInfo from header and path
    ///
    /// Returns None if header has no extent (invalid chart)
    pub fn from_header(header: SencHeader, path: PathBuf) -> Option<Self> {
        let extent = header.extent?;
        let id = Self::id_from_path(&path);

        // Convert WGS84 extent to Mercator bounds
        let (min_x, min_y) = latlon_to_mercator(extent.min_lat, extent.min_lon);
        let (max_x, max_y) = latlon_to_mercator(extent.max_lat, extent.max_lon);

        Some(Self {
            id,
            path,
            name: header.cell_name.clone(),
            native_scale: header.native_scale,
            extent_wgs84: extent,
            extent_mercator: TileBounds::new(min_x, max_x, min_y, max_y),
            ref_lat: header.ref_lat,
            ref_lon: header.ref_lon,
        })
    }

    /// Check if this chart intersects tile bounds
    pub fn intersects(&self, tile: &TileBounds) -> bool {
        self.extent_mercator.intersects(tile)
    }

    /// Check whether this chart extent contains a Mercator point.
    pub fn contains_point(&self, x: f64, y: f64) -> bool {
        x >= self.extent_mercator.min_x
            && x <= self.extent_mercator.max_x
            && y >= self.extent_mercator.min_y
            && y <= self.extent_mercator.max_y
    }
}

/// Collection of chart metadata with spatial query support
pub struct ChartCatalog {
    /// All charts in the catalog
    pub charts: Vec<ChartInfo>,
    /// Combined extent of all charts
    pub combined_extent: TileBounds,
}

impl ChartCatalog {
    /// Create empty catalog
    pub fn new() -> Self {
        Self {
            charts: Vec::new(),
            combined_extent: TileBounds::default(),
        }
    }

    /// Scan directory for .oesu files and load headers only
    ///
    /// Uses disk caching to speed up subsequent runs - first run decrypts all charts,
    /// subsequent runs read from ~/.cache/navcore/senc/
    pub fn from_directory(
        dir: &Path,
        keys: &KeyStore,
        decryptor: &mut CachedDecryptor,
    ) -> Result<Self, CatalogError> {
        let mut charts = Vec::new();
        let mut combined = None::<TileBounds>;

        // Find all .oesu files in directory
        let entries: Vec<_> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|ext| ext.eq_ignore_ascii_case("oesu"))
                    .unwrap_or(false)
            })
            .collect();

        // Unencrypted S-57 cells, wherever they sit below the folder: NOAA
        // and most hydrographic offices ship `ENC_ROOT/<cell>/<cell>.000`.
        let s57_cells = find_s57_cells(dir, 5);

        if entries.is_empty() && s57_cells.is_empty() {
            return Err(CatalogError::EmptyDirectory);
        }

        // Converting a cell is the slow step the first time (it is cached
        // after), and each cell is independent: do them side by side.
        let s57_infos: Vec<ChartInfo> = {
            use rayon::prelude::*;
            let shared: &CachedDecryptor = decryptor;
            s57_cells
                .par_iter()
                .filter_map(|path| {
                    let bytes = match shared.s57_senc(path) {
                        Ok(b) => b,
                        Err(e) => {
                            log::warn!("S-57 {}: {e}", path.display());
                            return None;
                        }
                    };
                    let header = SencReader::parse_header_only(&bytes)
                        .map_err(|e| log::warn!("S-57 {}: {e:?}", path.display()))
                        .ok()?;
                    ChartInfo::from_header(header, path.clone())
                })
                .collect()
        };
        for info in s57_infos {
            combined = Some(match combined {
                Some(c) => c.union(&info.extent_mercator),
                None => info.extent_mercator,
            });
            charts.push(info);
        }

        for entry in entries {
            let path = entry.path();

            // Get chart name for key lookup
            let chart_name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
                .unwrap_or_default();

            // Look up install key
            let install_key = match keys.lookup(&chart_name) {
                Some(k) => k.to_string(),
                None => {
                    log::warn!("No key for {}, skipping", chart_name);
                    continue;
                }
            };

            // Decrypt chart
            let senc_bytes = match decryptor.decrypt_chart(&path, &install_key) {
                Ok(b) => b,
                Err(e) => {
                    log::warn!("Failed to decrypt {}: {}", chart_name, e);
                    continue;
                }
            };

            // Parse header only (fast)
            let header = match SencReader::parse_header_only(&senc_bytes) {
                Ok(h) => h,
                Err(e) => {
                    log::warn!("Failed to parse header for {}: {:?}", chart_name, e);
                    continue;
                }
            };

            let info = match ChartInfo::from_header(header, path) {
                Some(i) => i,
                None => {
                    log::warn!("Chart {} has no extent, skipping", chart_name);
                    continue;
                }
            };

            // Update combined extent
            combined = Some(match combined {
                Some(c) => c.union(&info.extent_mercator),
                None => info.extent_mercator,
            });

            charts.push(info);
        }

        if charts.is_empty() {
            return Err(CatalogError::EmptyDirectory);
        }

        // Sort by scale (larger scale value = less detail = render first as background)
        charts.sort_by(|a, b| b.native_scale.cmp(&a.native_scale));

        let combined_extent = combined.unwrap_or_default();

        if log::log_enabled!(log::Level::Debug) {
            log::debug!("=== CATALOG DEBUG ===");
            log::debug!("  Charts loaded: {}", charts.len());
            log::debug!(
                "  Combined Mercator extent: ({:.0},{:.0})-({:.0},{:.0})",
                combined_extent.min_x,
                combined_extent.min_y,
                combined_extent.max_x,
                combined_extent.max_y
            );
            for chart in charts.iter().take(5) {
                log::debug!("  Chart '{}' (1:{}):", chart.name, chart.native_scale);
                log::debug!(
                    "    WGS84: ({:.4},{:.4})-({:.4},{:.4})",
                    chart.extent_wgs84.min_lat,
                    chart.extent_wgs84.min_lon,
                    chart.extent_wgs84.max_lat,
                    chart.extent_wgs84.max_lon
                );
                log::debug!(
                    "    Mercator: ({:.0},{:.0})-({:.0},{:.0}) ref=({:.4},{:.4})",
                    chart.extent_mercator.min_x,
                    chart.extent_mercator.min_y,
                    chart.extent_mercator.max_x,
                    chart.extent_mercator.max_y,
                    chart.ref_lat,
                    chart.ref_lon
                );
            }
            if charts.len() > 5 {
                log::debug!("  ... {} more charts omitted", charts.len() - 5);
            }
        }

        Ok(Self {
            charts,
            combined_extent,
        })
    }

    /// Put the world basemap under the charts, so the whole world can be
    /// panned across and there is land to steer by between chart sets. The
    /// catalogue then spans the world.
    pub fn add_world_basemap(&mut self) {
        if self.charts.iter().any(|c| crate::s57::basemap::is_basemap(&c.path)) {
            return;
        }
        let Some(info) = crate::s57::basemap::chart_info() else { return };
        self.combined_extent = if self.charts.is_empty() {
            info.extent_mercator
        } else {
            self.combined_extent.union(&info.extent_mercator)
        };
        // Coarsest first, as the quilt expects.
        self.charts.insert(0, info);
    }

    /// Whether a chart is an unencrypted S-57 cell rather than an o-charts one.
    pub fn is_s57(path: &Path) -> bool {
        path.extension().is_some_and(|e| e == "000")
    }

    /// Get charts relevant to a tile, sorted by scale (small scale first = background).
    ///
    /// This performs a lightweight quilt selection pass instead of returning every
    /// intersecting chart bbox. More-detailed charts are selected first; lower-scale
    /// charts are only retained if they still cover part of the tile that no
    /// more-detailed chart does.
    pub fn charts_for_tile(&self, tile: &TileBounds) -> Vec<&ChartInfo> {
        self.charts_for_tile_scaled(tile, f64::INFINITY)
    }

    /// Scale-aware variant of [`charts_for_tile`](Self::charts_for_tile).
    ///
    /// `tile_scale_denom` is the tile's nominal display scale (the N in 1:N).
    /// Charts far more detailed than that scale warrants are excluded, so a
    /// zoomed-out tile does not aggregate the full ungeneralised detail of
    /// every overlapping large-scale chart — that produced multi-megabyte,
    /// slow-to-build tiles that never finished, leaving the view blank/white at
    /// low zoom. Mirrors OpenCPN's scale-based quilt selection. If every
    /// intersecting chart is too detailed (e.g. zoomed out past the coarsest
    /// available chart) the coarsest is kept so something still renders.
    pub fn charts_for_tile_scaled(
        &self,
        tile: &TileBounds,
        tile_scale_denom: f64,
    ) -> Vec<&ChartInfo> {
        self.charts_for_tile_where(tile, tile_scale_denom, &|_| true)
    }

    /// [`charts_for_tile_scaled`](Self::charts_for_tile_scaled), leaving
    /// out charts for which `has_data` says no.
    ///
    /// Selection works on extent rectangles, and a rectangle is a poor
    /// stand-in for a cell's coverage: a NOAA cell charting one inlet has
    /// an extent reaching over miles of the coast beside it, and there it
    /// shut out every coarser chart while drawing nothing itself — blank
    /// blocks the size of a tile. The tile builder knows each cell's real
    /// coverage and passes it in here.
    pub fn charts_for_tile_where(
        &self,
        tile: &TileBounds,
        tile_scale_denom: f64,
        has_data: &dyn Fn(&ChartInfo) -> bool,
    ) -> Vec<&ChartInfo> {
        // A chart overzoomed out by more than this factor relative to the tile
        // scale is dropped (its detail is wasted and bloats the tile). This was
        // 16, to keep some land and water on screen when zoomed out past the
        // coarsest chart; the world basemap does that now, and at 16 the
        // Danish 1:1 500 000 overview still drew every light and track on it
        // as one black knot over a view of all of Europe.
        const MAX_OVERZOOM_OUT: f64 = 4.0;

        let intersecting: Vec<&ChartInfo> =
            self.charts.iter().filter(|c| c.intersects(tile) && has_data(c)).collect();
        if intersecting.len() <= 1 {
            return intersecting;
        }

        // Drop charts whose native scale is much finer than this tile needs.
        // A non-finite scale means "no scale filtering" (keep all intersecting).
        let min_native = if tile_scale_denom.is_finite() {
            tile_scale_denom / MAX_OVERZOOM_OUT
        } else {
            0.0
        };
        let mut candidates: Vec<&ChartInfo> = intersecting
            .iter()
            .copied()
            .filter(|c| (c.native_scale as f64) >= min_native)
            .collect();
        if candidates.is_empty() {
            // All intersecting charts are finer than the view warrants (zoomed
            // out past the coarsest chart). Keep the coarsest so it still draws.
            if let Some(coarsest) = intersecting.iter().copied().max_by_key(|c| c.native_scale) {
                candidates.push(coarsest);
            }
        }
        if candidates.len() <= 1 {
            return candidates;
        }
        // `candidates` retains the catalog's coarsest-first order.
        let intersecting = candidates;

        // What of the tile is still unaccounted for. Chart extents and tiles are
        // both rectangles, so this is exact — the previous 5x5 sample grid was
        // not, and it cost a visible hole: a chart covering 91% of a tile left
        // the remaining 30px strip between two sample columns, so every sample
        // read as covered and the coarser chart underneath was dropped. Nothing
        // then painted that strip and the background colour showed through as a
        // pale bar across the land.
        let mut uncovered = vec![*tile];
        let mut selected = Vec::new();
        // Half a pixel of a 256-px tile: below this nothing can show through.
        let eps = (tile.max_x - tile.min_x).abs() / 512.0;

        // Iterate most-detailed to least-detailed for quilt selection.
        for chart in intersecting.iter().rev() {
            let contributes = subtract_rect(&mut uncovered, &chart.extent_mercator, eps);
            if contributes || selected.is_empty() {
                selected.push(*chart);
            }
            if uncovered.is_empty() {
                break;
            }
        }

        // The world basemap stays under every tile it touches. A chart's
        // extent is a rectangle, and "covered" above only means inside that
        // rectangle: a NOAA cell that charts a strip of coast still claims
        // the land and sea around it, and with the basemap dropped there
        // the screen was blank in rectangular blocks. The basemap is a
        // handful of polygons, drawn first, so the charts still win.
        if let Some(world) = intersecting
            .iter()
            .copied()
            .find(|c| crate::s57::basemap::is_basemap(&c.path))
        {
            if !selected.iter().any(|c| std::ptr::eq(*c, world)) {
                selected.push(world);
            }
        }

        // Restore background->detail order expected by the renderer.
        selected.sort_by(|a, b| b.native_scale.cmp(&a.native_scale));
        selected
    }

    /// Number of charts in catalog
    pub fn len(&self) -> usize {
        self.charts.len()
    }


    /// Check if catalog is empty
    pub fn is_empty(&self) -> bool {
        self.charts.is_empty()
    }

    /// Summary for debugging
    pub fn summary(&self) -> String {
        let min_scale = self.charts.iter().map(|c| c.native_scale).min().unwrap_or(0);
        let max_scale = self.charts.iter().map(|c| c.native_scale).max().unwrap_or(0);

        format!(
            "ChartCatalog: {} charts, scales 1:{} to 1:{}\n\
             Combined extent: ({:.4}, {:.4}) to ({:.4}, {:.4}) [Mercator meters]",
            self.charts.len(),
            min_scale,
            max_scale,
            self.combined_extent.min_x,
            self.combined_extent.min_y,
            self.combined_extent.max_x,
            self.combined_extent.max_y,
        )
    }
}

impl Default for ChartCatalog {
    fn default() -> Self {
        Self::new()
    }
}

/// Remove `cut` from a set of disjoint rectangles, returning whether anything
/// was removed.
///
/// Each overlapped rectangle is replaced by the up-to-four bands left around
/// the cut. Chart extents and tiles are both axis-aligned rectangles, so this
/// answers "does this chart cover ground the finer ones do not" exactly, which
/// is what quilt selection needs — approximating it by sampling leaves holes
/// exactly at chart boundaries, where the sliver is usually thinner than the
/// sample spacing.
///
/// `eps` discards slivers below half a screen pixel, so floating-point noise in
/// the Mercator conversion does not drag an extra chart into every tile.
fn subtract_rect(region: &mut Vec<TileBounds>, cut: &TileBounds, eps: f64) -> bool {
    let mut out = Vec::with_capacity(region.len());
    let mut changed = false;
    for r in region.iter() {
        // No overlap: the rectangle survives whole.
        if cut.max_x <= r.min_x || cut.min_x >= r.max_x || cut.max_y <= r.min_y || cut.min_y >= r.max_y
        {
            out.push(*r);
            continue;
        }
        changed = true;
        let (lx, hx) = (cut.min_x.max(r.min_x), cut.max_x.min(r.max_x));
        if r.min_x < lx {
            out.push(TileBounds::new(r.min_x, lx, r.min_y, r.max_y));
        }
        if hx < r.max_x {
            out.push(TileBounds::new(hx, r.max_x, r.min_y, r.max_y));
        }
        if r.min_y < cut.min_y {
            out.push(TileBounds::new(lx, hx, r.min_y, cut.min_y));
        }
        if cut.max_y < r.max_y {
            out.push(TileBounds::new(lx, hx, cut.max_y, r.max_y));
        }
    }
    out.retain(|r| r.max_x - r.min_x > eps && r.max_y - r.min_y > eps);
    *region = out;
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bounded search stops, says so, and still returns what it found —
    /// so pointing the folder picker at a home folder cannot stall the UI.
    #[test]
    fn a_bounded_cell_search_stops_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..50 {
            let cell = dir.path().join(format!("ENC_ROOT/US5XX{i:03}/US5XX{i:03}.000"));
            std::fs::create_dir_all(cell.parent().unwrap()).unwrap();
            std::fs::write(&cell, b"").unwrap();
        }
        let (all, capped) = find_s57_cells_within(dir.path(), 5, 10_000);
        assert_eq!((all.len(), capped), (50, false));
        let (some, capped) = find_s57_cells_within(dir.path(), 5, 20);
        assert!(capped);
        assert!(some.len() < 50);
    }

    #[test]
    fn chart_info_id_is_stable() {
        let path1 = PathBuf::from("/some/path/chart.oesu");
        let path2 = PathBuf::from("/some/path/chart.oesu");
        let path3 = PathBuf::from("/other/path/chart.oesu");

        let id1 = ChartInfo::id_from_path(&path1);
        let id2 = ChartInfo::id_from_path(&path2);
        let id3 = ChartInfo::id_from_path(&path3);

        assert_eq!(id1, id2, "Same path should have same ID");
        assert_ne!(id1, id3, "Different paths should have different IDs");
    }

    fn chart_at(name: &str, scale: u32, extent: TileBounds) -> ChartInfo {
        ChartInfo {
            id: ChartInfo::id_from_path(Path::new(name)),
            path: PathBuf::from(name),
            name: name.to_string(),
            native_scale: scale,
            extent_wgs84: CellExtent {
                min_lat: 55.0,
                max_lat: 56.0,
                min_lon: 10.0,
                max_lon: 11.0,
            },
            extent_mercator: extent,
            ref_lat: 55.5,
            ref_lon: 10.5,
        }
    }

    /// A detailed chart that covers most of a tile must not displace the
    /// coarse one underneath it, or the strip it does not reach paints nothing.
    ///
    /// Regression: the harbour cell at Brøndby covers 91% of its tile. The old
    /// 5x5 sample grid put its outermost column at 90%, so every sample read as
    /// covered, the 1:22000 chart was dropped, and a 30px bar of background
    /// colour ran up the map through solid land.
    #[test]
    fn coarse_chart_kept_where_detailed_one_falls_short() {
        let tile = TileBounds::new(0.0, 1000.0, 0.0, 1000.0);
        let mut catalog = ChartCatalog::new();
        catalog.charts.push(chart_at(
            "coarse",
            22000,
            TileBounds::new(-9000.0, 9000.0, -9000.0, 9000.0),
        ));
        catalog.charts.push(chart_at(
            "detailed",
            4000,
            TileBounds::new(-100.0, 910.0, -100.0, 1100.0),
        ));

        let selected = catalog.charts_for_tile(&tile);
        let names: Vec<&str> = selected.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["coarse", "detailed"], "background first");

        // And when the detailed chart does cover the tile, the coarse one goes.
        catalog.charts[1].extent_mercator = TileBounds::new(-100.0, 1100.0, -100.0, 1100.0);
        let selected = catalog.charts_for_tile(&tile);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "detailed");
    }

    #[test]
    fn subtract_rect_leaves_the_uncovered_band() {
        let mut region = vec![TileBounds::new(0.0, 100.0, 0.0, 100.0)];
        // A cut across the middle in y leaves a band above and below.
        assert!(subtract_rect(
            &mut region,
            &TileBounds::new(-10.0, 110.0, 40.0, 60.0),
            0.01
        ));
        assert_eq!(region.len(), 2);
        assert!(region.iter().any(|r| r.max_y <= 40.0));
        assert!(region.iter().any(|r| r.min_y >= 60.0));

        // A cut that misses changes nothing and reports so.
        let before = region.clone();
        assert!(!subtract_rect(
            &mut region,
            &TileBounds::new(500.0, 600.0, 500.0, 600.0),
            0.01
        ));
        assert_eq!(region.len(), before.len());

        // Covering the rest empties it.
        assert!(subtract_rect(
            &mut region,
            &TileBounds::new(-10.0, 110.0, -10.0, 110.0),
            0.01
        ));
        assert!(region.is_empty());
    }

    #[test]
    fn chart_intersects_tile() {
        // Create a chart info with known bounds
        let info = ChartInfo {
            id: 1,
            path: PathBuf::from("test.oesu"),
            name: "TEST".to_string(),
            native_scale: 50000,
            extent_wgs84: CellExtent {
                min_lat: 55.0,
                max_lat: 56.0,
                min_lon: 10.0,
                max_lon: 11.0,
            },
            extent_mercator: TileBounds::new(1000.0, 2000.0, 1000.0, 2000.0),
            ref_lat: 55.5,
            ref_lon: 10.5,
        };

        // Overlapping tile
        let overlap = TileBounds::new(1500.0, 2500.0, 1500.0, 2500.0);
        assert!(info.intersects(&overlap));

        // Non-overlapping tile
        let distant = TileBounds::new(5000.0, 6000.0, 5000.0, 6000.0);
        assert!(!info.intersects(&distant));
    }

    #[test]
    fn charts_for_tile_prefers_more_detailed_coverage() {
        let tile = TileBounds::new(0.0, 100.0, 0.0, 100.0);
        let mut catalog = ChartCatalog::new();
        catalog.charts = vec![
            ChartInfo {
                id: 1,
                path: PathBuf::from("background.oesu"),
                name: "BG".to_string(),
                native_scale: 90000,
                extent_wgs84: CellExtent {
                    min_lat: 0.0,
                    max_lat: 1.0,
                    min_lon: 0.0,
                    max_lon: 1.0,
                },
                extent_mercator: tile,
                ref_lat: 0.5,
                ref_lon: 0.5,
            },
            ChartInfo {
                id: 2,
                path: PathBuf::from("detail.oesu"),
                name: "DETAIL".to_string(),
                native_scale: 4000,
                extent_wgs84: CellExtent {
                    min_lat: 0.0,
                    max_lat: 1.0,
                    min_lon: 0.0,
                    max_lon: 1.0,
                },
                extent_mercator: tile,
                ref_lat: 0.5,
                ref_lon: 0.5,
            },
        ];

        let selected = catalog.charts_for_tile(&tile);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "DETAIL");
    }
}

/// Every `*.000` below `dir`, to `depth` levels, hidden folders skipped.
/// Sorted, so a catalog lists the same cells in the same order every run.
pub fn find_s57_cells(dir: &Path, depth: usize) -> Vec<PathBuf> {
    // Generous: an ENC_ROOT of every NOAA state is some 6 000 folders.
    find_s57_cells_within(dir, depth, 200_000).0
}

/// [`find_s57_cells`] visiting at most `budget` directory entries. The
/// second value is true when the budget ran out before the search did.
///
/// Pointed at a home folder, an unbounded walk goes through all of
/// `Library` and every project on the disk; on the UI thread that froze the
/// Charts window. Symbolic links are not followed (a link back up the tree
/// would never end), and folders that never hold charts are skipped.
pub fn find_s57_cells_within(dir: &Path, depth: usize, budget: usize) -> (Vec<PathBuf>, bool) {
    let mut out = Vec::new();
    let mut left = budget;
    walk(dir, depth, &mut left, &mut out);
    out.sort();
    (out, left == 0)
}

fn walk(dir: &Path, depth: usize, left: &mut usize, out: &mut Vec<PathBuf>) {
    const SKIP: &[&str] = &["Library", "node_modules", "target", "Applications", "System"];
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.filter_map(Result::ok) {
        if *left == 0 {
            return;
        }
        *left -= 1;
        let p = e.path();
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else { continue };
        if name.starts_with('.') {
            continue;
        }
        let Ok(kind) = e.file_type() else { continue };
        if kind.is_dir() {
            if depth > 0 && !SKIP.contains(&name) {
                walk(&p, depth - 1, left, out);
            }
        } else if kind.is_file() && p.extension().is_some_and(|x| x == "000") {
            out.push(p);
        }
    }
}
