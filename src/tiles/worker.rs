//! Background tile building worker thread.
//!
//! Offloads CPU-intensive tile geometry generation to a dedicated thread,
//! keeping the main/render thread responsive for smooth navigation.
//! Uses rayon for parallel tile building within each batch.

use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use rayon::prelude::*;

use crate::cache::CachedDecryptor;
use crate::decrypt::KeyStore;
use crate::s52::S52Engine;
use crate::senc::{ChartCatalog, ChartData};
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
    },
    /// Shut down the worker thread
    Shutdown,
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
    _thread: thread::JoinHandle<()>,
}

impl TileWorkerHandle {
    /// Send a batch of tile build requests
    pub fn request_tiles(&self, tiles: Vec<TileId>, view_params: ViewParams) {
        let _ = self.request_tx.send(TileRequest::Build { tiles, view_params });
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

    let thread = thread::Builder::new()
        .name("tile-worker".to_string())
        .spawn(move || {
            worker_loop(catalog, keys, decryptor, s52_engine, request_rx, response_tx);
        })
        .expect("Failed to spawn tile worker thread");

    TileWorkerHandle {
        request_tx,
        response_rx,
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
) {
    // Wrap mutable resources in Mutex for thread-safe parallel access
    let chart_cache: Mutex<HashMap<u64, Arc<ChartData>>> = Mutex::new(HashMap::new());
    let decryptor: Mutex<CachedDecryptor> = Mutex::new(decryptor);

    loop {
        // Block until next request
        let request = match request_rx.recv() {
            Ok(r) => r,
            Err(_) => break, // Channel closed
        };

        match request {
            TileRequest::Build { tiles, view_params } => {
                // Build tiles in parallel using rayon.
                // TileBuilder is Sync because chart_cache and decryptor are Mutex-wrapped.
                let builder = TileBuilder::with_cache(
                    &catalog,
                    &keys,
                    &decryptor,
                    &chart_cache,
                );
                let builder = builder.with_view_params(
                    view_params.meters_per_pixel,
                    view_params.ppmm,
                    view_params.width_px,
                    view_params.height_px,
                    view_params.center_x,
                    view_params.center_y,
                );
                let builder = if let Some(ref engine) = s52_engine {
                    builder.with_s52_engine(engine)
                } else {
                    builder
                };

                // Build tiles in parallel and send results back
                let results: Vec<_> = tiles.par_iter().map(|&tile_id| {
                    let result = builder.build_cpu(tile_id).map_err(|e| e.to_string());
                    TileResponse { tile_id, result }
                }).collect();

                for response in results {
                    if response_tx.send(response).is_err() {
                        return;
                    }
                }
            }
            TileRequest::Shutdown => break,
        }
    }
}
