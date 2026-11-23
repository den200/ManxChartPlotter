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
        Some(Self::new(path, header.cell_name.clone(), header.native_scale, extent, header.ref_lat, header.ref_lon))
    }

    fn new(path: PathBuf, name: String, native_scale: u32, extent: CellExtent, ref_lat: f64, ref_lon: f64) -> Self {
        let id = Self::id_from_path(&path);

        // Convert WGS84 extent to Mercator bounds
        let (min_x, min_y) = latlon_to_mercator(extent.min_lat, extent.min_lon);
        let (max_x, max_y) = latlon_to_mercator(extent.max_lat, extent.max_lon);

        Self {
            id,
            path,
            name,
            native_scale,
            extent_wgs84: extent,
            extent_mercator: TileBounds::new(min_x, max_x, min_y, max_y),
            ref_lat,
            ref_lon,
        }
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

/// The catalogue of a folder of o-charts cells, kept between starts.
///
/// Without it every start decrypts every cell only to read its name, scale
/// and bounding box — sixteen seconds for the Danish set on a laptop, more
/// on a plotter. Decrypted cells themselves are never stored (o-charts'
/// conditions; see [`crate::cache`]), so this holds just those catalogue
/// facts and no chart content: which cells exist and where they lie, as
/// OpenCPN's chart database does. A cell is trusted to the index only
/// while its file has the size and date it had when it was read.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct CellIndex {
    cells: std::collections::BTreeMap<String, IndexedCell>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct IndexedCell {
    /// The cell file's size and modification time (seconds), as read.
    size: u64,
    modified: u64,
    name: String,
    native_scale: u32,
    /// min_lat, max_lat, min_lon, max_lon.
    extent: [f64; 4],
    ref_lat: f64,
    ref_lon: f64,
}

impl CellIndex {
    /// Where the index for a chart folder is kept.
    fn file_for(dir: &Path) -> Option<PathBuf> {
        let mut h = DefaultHasher::new();
        dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()).hash(&mut h);
        Some(crate::paths::cache_dir()?.join("index").join(format!("{:016x}.json", h.finish())))
    }

    fn load(file: &Path) -> Self {
        std::fs::read(file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self, file: &Path) {
        let Ok(json) = serde_json::to_vec(self) else { return };
        if let Some(parent) = file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = file.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, json).and_then(|_| std::fs::rename(&tmp, file)).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// The size and date of a cell file, as the index records them.
    fn stamp(path: &Path) -> Option<(u64, u64)> {
        let meta = std::fs::metadata(path).ok()?;
        let modified = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
        Some((meta.len(), modified.as_secs()))
    }

    /// The catalogue entry for a cell file, if the file is the one indexed.
    fn info(&self, path: &Path) -> Option<ChartInfo> {
        let cell = self.cells.get(path.file_name()?.to_str()?)?;
        let (size, modified) = Self::stamp(path)?;
        if (cell.size, cell.modified) != (size, modified) {
            return None;
        }
        let [min_lat, max_lat, min_lon, max_lon] = cell.extent;
        Some(ChartInfo::new(
            path.to_path_buf(),
            cell.name.clone(),
            cell.native_scale,
            CellExtent { min_lat, max_lat, min_lon, max_lon },
            cell.ref_lat,
            cell.ref_lon,
        ))
    }

    fn remember(&mut self, info: &ChartInfo) {
        let (Some(file), Some((size, modified))) = (
            info.path.file_name().and_then(|n| n.to_str()),
            Self::stamp(&info.path),
        ) else {
            return;
        };
        let e = info.extent_wgs84;
        self.cells.insert(
            file.to_string(),
            IndexedCell {
                size,
                modified,
                name: info.name.clone(),
                native_scale: info.native_scale,
                extent: [e.min_lat, e.max_lat, e.min_lon, e.max_lon],
                ref_lat: info.ref_lat,
                ref_lon: info.ref_lon,
            },
        );
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

    /// Scan a directory for cells and read their headers.
    ///
    /// The first time, every o-charts cell is decrypted for its header; the
    /// catalogue facts are then indexed ([`CellIndex`]) so later starts
    /// decrypt nothing until a cell is drawn.
    pub fn from_directory(
        dir: &Path,
        keys: &KeyStore,
        decryptor: &mut CachedDecryptor,
    ) -> Result<Self, CatalogError> {
        Self::scan(dir, None, keys, decryptor, CellIndex::file_for(dir).as_deref())
    }

    /// The chart folder a user opened, plus the free charts Manx downloaded
    /// for them, which live in a folder of their own
    /// ([`crate::shop::noaa::library`]) whatever chart folder is open. Free
    /// charts an older Manx left inside this folder are moved out first.
    pub fn from_chart_folder(
        dir: &Path,
        keys: &KeyStore,
        decryptor: &mut CachedDecryptor,
    ) -> Result<Self, CatalogError> {
        let library = crate::shop::noaa::library();
        if let Some(ref library) = library {
            crate::shop::noaa::adopt(dir, library);
        }
        let free = library.map(|l| l.join("noaa"));
        Self::scan(dir, free.as_deref(), keys, decryptor, CellIndex::file_for(dir).as_deref())
    }

    fn scan(
        dir: &Path,
        extra_s57: Option<&Path>,
        keys: &KeyStore,
        decryptor: &mut CachedDecryptor,
        index_file: Option<&Path>,
    ) -> Result<Self, CatalogError> {
        let mut index = index_file.map(CellIndex::load).unwrap_or_default();
        let mut fresh = CellIndex::default();
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
        let mut s57_cells = find_s57_cells(dir, 5);
        if let Some(extra) = extra_s57.filter(|e| !e.starts_with(dir)) {
            s57_cells.extend(find_s57_cells(extra, 5));
        }

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

            // Indexed, and the file unchanged: nothing to decrypt.
            if let Some(info) = index.info(&path) {
                fresh.remember(&info);
                combined = Some(match combined {
                    Some(c) => c.union(&info.extent_mercator),
                    None => info.extent_mercator,
                });
                charts.push(info);
                continue;
            }

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

            fresh.remember(&info);
            charts.push(info);
        }

        // Only what is in the folder now is kept: a cell that has gone
        // leaves the index with it.
        index.cells = fresh.cells;
        if let Some(file) = index_file {
            if !index.cells.is_empty() || file.exists() {
                index.save(file);
            }
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
        self.charts_for_tile_where(tile, tile_scale_denom, &|_| true, &|_, _| true)
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
    ///
    /// `covers(chart, rect)` answers whether the chart's real coverage fills
    /// `rect` (a part of its extent). Only a chart that does is allowed to
    /// hide the coarser charts under that part of the tile; one whose
    /// coverage stops short — a 1:90 000 cell whose M_COVR is a staircase
    /// inside its extent — is drawn, and the coarser charts are kept under
    /// it, or the gap between its coverage and its extent is a hole: a
    /// tile-aligned rectangle of bare background cut into the chart.
    pub fn charts_for_tile_where(
        &self,
        tile: &TileBounds,
        tile_scale_denom: f64,
        has_data: &dyn Fn(&ChartInfo) -> bool,
        covers: &dyn Fn(&ChartInfo, &TileBounds) -> bool,
    ) -> Vec<&ChartInfo> {
        // A chart overzoomed out by more than this factor relative to the tile
        // scale is dropped (its detail is wasted and bloats the tile). This was
        // 16, to keep some land and water on screen when zoomed out past the
        // coarsest chart; the world basemap does that now, and at 16 the
        // Danish 1:1 500 000 overview still drew every light and track on it
        // as one black knot over a view of all of Europe.
        //
        // 3, not 4, since the tile scale became a ground scale (it was
        // Mercator, 1.77x too large at Danish latitudes, so the 4 then
        // behaved like 2.3 here). 3 puts the switch between neighbouring
        // bands near their geometric middle — the 1:180 000 cells give way to
        // the 1:1 500 000 overview at about 1:540 000 — so neither side of
        // the switch is badly over- or under-zoomed.
        const MAX_OVERZOOM_OUT: f64 = 3.0;

        let intersecting: Vec<&ChartInfo> =
            self.charts.iter().filter(|c| c.intersects(tile)).collect();

        // Drop charts whose native scale is much finer than this tile needs.
        // A non-finite scale means "no scale filtering" (keep all intersecting).
        //
        // The scale test comes before `has_data`, which reads the cell's
        // coverage and so parses the whole cell: asked first, zoomed out over
        // California it parsed all 443 cells to draw the dozen overview ones,
        // and every parsed cell stays in memory.
        let min_native = if tile_scale_denom.is_finite() {
            tile_scale_denom / MAX_OVERZOOM_OUT
        } else {
            0.0
        };
        let mut candidates: Vec<&ChartInfo> = intersecting
            .iter()
            .copied()
            .filter(|c| (c.native_scale as f64) >= min_native && has_data(c))
            .collect();
        if candidates.is_empty() {
            // All intersecting charts are finer than the view warrants (zoomed
            // out past the coarsest chart). Keep the coarsest with data so it
            // still draws.
            let mut coarsest_first = intersecting.clone();
            coarsest_first.sort_by(|a, b| b.native_scale.cmp(&a.native_scale));
            if let Some(coarsest) = coarsest_first.into_iter().find(|c| has_data(c)) {
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
            let e = &chart.extent_mercator;
            let part = TileBounds::new(
                e.min_x.max(tile.min_x),
                e.max_x.min(tile.max_x),
                e.min_y.max(tile.min_y),
                e.max_y.min(tile.max_y),
            );
            let contributes = if covers(chart, &part) {
                subtract_rect(&mut uncovered, e, eps)
            } else {
                // Drawn, but it hides nothing: whatever it leaves out of its
                // extent still needs a coarser chart beneath.
                uncovered.iter().any(|r| {
                    e.min_x < r.max_x && e.max_x > r.min_x && e.min_y < r.max_y && e.max_y > r.min_y
                })
            };
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

    /// A cell in the index is catalogued without decrypting it — here
    /// there is no helper at all — and only while its file is the one that
    /// was indexed: a changed file is not trusted to old facts. The index
    /// holds the catalogue facts and nothing else.
    #[test]
    fn an_indexed_cell_is_catalogued_without_decrypting_it() {
        let dir = tempfile::tempdir().unwrap();
        let cells = dir.path().join("set");
        std::fs::create_dir_all(&cells).unwrap();
        let cell = cells.join("OC-45-TEST.oesu");
        std::fs::write(&cell, b"encrypted bytes").unwrap();
        let index_file = dir.path().join("index.json");

        let extent = CellExtent { min_lat: 55.0, max_lat: 56.0, min_lon: 12.0, max_lon: 13.0 };
        let mut index = CellIndex::default();
        index.remember(&ChartInfo::new(cell.clone(), "DK4TEST".into(), 22_000, extent, 55.5, 12.5));
        index.save(&index_file);
        let text = std::fs::read_to_string(&index_file).unwrap();
        let entry: serde_json::Value = serde_json::from_str(&text).unwrap();
        let mut fields: Vec<&str> = entry["cells"]["OC-45-TEST.oesu"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        fields.sort();
        assert_eq!(fields, ["extent", "modified", "name", "native_scale", "ref_lat", "ref_lon", "size"]);

        let mut keys = KeyStore::new();
        keys.insert("OC-45-TEST", "KEY");
        let scan = |file: &Path| {
            let mut d = CachedDecryptor::s57_only_in(dir.path().join("cache"));
            ChartCatalog::scan(&cells, None, &keys, &mut d, Some(file))
        };
        let catalog = scan(&index_file).expect("the indexed cell");
        assert_eq!(catalog.charts.len(), 1);
        assert_eq!(catalog.charts[0].name, "DK4TEST");
        assert_eq!(catalog.charts[0].native_scale, 22_000);
        assert_eq!(catalog.charts[0].extent_wgs84.max_lat, 56.0);

        // The file changes (a new edition): the index no longer speaks for it.
        std::fs::write(&cell, b"a different, longer encrypted file").unwrap();
        assert!(matches!(scan(&index_file), Err(CatalogError::EmptyDirectory)));
        // And with no index there is nothing to go on either.
        assert!(matches!(scan(&dir.path().join("none.json")), Err(CatalogError::EmptyDirectory)));
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

    /// An extent is not a coverage. A detailed cell whose extent spans the
    /// tile but whose M_COVR does not must leave the coarse chart in place —
    /// in the Kattegat a 1:90 000 cell did exactly that to the 1:180 000
    /// one, and a tile-sized rectangle of bare background cut into an island.
    #[test]
    fn a_chart_hides_nothing_its_coverage_does_not_reach() {
        let tile = TileBounds::new(0.0, 1000.0, 0.0, 1000.0);
        let mut catalog = ChartCatalog::new();
        catalog.charts.push(chart_at("coarse", 180000, TileBounds::new(-9000.0, 9000.0, -9000.0, 9000.0)));
        catalog.charts.push(chart_at("detailed", 90000, TileBounds::new(-100.0, 1100.0, -100.0, 1100.0)));

        let short = |c: &ChartInfo, _: &TileBounds| c.name != "detailed";
        let selected = catalog.charts_for_tile_where(&tile, f64::INFINITY, &|_| true, &short);
        let names: Vec<&str> = selected.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["coarse", "detailed"]);

        let full = |_: &ChartInfo, _: &TileBounds| true;
        let selected = catalog.charts_for_tile_where(&tile, f64::INFINITY, &|_| true, &full);
        let names: Vec<&str> = selected.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["detailed"]);
    }

    /// Zoomed out, a chart far too detailed for the tile is never asked
    /// whether it has data: the builder answers that by parsing the whole
    /// cell, and a parsed cell stays in memory. Asked before the scale cut,
    /// a view of California parsed all 443 cells to draw the overview ones.
    #[test]
    fn a_chart_too_fine_for_the_tile_is_not_parsed() {
        let tile = TileBounds::new(0.0, 1000.0, 0.0, 1000.0);
        let big = TileBounds::new(-9000.0, 9000.0, -9000.0, 9000.0);
        let mut catalog = ChartCatalog::new();
        catalog.charts.push(chart_at("overview", 1_500_000, big));
        catalog.charts.push(chart_at("coastal", 180_000, big));
        catalog.charts.push(chart_at("harbour", 12_000, big));

        let asked = std::cell::RefCell::new(Vec::new());
        let has_data = |c: &ChartInfo| {
            asked.borrow_mut().push(c.name.clone());
            true
        };
        let selected = catalog.charts_for_tile_where(&tile, 1_000_000.0, &has_data, &|_, _| true);
        let names: Vec<&str> = selected.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["overview"]);
        assert!(!asked.borrow().contains(&"harbour".to_string()), "asked {:?}", asked.borrow());
        assert!(!asked.borrow().contains(&"coastal".to_string()), "asked {:?}", asked.borrow());

        // Zoomed out past every chart, the coarsest one with data still draws.
        asked.borrow_mut().clear();
        let no_overview = |c: &ChartInfo| {
            asked.borrow_mut().push(c.name.clone());
            c.name != "overview"
        };
        let selected =
            catalog.charts_for_tile_where(&tile, 50_000_000.0, &no_overview, &|_, _| true);
        let names: Vec<&str> = selected.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["coastal"]);
        assert!(!asked.borrow().contains(&"harbour".to_string()));
    }

    /// The tile's scale is a ground scale: at 55.7°N a tile is shown at a
    /// scale 1/cos 55.7° = 1.776x larger than the Mercator metres per pixel
    /// alone suggest.
    #[test]
    fn a_tile_scale_is_measured_on_the_ground() {
        use crate::tiles::{latlon_to_mercator, tile_scale_denominator, TileId};
        let (mx, my) = latlon_to_mercator(55.7, 12.6);
        let z = 10u8;
        let ground = tile_scale_denominator(TileId::from_mercator(mx, my, z), 4.0);
        let mercator = crate::tiles::meters_per_pixel(z) * 4.0 * 1000.0;
        let ratio = mercator / ground;
        // A z10 tile here is about 0.2° tall, which moves 1/cos by under 1 %.
        assert!((ratio - 1.776).abs() < 0.015, "Mercator/ground = {ratio}");
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
        assert!((area(&region) - 8000.0).abs() < 1e-9, "{region:?}");

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

    fn area(region: &[TileBounds]) -> f64 {
        region.iter().map(|r| (r.max_x - r.min_x) * (r.max_y - r.min_y)).sum()
    }

    /// What is left after a cut is exactly the rectangle minus the cut — no
    /// strip counted twice, none lost — or a chart would be kept (or dropped)
    /// for ground that is not there.
    #[test]
    fn subtract_rect_keeps_exactly_what_the_cut_leaves() {
        let whole = TileBounds::new(0.0, 100.0, 0.0, 100.0);
        // A corner, an edge bite, a hole in the middle, and a sliver.
        for (cut, left) in [
            (TileBounds::new(50.0, 150.0, 50.0, 150.0), 7500.0),
            (TileBounds::new(-10.0, 30.0, 20.0, 60.0), 8800.0),
            (TileBounds::new(40.0, 60.0, 30.0, 70.0), 9200.0),
        ] {
            let mut region = vec![whole];
            assert!(subtract_rect(&mut region, &cut, 0.01));
            assert!((area(&region) - left).abs() < 1e-9, "{cut:?}: {region:?}");
            // The pieces stay disjoint, so the next cut is measured right.
            for (i, a) in region.iter().enumerate() {
                for b in &region[i + 1..] {
                    let overlap = (a.max_x.min(b.max_x) - a.min_x.max(b.min_x)).max(0.0)
                        * (a.max_y.min(b.max_y) - a.min_y.max(b.min_y)).max(0.0);
                    assert!(overlap < 1e-9, "{a:?} overlaps {b:?}");
                }
            }
        }
        // A strip thinner than eps is dropped as noise.
        let mut region = vec![whole];
        subtract_rect(&mut region, &TileBounds::new(0.005, 200.0, -1.0, 200.0), 0.01);
        assert!(region.is_empty(), "{region:?}");
    }

    /// The world basemap stays under every tile it touches even when a
    /// detailed chart's extent claims the whole tile: an extent is a
    /// rectangle, and where the chart charts nothing the basemap is all
    /// there is to draw.
    #[test]
    fn the_world_basemap_stays_under_a_covering_chart() {
        let tile = TileBounds::new(0.0, 1000.0, 0.0, 1000.0);
        let big = TileBounds::new(-9000.0, 9000.0, -9000.0, 9000.0);
        let mut catalog = ChartCatalog::new();
        let mut world = chart_at("world", crate::s57::basemap::SCALE, big);
        world.path = crate::s57::basemap::path();
        catalog.charts.push(world);
        catalog.charts.push(chart_at("coarse", 180_000, big));
        catalog.charts.push(chart_at("detailed", 12_000, big));
        let names: Vec<&str> = catalog.charts_for_tile(&tile).iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["world", "detailed"], "basemap first, coarse chart hidden");
    }

    /// The cell search skips hidden folders and the ones that never hold
    /// charts, stops at its depth, takes only `.000` base cells (not their
    /// updates), and lists them sorted.
    #[test]
    fn the_cell_search_skips_what_it_should() {
        let dir = tempfile::tempdir().unwrap();
        let put = |rel: &str| {
            let p = dir.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"").unwrap();
        };
        put("ENC_ROOT/B/B.000");
        put("ENC_ROOT/B/B.001");
        put("ENC_ROOT/A/A.000");
        put(".hidden/H.000");
        put("node_modules/N.000");
        put("Library/L.000");
        put("a/b/c/d/DEEP.000");
        let found = find_s57_cells(dir.path(), 2);
        let names: Vec<String> = found
            .iter()
            .map(|p| p.strip_prefix(dir.path()).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["ENC_ROOT/A/A.000", "ENC_ROOT/B/B.000"]);
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
