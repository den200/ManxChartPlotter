//! GPU tile cache with LRU eviction.
//!
//! Caches tile geometry on GPU with byte-budget eviction for RPi4.

use std::collections::{HashMap, HashSet, VecDeque};
use wgpu::util::DeviceExt;

use super::builder::TilePacket;
use super::{LineBatchKey, TileId};
use crate::render::text::SoundingInstance;

/// Complete cache key (includes style_hash for invalidation on palette change)
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub struct TileCacheKey {
    pub tile_id: TileId,
    pub style_hash: u64,
}

impl TileCacheKey {
    pub fn new(tile_id: TileId, style_hash: u64) -> Self {
        Self {
            tile_id,
            style_hash,
        }
    }
}

/// GPU buffers for a single line style batch
pub struct LineBatchGpu {
    /// Batch key (pass order + S-52 style)
    pub key: LineBatchKey,
    /// Line vertices — or, for an LC() line (`index_buffer` None), its
    /// `LcSegment` instances.
    pub vertex_buffer: wgpu::Buffer,
    /// Strip indices; None for an LC() line, which is drawn instanced.
    pub index_buffer: Option<wgpu::Buffer>,
    pub index_count: u32,
    /// Vertices, or LC() segment instances.
    pub vertex_count: u32,
}

/// GPU buffers for a single tile
pub struct TileGpuBuffers {
    /// Vertex buffer for area triangles
    pub area_buffer: wgpu::Buffer,
    /// Number of area vertices
    pub area_vertex_count: u32,
    /// Area vertex offsets per priority level (index i = start of priority i, index 10 = total)
    pub area_priority_offsets: [u32; 11],
    /// Line batches by style (multi-class rendering)
    pub line_batches: Vec<LineBatchGpu>,
    /// Symbol instance buffer (for point features)
    pub symbol_buffer: Option<wgpu::Buffer>,
    /// Number of symbol instances
    pub symbol_count: u32,
    /// Symbol instance offsets per priority level
    pub symbol_priority_offsets: [u32; 11],
    /// Text instances (numeric soundings) stored on CPU for global decluttering
    pub text_instances: Vec<SoundingInstance>,
    /// Label candidates (TextParams) stored on CPU for global layout and decluttering
    pub label_candidates: Vec<crate::render::TextParams>,
    /// Pattern-filled area vertex buffer (AP instruction)
    pub pattern_buffer: Option<wgpu::Buffer>,
    /// Number of pattern vertices
    pub pattern_vertex_count: u32,
    /// Pattern vertex offsets per priority level
    pub pattern_priority_offsets: [u32; 11],
    /// Background area buffer (drawn before the foreground)
    pub bg_area_buffer: Option<wgpu::Buffer>,
    /// Number of background area vertices
    pub bg_area_vertex_count: u32,
    /// Background area vertex offsets per priority level
    pub bg_area_priority_offsets: [u32; 11],
    /// Background pattern buffer (drawn before the foreground)
    pub bg_pattern_buffer: Option<wgpu::Buffer>,
    /// Number of background pattern vertices
    pub bg_pattern_vertex_count: u32,
    /// Background pattern vertex offsets per priority level
    pub bg_pattern_priority_offsets: [u32; 11],
    /// Light sector arcs and legs (instances)
    pub sector_buffer: Option<wgpu::Buffer>,
    /// Sector instance offsets per priority level
    pub sector_priority_offsets: [u32; 11],
    /// Byte size for cache budgeting
    pub byte_size: usize,
}

impl TileGpuBuffers {
    /// Check if tile has any renderable content
    pub fn is_empty(&self) -> bool {
        self.area_vertex_count == 0
            && self.line_batches.is_empty()
            && self.symbol_count == 0
            && self.text_instances.is_empty()
            && self.label_candidates.is_empty()
            && self.pattern_vertex_count == 0
            && self.sector_buffer.is_none()
    }

    /// Total line index count across all batches
    pub fn total_line_indices(&self) -> u32 {
        self.line_batches.iter().map(|b| b.index_count).sum::<u32>()
    }
}

