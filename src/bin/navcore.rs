//! NavCore Chart Plotter
//!
//! Usage:
//!   navcore                         - Test triangle (verify GPU)
//!   navcore <chart.oesu>            - Render a chart
//!   navcore <chart_dir/>            - Render all charts from directory
//!   navcore --info <chart.oesu>     - Show chart info without rendering
//!   navcore --catalog <chart_dir/>  - Show catalog info for directory
//!   navcore --tile-debug <chart_dir/> - Build a few tiles headlessly and exit

use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use winit::{
    application::ApplicationHandler,
    dpi::PhysicalPosition,
    event::{ElementState, MouseButton, MouseScrollDelta, Touch, TouchPhase, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Window, WindowId},
};

use navcore2::{CachedDecryptor, ChartDecryptor, KeyStore};
use navcore2::render::RenderState;
use navcore2::s52::S52Engine;
use navcore2::senc::{ChartData, ChartCatalog};
use navcore2::tiles::{TileId, visible_tiles, zoom_from_camera, TileBounds};
use navcore2::tiles::builder::TileBuilder;

/// Chart source for rendering
enum ChartSource {
    /// No charts - render test triangle for GPU verification
    TestTriangle,
    /// Single chart file
    SingleFile(PathBuf),
    /// Directory of charts - tile-based multi-chart rendering
    Directory(PathBuf),
}

fn main() {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    )
    .filter_module("wgpu_core", log::LevelFilter::Warn)
    .filter_module("wgpu_hal", log::LevelFilter::Warn)
    .init();

    let args: Vec<String> = env::args().collect();

    // Handle --info mode
    if args.len() >= 3 && args[1] == "--info" {
        info_mode(&args[2]);
        return;
    }

    // Handle --catalog mode (scan directory, show catalog)
    if args.len() >= 3 && args[1] == "--catalog" {
        catalog_mode(&args[2]);
        return;
    }

    // Headless tile debug mode (no WGPU, no window)
    if args.len() >= 3 && args[1] == "--tile-debug" {
        tile_debug_mode(&args[2]);
        return;
    }

    // Scan charts for object class + attribute usage (Phase 2 of performance plan)
    if args.len() >= 3 && args[1] == "--scan" {
        scan_charts_mode(&args[2]);
        return;
    }

    // Determine chart source from args
    let source = match args.get(1) {
        Some(p) => {
            let path = PathBuf::from(p);
            if path.is_dir() {
                ChartSource::Directory(path)
            } else {
                ChartSource::SingleFile(path)
            }
        }
        None => ChartSource::TestTriangle,
    };

    let event_loop = EventLoop::new().expect("Failed to create event loop");
    // Control flow is set dynamically in about_to_wait()

    let mut app = App::new(source);
    event_loop.run_app(&mut app).expect("Event loop failed");
}

/// Print chart info and exit
fn info_mode(chart_path: &str) {
    println!("Loading chart: {}", chart_path);

    // Find keys
    let chart_dir = PathBuf::from(chart_path)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let mut keys = KeyStore::new();
    if let Err(e) = keys.load_keylists_in_dir(&chart_dir) {
        eprintln!("Warning: Could not load keys from {}: {}", chart_dir.display(), e);
    }

    // Get chart name for key lookup
    let chart_name = PathBuf::from(chart_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_default();

    let install_key = match keys.lookup(&chart_name) {
        Some(k) => k.to_string(),
        None => {
            eprintln!("No install key found for {}", chart_name);
            return;
        }
    };

    // Decrypt (with disk cache)
    let base_decryptor = match ChartDecryptor::new("license") {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Failed to create decryptor: {}", e);
            return;
        }
    };
    let mut decryptor = CachedDecryptor::new(base_decryptor);

    let senc_bytes = match decryptor.decrypt_chart(chart_path, &install_key) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Failed to decrypt chart: {}", e);
            return;
        }
    };

    // Parse
    let chart = match ChartData::parse(senc_bytes) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to parse SENC: {}", e);
            return;
        }
    };

    println!("\n{}", chart.summary());
}

