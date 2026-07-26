//! Background tile building worker thread.
//!
//! Offloads CPU-intensive tile geometry generation to a dedicated thread,
//! keeping the main/render thread responsive for smooth navigation.
//! Uses rayon for parallel tile building within each batch.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Instant;

use rayon::prelude::*;

use crate::cache::CachedDecryptor;
use crate::decrypt::KeyStore;
use crate::s52::S52Engine;
use crate::senc::ChartCatalog;
use super::TileId;
use super::builder::{TileBuilder, TilePacket};

/// View parameters needed for tile building (sent with each batch)
#[derive(Debug, Clone, Copy)]
pub struct ViewParams {
    pub meters_per_pixel: f32,
    pub ppmm: f32,
    pub width_px: f32,
    pub height_px: f32,
    pub center_x: f32,
    pub center_y: f32,
}

/// Request from main thread to worker
pub enum TileRequest {
    /// Build these tiles with the given view params
    Build {
        tiles: Vec<TileId>,
        view_params: ViewParams,
        generation: u64,
    },
    /// Identify the chart objects at a position, for the info bubble.
    ///
    /// Answered on this thread because it is where the parsed charts live; the
    /// main thread would otherwise have to re-parse a cell on every click.
    Pick {
        mercator: [f64; 2],
        tolerance_m: f64,
        seq: u64,
    },
    /// Shut down the worker thread
    Shutdown,
}

/// Answer to a [`TileRequest::Pick`].
pub struct PickResponse {
    /// Echoes the request, so a stale answer to an earlier click is discarded
    /// rather than replacing the panel the user is looking at.
    pub seq: u64,
    pub position: [f64; 2],
    pub objects: Vec<crate::pick::PickedObject>,
}

/// Response from worker to main thread
pub struct TileResponse {
    pub tile_id: TileId,
    pub result: Result<TilePacket, String>,
}

/// Handle for communicating with the background tile worker
pub struct TileWorkerHandle {
    request_tx: mpsc::Sender<TileRequest>,
    response_rx: mpsc::Receiver<TileResponse>,
    pick_rx: mpsc::Receiver<PickResponse>,
    pick_seq: AtomicU64,
    /// Bumped on every request. The worker compares it per tile and abandons
    /// the rest of a batch once it is stale, so a zoom does not wait behind a
    /// screenful of tiles for the view the user has already left.
    generation: Arc<AtomicU64>,
    _thread: thread::JoinHandle<()>,
}

impl TileWorkerHandle {
    /// Ask what chart objects are at a position. The answer arrives through
    /// [`poll_pick`](Self::poll_pick); the returned sequence number identifies it.
    pub fn request_pick(&self, mercator: [f64; 2], tolerance_m: f64) -> u64 {
        let seq = self.pick_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.request_tx.send(TileRequest::Pick {
            mercator,
            tolerance_m,
            seq,
        });
        seq
    }

    /// The most recent pick answer, if one has arrived. Earlier answers are
    /// dropped: only the latest click matters.
    pub fn poll_pick(&self) -> Option<PickResponse> {
        let mut latest = None;
        while let Ok(r) = self.pick_rx.try_recv() {
            latest = Some(r);
        }
        latest
    }

    /// Send a batch of tile build requests
    pub fn request_tiles(&self, tiles: Vec<TileId>, view_params: ViewParams) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.request_tx.send(TileRequest::Build {
            tiles,
            view_params,
            generation,
        });
    }

    /// Poll for completed tile packets (non-blocking).
    /// Returns all available results.
    pub fn poll_results(&self) -> Vec<TileResponse> {
        let mut results = Vec::new();
        while let Ok(response) = self.response_rx.try_recv() {
            results.push(response);
        }
        results
    }

    /// Shut down the worker thread
    pub fn shutdown(self) {
        let _ = self.request_tx.send(TileRequest::Shutdown);
        let _ = self._thread.join();
    }
}

/// Spawn the background tile worker thread.
///
/// Ownership of catalog, keys, decryptor, chart_cache and s52_engine
/// is transferred to the worker thread.
pub fn spawn_tile_worker(
    catalog: Arc<ChartCatalog>,
    keys: Arc<KeyStore>,
    decryptor: CachedDecryptor,
    s52_engine: Option<S52Engine>,
) -> TileWorkerHandle {
    let (request_tx, request_rx) = mpsc::channel::<TileRequest>();
    let (response_tx, response_rx) = mpsc::channel::<TileResponse>();
    let (pick_tx, pick_rx) = mpsc::channel::<PickResponse>();
    let generation = Arc::new(AtomicU64::new(0));
    let worker_generation = Arc::clone(&generation);

    let thread = thread::Builder::new()
        .name("tile-worker".to_string())
        .spawn(move || {
            worker_loop(
                catalog,
                keys,
                decryptor,
                s52_engine,
                request_rx,
                response_tx,
                pick_tx,
                worker_generation,
            );
        })
        .expect("Failed to spawn tile worker thread");

    TileWorkerHandle {
        request_tx,
        response_rx,
        pick_rx,
        pick_seq: AtomicU64::new(0),
        generation,
        _thread: thread,
    }
}

