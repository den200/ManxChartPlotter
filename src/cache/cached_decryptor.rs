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
    /// `None` when the o-charts helper could not be started — free S-57
    /// charts still load; only encrypted cells are refused.
    decryptor: Option<ChartDecryptor>,
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
                log::warn!("Disk cache unavailable: {}. Running uncached.", e);
                // Create a dummy cache that won't be used
                (SencCache::with_dir(std::env::temp_dir().join("manx_dummy_cache"))
                    .unwrap_or_else(|_| panic!("Failed to create even temp cache")),
                 false)
            }
        };

        Self {
            decryptor: Some(decryptor),
            cache,
            cache_enabled,
        }
    }

    /// A decryptor for a chart folder: the o-charts helper if it starts,
    /// and S-57 conversion either way. A machine that cannot run the helper
    /// (no licence, no Rosetta on an Apple-silicon Mac) still reads free
    /// ENCs rather than refusing the whole folder.
    pub fn open(fpr_path: &str) -> Self {
        match ChartDecryptor::new(fpr_path) {
            Ok(d) => Self::new(d),
            Err(e) => {
                log::warn!("o-charts decryption unavailable ({e}); S-57 charts only");
                let mut s = Self::uncached_placeholder();
                if let Ok(cache) = SencCache::new() {
                    s.cache = cache;
                    s.cache_enabled = true;
                }
                s
            }
        }
    }

    fn uncached_placeholder() -> Self {
        Self {
            decryptor: None,
            cache: SencCache::with_dir(std::env::temp_dir().join("manx_dummy"))
                .expect("temp dir should exist"),
            cache_enabled: false,
        }
    }

    /// An unencrypted S-57 cell (`.000`, updates beside it) as SENC bytes,
    /// converted once and cached. The cache key covers the size and date of
    /// the base file and every update, so a new update is picked up.
    pub fn s57_senc(&self, path: &Path) -> Result<Vec<u8>, String> {
        use std::hash::{Hash, Hasher};
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("cell");
        let mut h = std::collections::hash_map::DefaultHasher::new();
        crate::s57::ENCODER_VERSION.hash(&mut h);
        for n in 0..=999u32 {
            let p = path.with_extension(format!("{n:03}"));
            let Ok(meta) = std::fs::metadata(&p) else { break };
            meta.len().hash(&mut h);
            if let Ok(t) = meta.modified() {
                t.hash(&mut h);
            }
        }
        let key = format!("s57_{stem}_{:016x}", h.finish());
        if self.cache_enabled {
            if let Ok(cached) = self.cache.read_cached(&key) {
                return Ok(cached);
            }
        }
        let cell = crate::s57::cell::Cell::open(path).map_err(|e| e.to_string())?;
        let bytes = crate::s57::senc_encode::encode(&cell);
        if self.cache_enabled {
            if let Err(e) = self.cache.write_cache(&key, &bytes) {
                log::warn!("Failed to cache {stem}: {e}");
            }
        }
        Ok(bytes)
    }

    /// Create with cache disabled (for testing or when disk access is slow).
    pub fn uncached(decryptor: ChartDecryptor) -> Self {
        Self {
            decryptor: Some(decryptor),
            cache: SencCache::with_dir(std::env::temp_dir().join("manx_dummy"))
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
        let Some(decryptor) = self.decryptor.as_mut() else {
            return Err(crate::decrypt::DecryptError::BinaryNotFound(Vec::new()));
        };
        let senc_data = decryptor.decrypt_chart(chart_path, install_key)?;

        // Write to cache (ignore errors - cache is best-effort)
        if self.cache_enabled {
            if let Err(e) = self.cache.write_cache(chart_name, &senc_data) {
                log::warn!("Failed to cache {}: {}", chart_name, e);
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
    pub fn decryptor_mut(&mut self) -> Option<&mut ChartDecryptor> {
        self.decryptor.as_mut()
    }

    /// Clear the disk cache.
    pub fn clear_cache(&self) -> std::io::Result<()> {
        self.cache.clear()
    }
}

#[cfg(test)]
mod tests {
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
