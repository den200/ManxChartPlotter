//! Disk cache for free S-57 cells converted to SENC.
//!
//! Only free charts (NOAA ENCs and the like) are cached, under names that
//! start with [`FREE_PREFIX`]. o-charts cells are never written to disk
//! decrypted: the chart providers' licences forbid storing unencrypted
//! data on any medium, and nothing decrypted may remain when the program
//! ends (o-charts' conditions, October 2026). They are decrypted into
//! memory on every start, as OpenCPN does.
//!
//! Versions up to 0.5.1 did cache decrypted o-charts cells here. Opening
//! the cache removes whatever they left ([`SencCache::purge_licensed`]).
//! Cache location: ~/.cache/manx/senc/

mod cached_decryptor;

pub use cached_decryptor::CachedDecryptor;

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// What every cached file's name starts with: a converted free S-57 cell.
pub const FREE_PREFIX: &str = "s57_";

/// Disk cache for converted free charts.
pub struct SencCache {
    cache_dir: PathBuf,
}

impl SencCache {
    /// Create a new SENC cache with default location (~/.cache/manx/senc/)
    pub fn new() -> io::Result<Self> {
        let cache_dir = Self::default_cache_dir()?;
        fs::create_dir_all(&cache_dir)?;
        let cache = Self { cache_dir };
        let removed = cache.purge_licensed();
        if removed > 0 {
            log::warn!(
                "Removed {removed} decrypted o-charts file(s) an earlier version had cached in {}",
                cache.cache_dir.display()
            );
        }
        Ok(cache)
    }

    /// Delete everything in the cache that is not a converted free chart:
    /// decrypted o-charts cells cached by versions up to 0.5.1, and any
    /// half-written file beside them. Returns how many files went.
    pub fn purge_licensed(&self) -> usize {
        let Ok(entries) = fs::read_dir(&self.cache_dir) else { return 0 };
        let mut removed = 0;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let cached = name.ends_with(".senc") || name.contains(".senc.tmp");
            if cached && !name.starts_with(FREE_PREFIX) && fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
        removed
    }

    /// Create cache with custom directory (for testing)
    pub fn with_dir(cache_dir: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&cache_dir)?;
        Ok(Self { cache_dir })
    }

    /// Get default cache directory
    fn default_cache_dir() -> io::Result<PathBuf> {
        let dir = crate::paths::cache_dir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Home directory not found"))?;
        Ok(dir.join("senc"))
    }

    /// Get cache file path for a chart
    fn cache_path(&self, chart_name: &str) -> PathBuf {
        // Use chart name as filename with .senc extension
        self.cache_dir.join(format!("{}.senc", chart_name))
    }

    /// Check if a chart is cached
    pub fn has_cached(&self, chart_name: &str) -> bool {
        self.cache_path(chart_name).exists()
    }

    /// Read cached SENC data
    pub fn read_cached(&self, chart_name: &str) -> io::Result<Vec<u8>> {
        let path = self.cache_path(chart_name);
        let mut file = fs::File::open(&path)?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        Ok(data)
    }

    /// Write SENC data to cache
    pub fn write_cache(&self, chart_name: &str, data: &[u8]) -> io::Result<()> {
        // Written aside and renamed into place: several cells are converted
        // in parallel, and a reader must never see half a file.
        let path = self.cache_path(chart_name);
        let tmp = path.with_extension(format!("senc.tmp{}", std::process::id()));
        let mut file = fs::File::create(&tmp)?;
        file.write_all(data)?;
        drop(file);
        fs::rename(&tmp, &path)
    }

    /// Clear all cached files
    pub fn clear(&self) -> io::Result<()> {
        for entry in fs::read_dir(&self.cache_dir)? {
            let entry = entry?;
            if entry.path().extension().map_or(false, |e| e == "senc") {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }

    /// Get cache statistics
    pub fn stats(&self) -> CacheStats {
        let mut stats = CacheStats::default();

        if let Ok(entries) = fs::read_dir(&self.cache_dir) {
            for entry in entries.flatten() {
                if entry.path().extension().map_or(false, |e| e == "senc") {
                    stats.file_count += 1;
                    if let Ok(meta) = entry.metadata() {
                        stats.total_bytes += meta.len() as usize;
                    }
                }
            }
        }

        stats
    }

    /// Cache directory path
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }
}

impl Default for SencCache {
    fn default() -> Self {
        Self::new().expect("Failed to create cache directory")
    }
}

/// Cache statistics
#[derive(Debug, Default)]
pub struct CacheStats {
    pub file_count: usize,
    pub total_bytes: usize,
}

impl CacheStats {
    /// Total size in MB
    pub fn total_mb(&self) -> f64 {
        self.total_bytes as f64 / (1024.0 * 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each test has its own temporary folder: cargo runs tests in parallel,
    // and several test processes can run at once, so a fixed folder name
    // shared between them lets one test's clean-up delete another's files.

    #[test]
    fn cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SencCache::with_dir(dir.path().join("senc")).unwrap();

        // Initially not cached
        assert!(!cache.has_cached("test_chart"));
        assert!(cache.read_cached("test_chart").is_err());

        // Write and verify
        let data = b"test senc data";
        cache.write_cache("test_chart", data).unwrap();
        assert!(cache.has_cached("test_chart"));

        // Read back
        let read_data = cache.read_cached("test_chart").unwrap();
        assert_eq!(read_data, data);

        // Writing again replaces the file rather than appending to it.
        cache.write_cache("test_chart", b"v2").unwrap();
        assert_eq!(cache.read_cached("test_chart").unwrap(), b"v2");
    }

    #[test]
    fn cache_stats() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SencCache::with_dir(dir.path().to_path_buf()).unwrap();

        // Write some data
        cache.write_cache("chart1", b"data1").unwrap();
        cache.write_cache("chart2", b"data22").unwrap();

        let stats = cache.stats();
        assert_eq!(stats.file_count, 2);
        assert_eq!(stats.total_bytes, 11); // 5 + 6
    }

    /// What versions up to 0.5.1 cached is removed: decrypted o-charts
    /// cells, named after the cell, and half-written ones. Converted free
    /// charts and anything that is not a cache file stay.
    #[test]
    fn decrypted_licensed_cells_left_by_older_versions_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SencCache::with_dir(dir.path().to_path_buf()).unwrap();
        for name in ["OC-45-AAVIN5.senc", "OC-45-B.senc.tmp4711", "s57_US4MA1BD_00ff.senc", "notes.txt"] {
            fs::write(dir.path().join(name), b"x").unwrap();
        }
        assert_eq!(cache.purge_licensed(), 2);
        let mut left: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, vec!["notes.txt", "s57_US4MA1BD_00ff.senc"]);
        assert_eq!(cache.purge_licensed(), 0);
    }

    /// A write goes to a side file and is renamed into place, so no side
    /// file is left behind; and clearing the cache removes only cached
    /// SENCs, never anything else that happens to share the folder.
    #[test]
    fn writes_leave_no_side_files_and_clear_removes_only_sencs() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SencCache::with_dir(dir.path().to_path_buf()).unwrap();
        cache.write_cache("chart1", b"data1").unwrap();
        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["chart1.senc"]);

        fs::write(dir.path().join("notes.txt"), b"keep me").unwrap();
        cache.clear().unwrap();
        assert!(!cache.has_cached("chart1"));
        assert!(dir.path().join("notes.txt").exists());
        assert_eq!(cache.stats().file_count, 0);
    }
}