/// Main loop for the background tile worker.
/// Uses rayon to build tiles in parallel within each batch.
fn worker_loop(
    catalog: Arc<ChartCatalog>,
    keys: Arc<KeyStore>,
    decryptor: CachedDecryptor,
    s52_engine: Option<S52Engine>,
    request_rx: mpsc::Receiver<TileRequest>,
    response_tx: mpsc::Sender<TileResponse>,
    pick_tx: mpsc::Sender<PickResponse>,
    generation: Arc<AtomicU64>,
) {
    // Wrap mutable resources in Mutex for thread-safe parallel access
    let chart_cache: crate::tiles::builder::ChartCache = Mutex::new(HashMap::new());
    // Shared across the per-tile builders below: a chart's M_COVR tessellation
    // is the same wherever it is used.
    let coverage_cache: crate::tiles::builder::CoverageCache = Mutex::new(HashMap::new());
    let decryptor: Mutex<CachedDecryptor> = Mutex::new(decryptor);

    loop {
        // Block until next request
        let request = match request_rx.recv() {
            Ok(r) => r,
            Err(_) => break, // Channel closed
        };

        match request {
            TileRequest::Build {
                tiles,
                view_params,
                generation: batch_generation,
            } => {
                let profile = std::env::var("NAVCORE_PROFILE")
                    .map(|v| v != "0" && !v.is_empty())
                    .unwrap_or(false);
                let batch_start = profile.then(Instant::now);

                // Build each tile for *its own* zoom, not the camera's.
                //
                // A tile's content depends on the view scale — SCAMIN decides
                // what is in it, and line simplification decides how finely.
                // Deriving that from the camera made a tile's contents depend
                // on where the camera happened to be when it was first built,
                // so a cached tile could carry the filtering of a different
                // zoom until it was evicted, and nothing could be legitimately
                // reused across a zoom change. Keyed on the tile's own z, tile
                // content is a pure function of (tile, settings) and the
                // pyramid works the way a pyramid should.
                // Results go out as each tile finishes rather than when the
                // whole batch does: a screenful is seconds of work, and holding
                // every tile until the slowest one lands is the difference
                // between the map filling in and the map appearing at once
                // after a stall.
                // Load every chart the batch needs, one task per chart, before
                // any tile is built.
                //
                // Loading lazily inside the tile loop deadlocks parallelism:
                // the chart cache serialises the threads that want the same
                // chart, and a blocked rayon worker cannot be stolen from, so
                // eight tiles wanting one chart stall the whole pool — the
                // batch took six times longer in wall clock than when every
                // thread wastefully parsed its own copy. Warming first gets
                // both: each chart parsed once, and parsed in parallel.
                {
                    let warm_start = profile.then(Instant::now);
                    let mut needed: Vec<u64> = Vec::new();
                    let mut seen = std::collections::HashSet::new();
                    for &tile_id in &tiles {
                        let scale = super::meters_per_pixel(tile_id.z)
                            * (view_params.ppmm as f64)
                            * 1000.0;
                        for info in catalog.charts_for_tile_scaled(&tile_id.bounds(), scale) {
                            if seen.insert(info.id) {
                                needed.push(info.id);
                            }
                        }
                    }
                    let warmer = TileBuilder::with_cache(
                        &catalog, &keys, &decryptor, &chart_cache, &coverage_cache,
                    );
                    needed.par_iter().for_each(|&id| {
                        if generation.load(Ordering::SeqCst) != batch_generation {
                            return;
                        }
                        if let Some(info) = catalog.charts.iter().find(|c| c.id == id) {
                            if let Err(e) = warmer.load_chart(info) {
                                eprintln!("Warning: Skipping chart {}: {}", info.name, e);
                            }
                        }
                    });
                    if let Some(start) = warm_start {
                        log::info!(
                            "profile.warm_charts: {} ms charts={}",
                            start.elapsed().as_millis(),
                            needed.len()
                        );
                    }
                }

                let sender = Mutex::new(response_tx.clone());
                tiles
                    .par_iter()
                    .for_each(|&tile_id| {
                        if generation.load(Ordering::SeqCst) != batch_generation {
                            return; // the view moved on; this tile is not wanted
                        }
                        let bounds = tile_id.bounds();
                        let builder = TileBuilder::with_cache(
                            &catalog, &keys, &decryptor, &chart_cache, &coverage_cache,
                        )
                        .with_view_params(
                            super::meters_per_pixel(tile_id.z) as f32,
                            view_params.ppmm,
                            view_params.width_px,
                            view_params.height_px,
                            ((bounds.min_x + bounds.max_x) * 0.5) as f32,
                            ((bounds.min_y + bounds.max_y) * 0.5) as f32,
                        );
                        let builder = if let Some(ref engine) = s52_engine {
                            builder.with_s52_engine(engine)
                        } else {
                            builder
                        };
                        let result = builder.build_cpu(tile_id).map_err(|e| e.to_string());
                        let _ = sender
                            .lock()
                            .unwrap()
                            .send(TileResponse { tile_id, result });
                    });

                if let Some(start) = batch_start {
                    log::info!(
                        "profile.worker_batch: {} ms tiles={} generation={}",
                        start.elapsed().as_millis(),
                        tiles.len(),
                        batch_generation,
                    );
                }


            }
            TileRequest::Pick {
                mercator,
                tolerance_m,
                seq,
            } => {
                // Answered here because this is where the parsed charts live;
                // on the main thread every click would re-parse a cell.
                let builder = TileBuilder::with_cache(
                    &catalog, &keys, &decryptor, &chart_cache, &coverage_cache,
                );
                let mut objects = Vec::new();
                for info in catalog
                    .charts
                    .iter()
                    .filter(|c| c.contains_point(mercator[0], mercator[1]))
                {
                    match builder.load_chart(info) {
                        Ok(chart) => objects.extend(crate::pick::pick_at(
                            &chart, info, mercator, tolerance_m,
                        )),
                        Err(e) => log::warn!("pick: skipping {}: {}", info.name, e),
                    }
                }
                crate::pick::sort_picks(&mut objects);
                let _ = pick_tx.send(PickResponse {
                    seq,
                    position: mercator,
                    objects,
                });
            }
            TileRequest::Shutdown => break,
        }
    }
}