/// Scan chart directory and show catalog info
fn catalog_mode(dir_path: &str) {
    use std::time::Instant;

    let dir = PathBuf::from(dir_path);
    if !dir.is_dir() {
        eprintln!("Error: {} is not a directory", dir_path);
        return;
    }

    println!("Scanning chart directory: {}", dir_path);
    let start = Instant::now();

    // Load keys from directory
    let mut keys = KeyStore::new();
    if let Err(e) = keys.load_keylists_in_dir(&dir) {
        eprintln!("Warning: Could not load keys: {}", e);
    }
    println!("Loaded {} chart keys", keys.len());

    // Create decryptor with disk cache
    let base_decryptor = match ChartDecryptor::new("license") {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Failed to create decryptor: {}", e);
            return;
        }
    };
    let mut decryptor = CachedDecryptor::new(base_decryptor);

    // Build catalog (header-only scan)
    let catalog = match ChartCatalog::from_directory(&dir, &keys, &mut decryptor) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to build catalog: {}", e);
            return;
        }
    };

    let elapsed = start.elapsed();
    println!("\n{}", catalog.summary());
    println!("Catalog built in {:.2?}", elapsed);

    // Show scale distribution
    let mut scale_counts: HashMap<u32, usize> = HashMap::new();
    for chart in &catalog.charts {
        // Group by scale magnitude (1:10K, 1:25K, 1:50K, etc.)
        let scale_group = if chart.native_scale < 15000 {
            10000
        } else if chart.native_scale < 35000 {
            25000
        } else if chart.native_scale < 75000 {
            50000
        } else if chart.native_scale < 150000 {
            100000
        } else if chart.native_scale < 350000 {
            200000
        } else {
            500000
        };
        *scale_counts.entry(scale_group).or_insert(0) += 1;
    }

    println!("\nScale distribution:");
    let mut scales: Vec<_> = scale_counts.iter().collect();
    scales.sort_by_key(|(s, _)| *s);
    for (scale, count) in scales {
        println!("  1:{:>6}: {} charts", scale, count);
    }

    // Show first few charts
    println!("\nFirst 5 charts:");
    for chart in catalog.charts.iter().take(5) {
        println!("  {} (1:{}) at ({:.4}, {:.4})",
            chart.name, chart.native_scale,
            chart.extent_wgs84.center_lat(), chart.extent_wgs84.center_lon());
    }
}

