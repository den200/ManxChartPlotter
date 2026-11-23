//! The chart decryptor, with a disk cache for free charts only.
//!
//! o-charts cells go through `oexserverd` on every start and stay in
//! memory; converted free S-57 cells are cached. See [`crate::cache`].

use std::path::Path;

use crate::cache::SencCache;
use crate::decrypt::{ChartDecryptor, DecryptResult};

/// Chart decryptor: o-charts cells decrypted into memory, free S-57 cells
/// converted once and cached on disk.
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

    /// No o-charts helper, and a cache in `dir`: what a test of S-57
    /// loading needs, without reading or filling the real `~/.cache/manx`
    /// — where an earlier run's cached cell would answer instead of the
    /// converter under test.
    #[cfg(test)]
    pub(crate) fn s57_only_in(dir: std::path::PathBuf) -> Self {
        Self {
            decryptor: None,
            cache: SencCache::with_dir(dir).expect("a test cache folder"),
            cache_enabled: true,
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
        let key = format!("{}{stem}_{:016x}", crate::cache::FREE_PREFIX, h.finish());
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

    /// Decrypt an o-charts cell into memory.
    ///
    /// Never read from or written to disk: the licences forbid storing the
    /// decrypted data, so each start asks `oexserverd` again.
    pub fn decrypt_chart<P: AsRef<Path>>(
        &mut self,
        chart_path: P,
        install_key: &str,
    ) -> DecryptResult<Vec<u8>> {
        let Some(decryptor) = self.decryptor.as_mut() else {
            return Err(crate::decrypt::DecryptError::BinaryNotFound(Vec::new()));
        };
        // o-charts' conditions: the agreement that comes with the chart set
        // is accepted before the charts are used. Every decryption comes
        // through here, the command-line tools included.
        crate::shop::eula::check(chart_path.as_ref())
            .map_err(crate::decrypt::DecryptError::EulaNotAccepted)?;
        decryptor.decrypt_chart(chart_path.as_ref(), install_key)
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
    use super::*;

    // Full integration tests require oexserverd and chart files; these
    // cover the wrapper's own logic with no helper at all.

    fn without_helper(dir: &Path) -> CachedDecryptor {
        CachedDecryptor {
            decryptor: None,
            cache: SencCache::with_dir(dir.to_path_buf()).unwrap(),
            cache_enabled: true,
        }
    }

    /// o-charts' conditions: no unencrypted chart data on any medium. A
    /// decrypted cell sitting in the cache folder (as versions up to 0.5.1
    /// left them) is not served; without the helper the cell is refused.
    #[test]
    fn a_licensed_cell_is_never_served_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = without_helper(dir.path());
        d.cache.write_cache("DK4KATKS", b"senc bytes").unwrap();
        assert!(matches!(
            d.decrypt_chart("/charts/DK4KATKS.oesu", "key"),
            Err(crate::decrypt::DecryptError::BinaryNotFound(_))
        ));
    }

    /// An S-57 cell is converted once and then read from the cache; a new
    /// update file beside it changes the key, so the update is not hidden
    /// behind the stale conversion.
    #[test]
    fn a_new_s57_update_is_not_hidden_by_the_cache() {
        let fixtures = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/s57"));
        let cells = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let d = without_helper(cache_dir.path());
        let cell = cells.path().join("US4MA1BD.000");
        std::fs::copy(fixtures.join("US4MA1BD.000"), &cell).unwrap();

        let base = d.s57_senc(&cell).unwrap();
        assert_eq!(d.cache_stats().file_count, 1);
        std::fs::copy(fixtures.join("US4MA1BD.001"), cells.path().join("US4MA1BD.001")).unwrap();
        let updated = d.s57_senc(&cell).unwrap();
        assert_ne!(base, updated, "the update was not applied");
        assert_eq!(d.cache_stats().file_count, 2);

        // And a repeat is answered from the cache, not converted again.
        for entry in std::fs::read_dir(cache_dir.path()).unwrap() {
            std::fs::write(entry.unwrap().path(), b"from the cache").unwrap();
        }
        assert_eq!(d.s57_senc(&cell).unwrap(), b"from the cache");
    }
}