/// LRU cache for GPU tile buffers with byte budget
/// How many known-empty tiles to remember before starting over.
const MAX_EMPTY_TILES: usize = 100_000;

/// How long a tile that failed is left before it is asked for again.
pub const RETRY_FAILED_AFTER: std::time::Duration = std::time::Duration::from_secs(10);

pub struct TileGpuCache {
    tiles: HashMap<TileCacheKey, TileGpuBuffers>,
    lru_order: VecDeque<TileCacheKey>,
    total_bytes: usize,
    max_bytes: usize,
    revision: u64,
    /// Tiles known to contain no renderable geometry (ocean-only).
    /// Prevents re-requesting them every frame.
    empty_tiles: HashSet<TileCacheKey>,
    /// Tiles that could not be built or kept, and when. See
    /// [`mark_failed`](Self::mark_failed).
    failed: HashMap<TileCacheKey, std::time::Instant>,
}

impl TileGpuCache {
    /// Create a new cache with the given byte budget
    pub fn new(max_bytes: usize) -> Self {
        Self {
            tiles: HashMap::new(),
            lru_order: VecDeque::new(),
            total_bytes: 0,
            max_bytes,
            revision: 0,
            empty_tiles: HashSet::new(),
            failed: HashMap::new(),
        }
    }

    /// A tile could not be built, or is too big to keep. It is left alone
    /// for [`RETRY_FAILED_AFTER`] instead of being asked for again on the
    /// very next frame — and the frame after, sixty times a second, each
    /// one a rebuild and a line in the log, for as long as it is on screen.
    pub fn mark_failed(&mut self, key: &TileCacheKey, now: std::time::Instant) {
        self.failed.insert(*key, now);
    }

    /// Whether the tile failed too recently to be worth asking for again.
    pub fn failed_recently(&self, key: &TileCacheKey, now: std::time::Instant) -> bool {
        self.failed
            .get(key)
            .is_some_and(|at| now.saturating_duration_since(*at) < RETRY_FAILED_AFTER)
    }

    /// Default cache for RPi4 (~200MB)
    pub fn default_rpi4() -> Self {
        Self::new(200 * 1024 * 1024)
    }

    /// Default cache for desktop/MacBook (~512MB)
    pub fn default_desktop() -> Self {
        Self::new(512 * 1024 * 1024)
    }

    /// Auto-detect platform and return appropriate cache size
    #[cfg(target_os = "macos")]
    pub fn default_auto() -> Self {
        Self::default_desktop()
    }

    #[cfg(not(target_os = "macos"))]
    pub fn default_auto() -> Self {
        Self::default_rpi4()
    }

    /// Check if a tile is cached
    pub fn contains(&self, key: &TileCacheKey) -> bool {
        self.tiles.contains_key(key)
    }

    /// Check if a tile is known to be empty (ocean-only, no geometry)
    pub fn is_known_empty(&self, key: &TileCacheKey) -> bool {
        self.empty_tiles.contains(key)
    }

    /// Mark a tile as known-empty without building it (e.g. no charts intersect)
    pub fn mark_empty(&mut self, key: &TileCacheKey) {
        // A plotter left panning for days would otherwise remember every
        // empty tile it ever saw. Forgetting them all costs one re-check of
        // each, and only when the set has grown past any plausible screenful.
        if self.empty_tiles.len() >= MAX_EMPTY_TILES {
            self.empty_tiles.clear();
        }
        if self.empty_tiles.insert(*key) {
            self.revision = self.revision.wrapping_add(1);
        }
    }

    /// Get cached tile buffers
    pub fn get(&self, key: &TileCacheKey) -> Option<&TileGpuBuffers> {
        self.tiles.get(key)
    }

    /// Upload a tile packet to GPU and cache it
    pub fn upload(&mut self, device: &wgpu::Device, packet: TilePacket, style_hash: u64) {
        let profile = std::env::var("MANX_PROFILE")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false);
        let upload_start = profile.then(std::time::Instant::now);
        let key = TileCacheKey::new(packet.tile_id, style_hash);

