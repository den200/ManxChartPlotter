//! Disk cache for decrypted SENC files.
//!
//! Caches decrypted chart data to avoid re-decrypting on subsequent runs.
//! Cache location: ~/.cache/navcore/senc/

mod cached_decryptor;

pub use cached_decryptor::CachedDecryptor;

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Disk cache for decrypted SENC data
pub struct SencCache {
    cache_dir: PathBuf,
}

impl SencCache {
    /// Create a new SENC cache with default location (~/.cache/navcore/senc/)
    pub fn new() -> io::Result<Self> {
        let cache_dir = Self::default_cache_dir()?;
        fs::create_dir_all(&cache_dir)?;
        Ok(Self { cache_dir })
    }

    /// Create cache with custom directory (for testing)
    pub fn with_dir(cache_dir: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&cache_dir)?;
        Ok(Self { cache_dir })
    }

    /// Get default cache directory
    fn default_cache_dir() -> io::Result<PathBuf> {
        let home = dirs::home_dir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Home directory not found"))?;
        Ok(home.join(".cache").join("navcore").join("senc"))
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
    use std::fs;

    #[test]
    fn cache_roundtrip() {
        let temp_dir = std::env::temp_dir().join("navcore_cache_test");
        let _ = fs::remove_dir_all(&temp_dir);

        let cache = SencCache::with_dir(temp_dir.clone()).unwrap();

        // Initially not cached
        assert!(!cache.has_cached("test_chart"));

        // Write and verify
        let data = b"test senc data";
        cache.write_cache("test_chart", data).unwrap();
        assert!(cache.has_cached("test_chart"));

        // Read back
        let read_data = cache.read_cached("test_chart").unwrap();
        assert_eq!(read_data, data);

        // Cleanup
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn cache_stats() {
        let temp_dir = std::env::temp_dir().join("navcore_cache_stats_test");
        let _ = fs::remove_dir_all(&temp_dir);

        let cache = SencCache::with_dir(temp_dir.clone()).unwrap();

        // Write some data
        cache.write_cache("chart1", b"data1").unwrap();
        cache.write_cache("chart2", b"data22").unwrap();

        let stats = cache.stats();
        assert_eq!(stats.file_count, 2);
        assert_eq!(stats.total_bytes, 11); // 5 + 6

        // Cleanup
        let _ = fs::remove_dir_all(&temp_dir);
    }
}