/// Build a few tiles in CPU-only mode and print stats.
///
/// This isolates catalog/intersection/clipping issues from WGPU/windowing.
fn tile_debug_mode(dir_path: &str) {
    let dir = PathBuf::from(dir_path);
    if !dir.is_dir() {
        eprintln!("Error: {} is not a directory", dir_path);
        return;
    }

    println!("Tile debug: scanning chart directory: {}", dir.display());

    let mut keys = KeyStore::new();
    if let Err(e) = keys.load_keylists_in_dir(&dir) {
        eprintln!("Warning: Could not load keys: {}", e);
    }
    println!("Loaded {} chart keys", keys.len());

    let base_decryptor = match ChartDecryptor::new("license") {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Failed to create decryptor: {}", e);
            return;
        }
    };
    let mut decryptor = CachedDecryptor::new(base_decryptor);

    let catalog = match ChartCatalog::from_directory(&dir, &keys, &mut decryptor) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to build catalog: {}", e);
            return;
        }
    };

    println!("{}", catalog.summary());

    // Mimic RenderState::load_catalog camera fit (default window size 1024x768)
    let extent = &catalog.combined_extent;
    let center_x = (extent.min_x + extent.max_x) / 2.0;
    let center_y = (extent.min_y + extent.max_y) / 2.0;
    let width_m = extent.max_x - extent.min_x;
    let height_m = extent.max_y - extent.min_y;

    let viewport_w = 1024.0_f64;
    let viewport_h = 768.0_f64;
    let zoom_x = (width_m * 1.1) / viewport_w;
    let zoom_y = (height_m * 1.1) / viewport_h;
    let zoom_m_per_px = zoom_x.max(zoom_y) as f32;

    let z = zoom_from_camera(zoom_m_per_px).max(9);

    let half_w = (viewport_w * zoom_m_per_px as f64) / 2.0;
    let half_h = (viewport_h * zoom_m_per_px as f64) / 2.0;
    let view_bounds = TileBounds::new(
        center_x - half_w,
        center_x + half_w,
        center_y - half_h,
        center_y + half_h,
    );

    let tiles = visible_tiles(&view_bounds, z, 1.2);
    println!(
        "Tile debug: zoom={:.1} m/px -> z={}, view_bounds=({:.0},{:.0})..({:.0},{:.0}), visible_tiles={}",
        zoom_m_per_px,
        z,
        view_bounds.min_x,
        view_bounds.min_y,
        view_bounds.max_x,
        view_bounds.max_y,
        tiles.len()
    );

    let chart_cache: Mutex<HashMap<u64, Arc<ChartData>>> = Mutex::new(HashMap::new());
    let decryptor = Mutex::new(decryptor);

    // Load S-52 engine for display category filtering
    let s52_engine = match S52Engine::load("assets/s52/chartsymbols.xml") {
        Ok(engine) => {
            println!("{}", engine.summary());
            Some(engine)
        }
        Err(e) => {
            eprintln!("Warning: Could not load S-52 engine: {}", e);
            None
        }
    };

    let builder = TileBuilder::with_cache(&catalog, &keys, &decryptor, &chart_cache);
    let builder = if let Some(ref engine) = s52_engine {
        builder.with_s52_engine(engine)
    } else {
        builder
    };

    // Build tiles around the camera center (matches what RenderState prioritizes).
    let center_tile = TileId::from_mercator(center_x, center_y, z);
    println!("Tile debug: center_tile={:?} for center=({:.0},{:.0})", center_tile, center_x, center_y);

    let n = 1u32 << z;
    let mut candidates: Vec<TileId> = Vec::new();
    for dy in [-1i32, 0, 1] {
        for dx in [-1i32, 0, 1] {
            let x = (center_tile.x as i32 + dx).rem_euclid(n as i32) as u32;
            let y = (center_tile.y as i32 + dy).clamp(0, (n - 1) as i32) as u32;
            candidates.push(TileId { z, x, y });
        }
    }
    // Also include the first few visible tiles as a sanity check.
    candidates.extend(tiles.into_iter().take(4));
    candidates.sort_by_key(|t| (t.y, t.x));
    candidates.dedup();

    for tile_id in candidates {
        let bounds = tile_id.bounds();
        let intersecting = catalog.charts_for_tile(&bounds).len();
        println!("Tile {:?}: {} charts intersect", tile_id, intersecting);

        match builder.build_cpu(tile_id) {
            Ok(packet) => {
                println!(
                    "  packet: {} area verts, {} line batches ({} line verts), {} bytes",
                    packet.area_vertices.len(),
                    packet.line_batches.len(),
                    packet.total_line_vertices(),
                    packet.byte_size
                );
                if let Some(v) = packet.area_vertices.first() {
                    println!("  first area vert: pos=({:.0},{:.0}) color_index={}", v.position[0], v.position[1], v.color_index);
                }
            }
            Err(e) => eprintln!("  build_cpu failed: {}", e),
        }
    }
}

