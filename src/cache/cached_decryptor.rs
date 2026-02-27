//! Cached chart decryptor wrapper.
//!
//! Wraps ChartDecryptor with disk caching to avoid re-decrypting charts.

use std::path::Path;

use crate::cache::SencCache;
use crate::decrypt::{ChartDecryptor, DecryptResult};

/// Chart decryptor with disk caching.
///
/// On first access, decrypts via oexserverd and caches to disk.
/// On subsequent accesses, reads from disk cache (fast).
pub struct CachedDecryptor {
    decryptor: ChartDecryptor,
    cache: SencCache,
    cache_enabled: bool,
}

impl CachedDecryptor {
    /// Create a new cached decryptor.
    ///
    /// Falls back to uncached if cache directory creation fails.
    pub fn new(decryptor: ChartDecryptor) -> Self {
        let (cache, cache_enabled) = match SencCache::new() {
            Ok(c) => (c, true),
            Err(e) => {
                eprintln!("Warning: Disk cache unavailable: {}. Running uncached.", e);
                // Create a dummy cache that won't be used
                (SencCache::with_dir(std::env::temp_dir().join("navcore_dummy_cache"))
                    .unwrap_or_else(|_| panic!("Failed to create even temp cache")),
                 false)
            }
        };

        Self {
            decryptor,
            cache,
            cache_enabled,
        }
    }

    /// Create with cache disabled (for testing or when disk access is slow).
    pub fn uncached(decryptor: ChartDecryptor) -> Self {
        Self {
            decryptor,
            cache: SencCache::with_dir(std::env::temp_dir().join("navcore_dummy"))
                .expect("temp dir should exist"),
            cache_enabled: false,
        }
    }

    /// Decrypt a chart, using cache if available.
    ///
    /// Cache key is the chart filename (without extension).
    pub fn decrypt_chart<P: AsRef<Path>>(
        &mut self,
        chart_path: P,
        install_key: &str,
    ) -> DecryptResult<Vec<u8>> {
        let chart_path = chart_path.as_ref();

        // Get chart name for cache key
        let chart_name = chart_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");

        // Try cache first
        if self.cache_enabled {
            if let Ok(cached) = self.cache.read_cached(chart_name) {
                return Ok(cached);
            }
        }

        // Cache miss - decrypt via oexserverd
        let senc_data = self.decryptor.decrypt_chart(chart_path, install_key)?;

        // Write to cache (ignore errors - cache is best-effort)
        if self.cache_enabled {
            if let Err(e) = self.cache.write_cache(chart_name, &senc_data) {
                eprintln!("Warning: Failed to cache {}: {}", chart_name, e);
            }
        }

        Ok(senc_data)
    }

    /// Get cache statistics.
    pub fn cache_stats(&self) -> crate::cache::CacheStats {
        self.cache.stats()
    }

    /// Check if caching is enabled.
    pub fn is_cache_enabled(&self) -> bool {
        self.cache_enabled
    }

    /// Access the underlying decryptor (for restart, etc.)
    pub fn decryptor_mut(&mut self) -> &mut ChartDecryptor {
        &mut self.decryptor
    }

    /// Clear the disk cache.
    pub fn clear_cache(&self) -> std::io::Result<()> {
        self.cache.clear()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // Note: Full integration tests require oexserverd and chart files
    // These are unit tests for the wrapper logic only

    #[test]
    fn cache_key_from_path() {
        let path = PathBuf::from("/charts/DK4KATKS.oesu");
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap();
        assert_eq!(name, "DK4KATKS");
    }
}
