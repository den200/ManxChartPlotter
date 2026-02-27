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

        if entries.is_empty() {
            return Err(CatalogError::EmptyDirectory);
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
                    eprintln!("Warning: No key for {}, skipping", chart_name);
                    continue;
                }
            };

            // Decrypt chart
            let senc_bytes = match decryptor.decrypt_chart(&path, &install_key) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("Warning: Failed to decrypt {}: {}", chart_name, e);
                    continue;
                }
            };

            // Parse header only (fast)
            let header = match SencReader::parse_header_only(&senc_bytes) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("Warning: Failed to parse header for {}: {:?}", chart_name, e);
                    continue;
                }
            };

            let info = match ChartInfo::from_header(header, path) {
                Some(i) => i,
                None => {
                    eprintln!("Warning: Chart {} has no extent, skipping", chart_name);
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

    /// Get charts intersecting a tile, sorted by scale (small scale first = background)
    ///
    /// Returns references to ChartInfo for charts that overlap the tile bounds.
    pub fn charts_for_tile(&self, tile: &TileBounds) -> Vec<&ChartInfo> {
        self.charts
            .iter()
            .filter(|c| c.intersects(tile))
            .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