/// Scan all charts in directory and output object class + attribute usage.
/// This helps prioritize which S-57 object classes to implement in the Rust S-52 engine.
fn scan_charts_mode(dir_path: &str) {
    use std::collections::{BTreeMap, BTreeSet};
    use std::time::Instant;

    let dir = PathBuf::from(dir_path);
    if !dir.is_dir() {
        eprintln!("Error: {} is not a directory", dir_path);
        return;
    }

    println!("=== Chart Object Class Scanner ===");
    println!("Scanning: {}\n", dir_path);
    let start = Instant::now();

    // Load keys
    let mut keys = KeyStore::new();
    if let Err(e) = keys.load_keylists_in_dir(&dir) {
        eprintln!("Warning: Could not load keys: {}", e);
    }
    println!("Loaded {} chart keys", keys.len());

    // Create decryptor
    let base_decryptor = match ChartDecryptor::new("license") {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Failed to create decryptor: {}", e);
            return;
        }
    };
    let mut decryptor = CachedDecryptor::new(base_decryptor);

    // Build catalog
    let catalog = match ChartCatalog::from_directory(&dir, &keys, &mut decryptor) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to build catalog: {}", e);
            return;
        }
    };
    println!("Found {} charts\n", catalog.charts.len());

    // Collect object class usage:
    // - object_class -> count
    // - object_class -> set of attribute names
    // - object_class -> geometry type (area/line/point)
    let mut class_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut class_attrs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut class_geom: BTreeMap<String, String> = BTreeMap::new();

    let mut charts_scanned = 0;
    let mut charts_failed = 0;

    for info in &catalog.charts {
        // Try to load full chart data
        let chart_path = &info.path;
        let chart_name = chart_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");

        let install_key = match keys.lookup(chart_name) {
            Some(k) => k.to_string(),
            None => continue,
        };

        let senc_bytes = match decryptor.decrypt_chart(chart_path, &install_key) {
            Ok(b) => b,
            Err(_) => {
                charts_failed += 1;
                continue;
            }
        };

        let chart = match ChartData::parse(senc_bytes) {
            Ok(c) => c,
            Err(_) => {
                charts_failed += 1;
                continue;
            }
        };

        charts_scanned += 1;

        // Scan all features
        for feature in &chart.features {
            let class_name = feature.object_class.acronym().to_string();

            // Count
            *class_counts.entry(class_name.clone()).or_insert(0) += 1;

            // Attributes
            let attrs = class_attrs.entry(class_name.clone()).or_default();
            for (attr_name, _) in &feature.attributes {
                attrs.insert(attr_name.clone());
            }

            // Geometry type
            let geom_type = if feature.area_geometry.is_some() {
                "Area"
            } else if feature.line_geometry.is_some() {
                "Line"
            } else if feature.point_geometry.is_some() {
                "Point"
            } else {
                "None"
            };
            class_geom.entry(class_name).or_insert_with(|| geom_type.to_string());
        }

        // Progress
        if charts_scanned % 50 == 0 {
            print!("\rScanned {} charts...", charts_scanned);
            use std::io::Write;
            std::io::stdout().flush().ok();
        }
    }
    println!("\rScanned {} charts ({} failed)", charts_scanned, charts_failed);

    let elapsed = start.elapsed();
    println!("Scan completed in {:.2?}\n", elapsed);

    // Sort by count (most frequent first)
    let mut sorted: Vec<_> = class_counts.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(a.1));

    // Output results
    println!("=== Object Class Usage (sorted by count) ===\n");
    println!("{:<10} {:>8} {:>6} Attributes", "Class", "Count", "Geom");
    println!("{}", "-".repeat(80));

    for (class, count) in &sorted {
        let geom = class_geom.get(*class).map(|s| s.as_str()).unwrap_or("?");
        let attrs = class_attrs.get(*class)
            .map(|a| {
                let v: Vec<_> = a.iter().take(10).map(|s| s.as_str()).collect();
                let suffix = if a.len() > 10 { "..." } else { "" };
                format!("{}{}", v.join(", "), suffix)
            })
            .unwrap_or_default();

        println!("{:<10} {:>8} {:>6} {}", class, count, geom, attrs);
    }

    println!("\n=== Summary ===");
    println!("Total object classes: {}", class_counts.len());
    println!("Total features: {}", class_counts.values().sum::<usize>());

    // Group by geometry type
    let mut area_classes = 0;
    let mut line_classes = 0;
    let mut point_classes = 0;
    for geom in class_geom.values() {
        match geom.as_str() {
            "Area" => area_classes += 1,
            "Line" => line_classes += 1,
            "Point" => point_classes += 1,
            _ => {}
        }
    }
    println!("Area classes: {}, Line classes: {}, Point classes: {}", area_classes, line_classes, point_classes);
}

struct App {
    source: ChartSource,
    state: Option<RenderState>,
    // For single file mode
    chart_data: Option<ChartData>,
    // For directory mode (Arc for sharing with RenderState)
    keys: Arc<KeyStore>,
    // Mouse state for pan
    last_mouse_pos: PhysicalPosition<f64>,
    mouse_pressed: bool,
    // Touch state for pinch-zoom
    touches: HashMap<u64, PhysicalPosition<f64>>,
    pinch_start_distance: Option<f32>,
}

impl App {
    fn new(source: ChartSource) -> Self {
        Self {
            source,
            state: None,
            chart_data: None,
            keys: Arc::new(KeyStore::new()),
            last_mouse_pos: PhysicalPosition::new(0.0, 0.0),
            mouse_pressed: false,
            touches: HashMap::new(),
            pinch_start_distance: None,
        }
    }