        if log::log_enabled!(log::Level::Debug) {
            log::debug!(
                "cache.upload {:?}: area_verts={} line_batches={} bytes={}",
                packet.tile_id,
                packet.area_vertices.len(),
                packet.line_batches.len(),
                packet.byte_size
            );
        }

        // Don't re-upload if already cached
        if self.tiles.contains_key(&key) {
            if log::log_enabled!(log::Level::Debug) {
                log::debug!("  -> already cached, skipping");
            }
            return;
        }

        // Track empty tiles so we don't re-request them every frame.
        if packet.is_empty() {
            log::debug!("  -> empty packet, marking as known-empty");
            self.mark_empty(&key);
            return;
        }

        // If a single tile exceeds the cache budget, don't cache it at all.
        // Otherwise it would immediately evict everything and still be evicted.
        if packet.byte_size > self.max_bytes {
            log::warn!(
                "Warning: Tile {:?} is {} bytes (> cache budget {}), skipping upload",
                packet.tile_id, packet.byte_size, self.max_bytes
            );
            self.mark_failed(&key, std::time::Instant::now());
            return;
        }

        // Create GPU buffers for areas
        // Buffer labels are only useful for GPU debuggers (RenderDoc/Xcode);
        // skip the format!() allocations in release builds.
        let area_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: None,
            contents: bytemuck::cast_slice(&packet.area_vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });

        // Create GPU buffers for each line batch
        let mut line_batches_gpu = Vec::with_capacity(packet.line_batches.len());
        for batch in packet.line_batches {
            if batch.vertices.is_empty() {
                continue;
            }

            let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None,
                contents: bytemuck::cast_slice(&batch.vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });

            let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None,
                contents: bytemuck::cast_slice(&batch.indices),
                usage: wgpu::BufferUsages::INDEX,
            });

            line_batches_gpu.push(LineBatchGpu {
                key: batch.key.clone(),
                vertex_buffer,
                index_buffer: Some(index_buffer),
                index_count: batch.indices.len() as u32,
                vertex_count: batch.vertices.len() as u32,
            });
        }
        for batch in packet.lc_batches {
            if batch.segments.is_empty() {
                continue;
            }
            let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None,
                contents: bytemuck::cast_slice(&batch.segments),
                usage: wgpu::BufferUsages::VERTEX,
            });
            line_batches_gpu.push(LineBatchGpu {
                key: batch.key,
                vertex_buffer,
                index_buffer: None,
                index_count: 0,
                vertex_count: batch.segments.len() as u32,
            });
        }

        let symbol_instances = packet.symbol_instances;

        // Create GPU buffer for symbol instances
        let (symbol_buffer, symbol_count) = if !symbol_instances.is_empty() {
            let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None,
                contents: bytemuck::cast_slice(&symbol_instances),
                usage: wgpu::BufferUsages::VERTEX,
            });
            (Some(buffer), symbol_instances.len() as u32)
        } else {
            (None, 0)
        };

        // Create GPU buffer for pattern vertices (AP instruction areas)
        let (pattern_buffer, pattern_vertex_count) = if !packet.pattern_vertices.is_empty() {
            let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None,
                contents: bytemuck::cast_slice(&packet.pattern_vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });
            (Some(buffer), packet.pattern_vertices.len() as u32)
        } else {
            (None, 0)
        };

        // Create GPU buffers for background area vertices (drawn before the foreground)
        let (bg_area_buffer, bg_area_vertex_count) = if !packet.bg_area_vertices.is_empty() {
            let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None,
                contents: bytemuck::cast_slice(&packet.bg_area_vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });
            (Some(buffer), packet.bg_area_vertices.len() as u32)
        } else {
            (None, 0)
        };

        // Create GPU buffers for background pattern vertices (drawn before the foreground)
        let (bg_pattern_buffer, bg_pattern_vertex_count) = if !packet.bg_pattern_vertices.is_empty() {
            let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None,
                contents: bytemuck::cast_slice(&packet.bg_pattern_vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });
            (Some(buffer), packet.bg_pattern_vertices.len() as u32)
        } else {
            (None, 0)
        };

        let sector_buffer = (!packet.sector_instances.is_empty()).then(|| {
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None,
                contents: bytemuck::cast_slice(&packet.sector_instances),
                usage: wgpu::BufferUsages::VERTEX,
            })
        });

        let byte_size = packet.byte_size;

        let buffers = TileGpuBuffers {
            area_buffer,
            area_vertex_count: packet.area_vertices.len() as u32,
            area_priority_offsets: packet.area_priority_offsets,
            line_batches: line_batches_gpu,
            symbol_buffer,
            symbol_count,
            symbol_priority_offsets: packet.symbol_priority_offsets,
            text_instances: packet.text_instances,
            label_candidates: packet.label_candidates,
            pattern_buffer,
            pattern_vertex_count,
            pattern_priority_offsets: packet.pattern_priority_offsets,
            bg_area_buffer,
            bg_area_vertex_count,
            bg_area_priority_offsets: packet.bg_area_priority_offsets,
            bg_pattern_buffer,
            bg_pattern_vertex_count,
            bg_pattern_priority_offsets: packet.bg_pattern_priority_offsets,
            sector_buffer,
            sector_priority_offsets: packet.sector_priority_offsets,
            byte_size,
        };

        // Update cache
        self.total_bytes += byte_size;
        self.tiles.insert(key, buffers);
        self.lru_order.push_back(key);
        self.revision = self.revision.wrapping_add(1);
        if let Some(start) = upload_start {
            log::info!(
                "profile.tile_upload: {} ms tile={:?} line_batches={} bytes={}",
                start.elapsed().as_millis(),
                key.tile_id,
                self.tiles.get(&key).map(|b| b.line_batches.len()).unwrap_or(0),
                byte_size
            );
        }
    }

    /// Evict oldest tiles until under budget
    pub fn evict_to_budget(&mut self) {
        self.evict_to_budget_keeping(&[]);
    }

    /// Evict to the byte budget without ever dropping a tile in `keep`.
    ///
    /// The frame's draw list is built before this runs, and a tile evicted
    /// after it joined that list simply does not draw: a hole in the chart
    /// for a frame, exactly where the user is looking. Pinning the visible
    /// tiles costs nothing — if they alone exceed the budget the cache is
    /// briefly over it, which is the right way to be wrong.
    pub fn evict_to_budget_keeping(&mut self, keep: &[TileCacheKey]) {
        let mut skipped: Vec<TileCacheKey> = Vec::new();
        while self.total_bytes > self.max_bytes && !self.lru_order.is_empty() {
            let Some(key) = self.lru_order.pop_front() else { break };
            if keep.contains(&key) {
                skipped.push(key);
                continue;
            }
            if let Some(buffers) = self.tiles.remove(&key) {
                self.total_bytes -= buffers.byte_size;
                self.revision = self.revision.wrapping_add(1);
            }
        }
        // Pinned tiles go back at the front: they are the newest in use, and
        // the next frame will touch them again anyway.
        for key in skipped.into_iter().rev() {
            self.lru_order.push_front(key);
        }
    }

    /// Clear all cached tiles (e.g., on style change)
    pub fn clear(&mut self) {
        self.tiles.clear();
        self.lru_order.clear();
        self.total_bytes = 0;
        self.empty_tiles.clear();
        self.failed.clear();
        self.revision = self.revision.wrapping_add(1);
    }

    /// Current cache size in bytes
    pub fn size_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Number of cached tiles
    pub fn tile_count(&self) -> usize {
        self.tiles.len()
    }

    /// Monotonic cache revision for incremental render-state invalidation.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Touch a tile (move to end of LRU)
    pub fn touch(&mut self, key: &TileCacheKey) {
        // Remove from current position
        if let Some(pos) = self.lru_order.iter().position(|k| k == key) {
            self.lru_order.remove(pos);
            self.lru_order.push_back(*key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tile that failed is not asked for again at once, is asked for
    /// again after the wait (the trouble may have passed), and is forgotten
    /// with everything else when the cache is cleared.
    #[test]
    fn a_failed_tile_is_left_alone_for_a_while_then_tried_again() {
        let mut cache = TileGpuCache::new(100 * 1024 * 1024);
        let key = TileCacheKey::new(TileId::new(10, 512, 512), 1);
        let other = TileCacheKey::new(TileId::new(10, 512, 513), 1);
        let t0 = std::time::Instant::now();
        assert!(!cache.failed_recently(&key, t0));
        cache.mark_failed(&key, t0);
        assert!(cache.failed_recently(&key, t0));
        assert!(cache.failed_recently(&key, t0 + RETRY_FAILED_AFTER - std::time::Duration::from_millis(1)));
        assert!(!cache.failed_recently(&key, t0 + RETRY_FAILED_AFTER));
        assert!(!cache.failed_recently(&other, t0), "one tile's failure is not another's");
        cache.mark_failed(&key, t0);
        cache.clear();
        assert!(!cache.failed_recently(&key, t0));
    }

    #[test]
    fn cache_key_equality() {
        let key1 = TileCacheKey::new(TileId::new(10, 512, 512), 12345);
        let key2 = TileCacheKey::new(TileId::new(10, 512, 512), 12345);
        let key3 = TileCacheKey::new(TileId::new(10, 512, 512), 67890);

        assert_eq!(key1, key2);
        assert_ne!(key1, key3);
    }

    /// An ocean-only tile is remembered so it is not requested every frame,
    /// but only for the style it was built in: after a palette change it has
    /// to be asked about again. Each new fact bumps the revision the renderer
    /// watches; repeating a known one does not, or every frame would redraw.
    #[test]
    fn empty_tiles_are_remembered_per_style_and_forgotten_on_clear() {
        let mut cache = TileGpuCache::new(100 * 1024 * 1024);
        assert_eq!((cache.tile_count(), cache.size_bytes(), cache.revision()), (0, 0, 0));
        let day = TileCacheKey::new(TileId::new(10, 512, 512), 1);
        let dusk = TileCacheKey::new(TileId::new(10, 512, 512), 2);

        cache.mark_empty(&day);
        assert!(cache.is_known_empty(&day));
        assert!(!cache.is_known_empty(&dusk), "another palette has not been asked");
        assert_eq!(cache.revision(), 1);
        cache.mark_empty(&day);
        assert_eq!(cache.revision(), 1, "nothing new was learnt");

        cache.clear();
        assert!(!cache.is_known_empty(&day));
        assert_eq!(cache.revision(), 2);
    }

    /// The set of empty tiles is bounded: past the limit it starts over
    /// rather than growing for as long as the plotter is left panning.
    #[test]
    fn the_empty_tile_set_starts_over_at_its_limit() {
        let mut cache = TileGpuCache::new(0);
        for i in 0..MAX_EMPTY_TILES as u32 {
            cache.mark_empty(&TileCacheKey::new(TileId::new(18, i, 0), 0));
        }
        assert_eq!(cache.empty_tiles.len(), MAX_EMPTY_TILES);
        let next = TileCacheKey::new(TileId::new(18, 0, 1), 0);
        cache.mark_empty(&next);
        assert_eq!(cache.empty_tiles.len(), 1);
        assert!(cache.is_known_empty(&next));
    }

    /// Touching a tile makes it the most recently used, so eviction (which
    /// pops from the front) takes the others first.
    #[test]
    fn touch_moves_a_tile_to_the_back_of_the_queue() {
        let mut cache = TileGpuCache::new(0);
        let k = |x| TileCacheKey::new(TileId::new(10, x, 0), 0);
        cache.lru_order.extend([k(1), k(2), k(3)]);
        cache.touch(&k(1));
        assert_eq!(cache.lru_order, [k(2), k(3), k(1)]);
        // A tile that is not cached is not added by touching it.
        cache.touch(&k(9));
        assert_eq!(cache.lru_order.len(), 3);
    }
}