    fn load_charts(&mut self) {
        match &self.source {
            ChartSource::TestTriangle => {
                if let Some(ref mut state) = self.state {
                    println!("No chart path provided - loading test triangle");
                    state.load_test_triangle();
                }
            }
            ChartSource::Directory(dir) => {
                self.load_directory(dir.clone());
            }
            ChartSource::SingleFile(path) => {
                self.load_single_chart(path.clone());
            }
        }
    }

    fn load_directory(&mut self, dir: PathBuf) {
        println!("Loading chart directory: {}", dir.display());

        // Load keys into our Arc (get_mut works since we haven't shared it yet)
        let keys = Arc::get_mut(&mut self.keys)
            .expect("KeyStore Arc should have single reference before sharing");
        if let Err(e) = keys.load_keylists_in_dir(&dir) {
            eprintln!("Warning: Could not load keys: {}", e);
        }
        println!("Loaded {} chart keys", keys.len());

        // Create decryptor with disk cache (will be moved to RenderState)
        let base_decryptor = match ChartDecryptor::new("license") {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Failed to create decryptor: {}", e);
                return;
            }
        };
        let mut decryptor = CachedDecryptor::new(base_decryptor);

        // Build catalog (needs mutable decryptor for header parsing)
        let catalog = match ChartCatalog::from_directory(&dir, &self.keys, &mut decryptor) {
            Ok(c) => {
                println!("{}", c.summary());
                c
            }
            Err(e) => {
                eprintln!("Failed to build catalog: {}", e);
                return;
            }
        };

        // Pass to render state - Arc::clone for keys, move decryptor
        if let Some(ref mut state) = self.state {
            state.load_catalog(catalog, Arc::clone(&self.keys), decryptor);
        }
    }

    fn load_single_chart(&mut self, path: PathBuf) {
        println!("Loading chart: {}", path.display());

        // Find keys directory
        let chart_dir = path.parent().unwrap_or(&path);
        let mut keys = KeyStore::new();

        if let Err(e) = keys.load_keylists_in_dir(chart_dir) {
            eprintln!("Warning: Could not load keys from {}: {}", chart_dir.display(), e);
        }

        // Get chart name for key lookup
        let chart_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_default();

        let install_key = match keys.lookup(&chart_name) {
            Some(k) => k.to_string(),
            None => {
                eprintln!("No install key found for {}. Keys loaded: {}", chart_name, keys.len());
                return;
            }
        };

        println!("Found key for {}", chart_name);

        // Create decryptor with disk cache
        let base_decryptor = match ChartDecryptor::new("license") {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Failed to create decryptor: {}", e);
                return;
            }
        };
        let mut decryptor = CachedDecryptor::new(base_decryptor);

        // Decrypt chart (uses disk cache if available)
        let senc_bytes = match decryptor.decrypt_chart(&path, &install_key) {
            Ok(b) => {
                println!("Decrypted {} bytes", b.len());
                b
            }
            Err(e) => {
                eprintln!("Failed to decrypt: {}", e);
                return;
            }
        };

        // Parse SENC
        let chart = match ChartData::parse(senc_bytes) {
            Ok(c) => {
                println!("{}", c.summary());
                c
            }
            Err(e) => {
                eprintln!("Failed to parse SENC: {}", e);
                return;
            }
        };

        self.chart_data = Some(chart);

        // Load into renderer if available
        if let (Some(ref mut state), Some(ref chart)) = (&mut self.state, &self.chart_data) {
            state.load_chart(chart);
        }
    }
}

impl ApplicationHandler for App {
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(ref state) = self.state {
            if state.needs_redraw() {
                // Immediate redraw for user interaction
                state.window().request_redraw();
            } else if !state.pending_tiles_empty() {
                // Fast polling while tiles load — ~60fps for responsive tile appearance
                event_loop.set_control_flow(ControlFlow::WaitUntil(
                    std::time::Instant::now() + std::time::Duration::from_millis(16),
                ));
                state.window().request_redraw();
            } else {
                // Idle — block until next event
                event_loop.set_control_flow(ControlFlow::Wait);
            }
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_none() {
            let title = match &self.source {
                ChartSource::TestTriangle => "NavCore - Test Triangle (pass chart path to render)",
                ChartSource::SingleFile(_) => "NavCore - Chart Viewer",
                ChartSource::Directory(_) => "NavCore - Multi-Chart Viewer",
            };

            let window = Arc::new(
                event_loop
                    .create_window(
                        Window::default_attributes()
                            .with_title(title)
                            .with_inner_size(winit::dpi::LogicalSize::new(1024, 768)),
                    )
                    .expect("Failed to create window"),
            );

            let state = pollster::block_on(RenderState::new(window));
            self.state = Some(state);

            // Load charts after renderer is ready
            self.load_charts();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = &mut self.state else { return };

        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }

            WindowEvent::Resized(physical_size) => {
                state.resize(physical_size);
            }

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                state.set_scale_factor(scale_factor as f32);
            }

            WindowEvent::RedrawRequested => {
                match state.render() {
                    Ok(_) => {}
                    Err(wgpu::SurfaceError::Lost) => state.resize(state.size),
                    Err(wgpu::SurfaceError::OutOfMemory) => event_loop.exit(),
                    Err(e) => eprintln!("Render error: {:?}", e),
                }
            }

            WindowEvent::MouseInput { state: btn_state, button, .. } => {
                if button == MouseButton::Left {
                    self.mouse_pressed = btn_state == ElementState::Pressed;
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                if self.mouse_pressed {
                    let dx = (position.x - self.last_mouse_pos.x) as f32;
                    let dy = (position.y - self.last_mouse_pos.y) as f32;
                    state.camera.pan(dx, dy);
                    state.mark_dirty();
                }
                self.last_mouse_pos = position;
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let scroll = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(pos) => pos.y as f32 / 50.0,
                };
                state.camera.zoom_by_wheel(
                    scroll,
                    self.last_mouse_pos.x as f32,
                    self.last_mouse_pos.y as f32,
                );
                state.mark_dirty();
            }

            // macOS trackpad gestures
            WindowEvent::PinchGesture { delta, .. } => {
                // delta is additive: positive = zoom in
                let factor = 1.0 + delta as f32;
                state.camera.zoom_at(
                    factor,
                    self.last_mouse_pos.x as f32,
                    self.last_mouse_pos.y as f32,
                );
                state.mark_dirty();
            }

            WindowEvent::PanGesture { delta, .. } => {
                state.camera.pan(delta.x, delta.y);
                state.mark_dirty();
            }

            // Raw touch events (Pi touchscreen)
            WindowEvent::Touch(Touch { id, phase, location, .. }) => {
                match phase {
                    TouchPhase::Started => {
                        self.touches.insert(id, location);
                        if self.touches.len() == 2 {
                            // Inline distance calculation to avoid borrow conflicts
                            let positions: Vec<_> = self.touches.values().collect();
                            let dx = positions[0].x - positions[1].x;
                            let dy = positions[0].y - positions[1].y;
                            self.pinch_start_distance = Some(((dx * dx + dy * dy) as f32).sqrt());
                        }
                    }
                    TouchPhase::Moved => {
                        let old_location = self.touches.get(&id).copied();
                        self.touches.insert(id, location);

                        if self.touches.len() == 2 {
                            // Two-finger pinch zoom - inline calculations to avoid borrow conflicts
                            let positions: Vec<_> = self.touches.values().collect();
                            let dx = positions[0].x - positions[1].x;
                            let dy = positions[0].y - positions[1].y;
                            let new_distance = ((dx * dx + dy * dy) as f32).sqrt();
                            let cx = ((positions[0].x + positions[1].x) / 2.0) as f32;
                            let cy = ((positions[0].y + positions[1].y) / 2.0) as f32;

                            if let Some(sd) = self.pinch_start_distance {
                                if sd > 10.0 {
                                    let factor = new_distance / sd;
                                    state.camera.zoom_at(factor, cx, cy);
                                    state.mark_dirty();
                                }
                            }
                            self.pinch_start_distance = Some(new_distance);
                        } else if self.touches.len() == 1 {
                            // Single-finger pan
                            if let Some(old_loc) = old_location {
                                let dx = (location.x - old_loc.x) as f32;
                                let dy = (location.y - old_loc.y) as f32;
                                state.camera.pan(dx, dy);
                                state.mark_dirty();
                            }
                        }
                    }
                    TouchPhase::Ended | TouchPhase::Cancelled => {
                        self.touches.remove(&id);
                        self.pinch_start_distance = None;
                    }
                }
            }

            _ => {}
        }
    }
}
