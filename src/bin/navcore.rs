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
    event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, Touch, TouchPhase, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{Key, NamedKey},
    window::{Window, WindowId},
};

/// How far the pointer may move between press and release and still count as a
/// click rather than a drag, in physical pixels. A mouse is steady; a hand on a
/// trackpad is not.
const CLICK_SLOP: f64 = 4.0;
/// The same for a finger, which never lifts from quite where it landed.
const TOUCH_SLOP: f64 = 10.0;

use navcore2::{CachedDecryptor, ChartDecryptor, KeyStore};
use navcore2::render::RenderState;
use navcore2::s52::S52Engine;
use navcore2::senc::{ChartData, ChartCatalog, s57_code_to_acronym};
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

/// Run from the directory that holds `assets/`.
///
/// The symbology, fonts, the decryption helper and the licence are all found
/// by paths relative to it. Started by hand from the project that is always
/// so; started by the system at boot (a systemd service on the plotter) the
/// working directory is `/`, and the chart would come up with no S-52
/// symbology at all. So when `assets/` is not here, look beside the
/// executable and up from it — `target/release/navcore` sits two levels
/// below the project — and move there.
fn find_install_dir() {
    let marker = std::path::Path::new("assets/s52/chartsymbols.xml");
    if marker.exists() {
        return;
    }
    let Ok(exe) = env::current_exe().and_then(|p| p.canonicalize()) else {
        return;
    };
    for dir in exe.ancestors().skip(1) {
        if dir.join(marker).exists() {
            if env::set_current_dir(dir).is_ok() {
                log::info!("working directory set to {}", dir.display());
            }
            return;
        }
    }
    log::warn!("assets/ not found beside {}; S-52 symbology will be missing", exe.display());
}

fn main() {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    )
    .filter_module("wgpu_core", log::LevelFilter::Warn)
    .filter_module("wgpu_hal", log::LevelFilter::Warn)
    .init();

    // Paths on the command line mean what they meant where the command was
    // typed; fix them before the working directory can move.
    let args: Vec<String> = env::args()
        .map(|a| match std::path::Path::new(&a).canonicalize() {
            Ok(abs) if !a.starts_with('-') => abs.display().to_string(),
            _ => a,
        })
        .collect();
    find_install_dir();

    // Handle --info mode
    if args.len() >= 3 && args[1] == "--info" {
        info_mode(&args[2]);
        return;
    }

    // Identify the chart objects at a position, the way the info bubble does.
    if args.len() >= 4 && args[1] == "--pick" {
        let parts: Vec<f64> = args[3].split(',').filter_map(|v| v.trim().parse().ok()).collect();
        if parts.len() != 2 {
            eprintln!("--pick: position must be 'lat,lon'");
            return;
        }
        let tol = args.get(4).and_then(|v| v.parse::<f64>().ok()).unwrap_or(50.0);
        pick_mode(&args[2], parts[0], parts[1], tol);
        return;
    }

    // M3 auto-routing: a safe route between two points, from the charts.
    // navcore --auto-route <chart_dir> lat1,lon1 lat2,lon2 [draft_m]
    // `--noaa-install <chart folder> <STATE>`: what the Free charts tab's
    // Download button does, from the command line.
    if args.len() >= 4 && args[1] == "--noaa-install" {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let mut last = 0;
        match navcore2::shop::noaa::install(
            std::path::Path::new(&args[2]),
            &args[3].to_uppercase(),
            |done, total| {
                if done - last >= 1_000_000 || done == total {
                    last = done;
                    eprint!("\r  {} of {} MB", done / 1_000_000, total / 1_000_000);
                }
            },
            &cancel,
        ) {
            Ok(i) => println!("\ninstalled ({} , package {})", i.downloaded, i.last_modified),
            Err(e) => {
                eprintln!("\n{e}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.len() >= 3 && args[1] == "--s57-dump" {
        s57_dump_mode(&args[2]);
        return;
    }

    if args.len() >= 5 && args[1] == "--auto-route" {
        let parse_pos = |s: &str| -> Option<(f64, f64)> {
            let mut it = s.split(',').filter_map(|v| v.trim().parse::<f64>().ok());
            Some((it.next()?, it.next()?))
        };
        let (Some(a), Some(b)) = (parse_pos(&args[3]), parse_pos(&args[4])) else {
            eprintln!("--auto-route: positions must be 'lat,lon'");
            return;
        };
        let draft = args.get(5).and_then(|v| v.parse::<f64>().ok());
        auto_route_mode(&args[2], a, b, draft);
        return;
    }

    // M5 weather routing: forecast + polar + charts.
    // navcore --weather-route <chart_dir> lat1,lon1 lat2,lon2 [hours]
    if args.len() >= 5 && args[1] == "--weather-route" {
        let parse_pos = |s: &str| -> Option<(f64, f64)> {
            let mut it = s.split(',').filter_map(|v| v.trim().parse::<f64>().ok());
            Some((it.next()?, it.next()?))
        };
        let (Some(a), Some(b)) = (parse_pos(&args[3]), parse_pos(&args[4])) else {
            eprintln!("--weather-route: positions must be 'lat,lon'");
            return;
        };
        let hours = args.get(5).and_then(|v| v.parse::<u32>().ok()).unwrap_or(48);
        weather_route_mode(&args[2], a, b, hours);
        return;
    }

    // The o-charts chart shop: what does this account own, and what is stale.
    if args.len() >= 3 && args[1] == "--shop-list" {
        shop_list_mode(&args[2], args.get(3).map(String::as_str));
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

    // Inspect one detailed-chart center tile and report masked/leaking features
    if args.len() >= 4 && args[1] == "--inspect-tile" {
        let zoom = args.get(4).and_then(|z| z.parse::<u8>().ok()).unwrap_or(15);
        inspect_tile_mode(&args[2], &args[3], zoom);
        return;
    }

    // Inspect a tile directly from lat/lon coordinates
    if args.len() >= 5 && args[1] == "--inspect-latlon" {
        let lat = match args[3].parse::<f64>() {
            Ok(v) => v,
            Err(_) => {
                eprintln!("Invalid latitude: {}", args[3]);
                return;
            }
        };
        let lon = match args[4].parse::<f64>() {
            Ok(v) => v,
            Err(_) => {
                eprintln!("Invalid longitude: {}", args[4]);
                return;
            }
        };
        let zoom = args.get(5).and_then(|z| z.parse::<u8>().ok()).unwrap_or(15);
        inspect_latlon_mode(&args[2], lat, lon, zoom);
        return;
    }

    // Scan charts for object class + attribute usage (Phase 2 of performance plan)
    if args.len() >= 3 && args[1] == "--scan" {
        scan_charts_mode(&args[2]);
        return;
    }

    // Dump what the renderer decided to draw, with provenance
    if args.len() >= 6 && args[1] == "--dump-scene" {
        dump_scene_mode(&args[2], &args[3], &args[4], &args[5]);
        return;
    }

    // Dump the S-52 instruction stream for conformance testing against OpenCPN
    if args.len() >= 4 && args[1] == "--dump-ir" {
        let limit = args.get(4).and_then(|s| s.parse::<usize>().ok());
        dump_ir_mode(&args[2], &args[3], limit);
        return;
    }

    // Where the charts come from: what was asked for on the command line,
    // else the folder the last session settled on, else nothing. The argument
    // wins so a developer can point at one set without disturbing the folder
    // the boat boots into — and a plotter with no keyboard needs the second
    // line, because otherwise it can only ever show the test triangle.
    let source = match args.get(1) {
        Some(p) => {
            let path = PathBuf::from(p);
            if path.is_dir() {
                ChartSource::Directory(path)
            } else {
                ChartSource::SingleFile(path)
            }
        }
        None => match RenderState::remembered_chart_folder() {
            Some(dir) => {
                println!("Opening the remembered chart folder: {}", dir.display());
                ChartSource::Directory(dir)
            }
            None => ChartSource::TestTriangle,
        },
    };

    let event_loop = EventLoop::new().expect("Failed to create event loop");
    // Control flow is set dynamically in about_to_wait()

    let mut app = App::new(source);
    event_loop.run_app(&mut app).expect("Event loop failed");
}

/// Dump every area primitive the renderer emits for a viewport, with the
/// provenance needed to trace a pixel back to a feature and a code path.
///
///   navcore --dump-scene <charts> <lat,lon,mpp> <WxH> <out.ndjson>
///
/// Coordinates in the output are screen pixels for that viewport, so a pixel
/// noticed in a capture can be looked up directly (tools/scenecheck.py).
fn dump_scene_mode(chart_path: &str, view: &str, size: &str, out_path: &str) {
    use std::io::Write;

    let parts: Vec<f64> = view.split(',').filter_map(|s| s.trim().parse().ok()).collect();
    let [lat, lon, mpp] = parts[..] else {
        eprintln!("--dump-scene: view must be 'lat,lon,mpp'");
        return;
    };
    let Some((w, h)) = size.split_once('x') else {
        eprintln!("--dump-scene: size must be 'WxH'");
        return;
    };
    let (view_w, view_h): (f64, f64) = match (w.trim().parse(), h.trim().parse()) {
        (Ok(a), Ok(b)) => (a, b),
        _ => {
            eprintln!("--dump-scene: size must be 'WxH'");
            return;
        }
    };

    let dir = PathBuf::from(chart_path);
    let mut keys = KeyStore::new();
    if let Err(e) = keys.load_keylists_in_dir(&dir) {
        eprintln!("Warning: could not load keys: {}", e);
    }
    let base = match ChartDecryptor::new("license") {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Failed to create decryptor: {}", e);
            return;
        }
    };
    let mut decryptor = CachedDecryptor::new(base);
    let catalog = match ChartCatalog::from_directory(&dir, &keys, &mut decryptor) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to build catalog: {}", e);
            return;
        }
    };
    let mut engine = match S52Engine::load("assets/s52/chartsymbols.xml") {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Failed to load chartsymbols.xml: {}", e);
            return;
        }
    };
    engine.set_settings(navcore2::s52::MarinerSettings::from_env());

    let (cx, cy) = navcore2::tiles::latlon_to_mercator(lat, lon);
    let z = zoom_from_camera(mpp as f32);
    let half_w = view_w * mpp / 2.0;
    let half_h = view_h * mpp / 2.0;
    let view_bounds = TileBounds::new(cx - half_w, cx + half_w, cy - half_h, cy + half_h);
    let tiles = visible_tiles(&view_bounds, z, 1.0);

    let chart_cache: navcore2::tiles::builder::ChartCache = Mutex::new(HashMap::new());
    let coverage_cache: navcore2::tiles::builder::CoverageCache = Mutex::new(HashMap::new());
    let decryptor = Mutex::new(decryptor);

    let mut out = std::io::BufWriter::new(
        std::fs::File::create(out_path).expect("create scene file"),
    );
    // Header: everything needed to map the records back to a capture.
    writeln!(
        out,
        "{}",
        serde_json::json!({
            "record": "viewport", "lat": lat, "lon": lon, "mpp": mpp,
            "width_px": view_w, "height_px": view_h, "zoom": z,
            "center_mercator": [cx, cy], "tiles": tiles.len(),
        })
    )
    .ok();

    // Global Mercator -> screen pixels, y down.
    let to_px = |p: [f64; 2]| [(p[0] - cx) / mpp + view_w / 2.0, view_h / 2.0 - (p[1] - cy) / mpp];

    let mut n_areas = 0usize;
    let mut n_fallback = 0usize;
    let mut n_skipped = 0usize;
    let mut n_lines = 0usize;
    for tile_id in tiles {
        let mut builder = TileBuilder::with_cache(&catalog, &keys, &decryptor, &chart_cache, &coverage_cache)
            .with_s52_engine(&engine)
            .with_view_params(mpp as f32, 8.0, view_w as f32, view_h as f32, cx as f32, cy as f32)
            .with_scene_log();
        if builder.build_cpu(tile_id).is_err() {
            continue;
        }
        let Some(log) = builder.take_scene_log() else {
            continue;
        };
        // json! cannot take a block, so the corner transform lives here.
        fn cov_extent_px(
            cov: &navcore2::tiles::scene::SceneCoverage,
            to_px: &impl Fn([f64; 2]) -> [f64; 2],
        ) -> [f64; 4] {
            let a = to_px([cov.extent[0], cov.extent[3]]);
            let b = to_px([cov.extent[2], cov.extent[1]]);
            [a[0], a[1], b[0], b[1]]
        }
        for (prio, count, b) in &log.packet {
            let a = to_px([b[0], b[3]]);
            let c = to_px([b[2], b[1]]);
            writeln!(
                out,
                "{}",
                serde_json::json!({
                    "record": "packet",
                    "tile": [tile_id.z, tile_id.x, tile_id.y],
                    "priority": prio, "vertices": count,
                    "bbox_px": [a[0], a[1], c[0], c[1]],
                })
            )
            .ok();
        }
        for cov in &log.coverage {
            writeln!(
                out,
                "{}",
                serde_json::json!({
                    "record": "coverage",
                    "tile": [tile_id.z, tile_id.x, tile_id.y],
                    "chart": cov.chart,
                    "chart_scale": cov.chart_scale,
                    "extent_px": cov_extent_px(cov, &to_px),
                    "polygons_px": cov.polygons.iter()
                        .map(|p| p.iter().map(|&v| to_px(v)).collect::<Vec<_>>())
                        .collect::<Vec<_>>(),
                })
            )
            .ok();
        }
        for skip in &log.skipped {
            n_skipped += 1;
            writeln!(
                out,
                "{}",
                serde_json::json!({
                    "record": "skip",
                    "tile": [tile_id.z, tile_id.x, tile_id.y],
                    "chart": skip.chart,
                    "feature": skip.feature,
                    "class": skip.class,
                    "reason": skip.reason,
                    "extent_px": skip.extent.map(|ext| {
                        let a = to_px([ext[0], ext[3]]);
                        let b = to_px([ext[2], ext[1]]);
                        [a[0], a[1], b[0], b[1]]
                    }),
                })
            )
            .ok();
        }
        for line in &log.lines {
            n_lines += 1;
            writeln!(
                out,
                "{}",
                serde_json::json!({
                    "record": "line",
                    "tile": [tile_id.z, tile_id.x, tile_id.y],
                    "chart": line.chart,
                    "feature": line.feature,
                    "class": line.class,
                    "source": line.source,
                    "priority": line.priority,
                    "is_background": line.is_background,
                    "style": line.style,
                    "polylines_px": line.polylines.iter()
                        .map(|p| p.iter().map(|&v| to_px(v)).collect::<Vec<_>>())
                        .collect::<Vec<_>>(),
                })
            )
            .ok();
        }
        for area in log.areas {
            n_areas += 1;
            if area.source == navcore2::tiles::scene::AreaSource::RingFallback {
                n_fallback += 1;
            }
            let tris_px: Vec<[[f64; 2]; 3]> = area
                .tris
                .iter()
                .map(|t| [to_px(t[0]), to_px(t[1]), to_px(t[2])])
                .collect();
            let ext = area.extent;
            let e0 = to_px([ext[0], ext[3]]); // NW corner -> top-left in pixels
            let e1 = to_px([ext[2], ext[1]]);
            writeln!(
                out,
                "{}",
                serde_json::json!({
                    "record": "area",
                    "tile": [tile_id.z, tile_id.x, tile_id.y],
                    "chart": area.chart,
                    "feature": area.feature,
                    "class": area.class,
                    "source": area.source,
                    "priority": area.priority,
                    "color_index": area.color_index,
                    "chart_scale": area.chart_scale,
                    "is_background": area.is_background,
                    "extent_px": [e0[0], e0[1], e1[0], e1[1]],
                    "tris_px": tris_px,
                })
            )
            .ok();
        }
    }

    eprintln!(
        "navcore --dump-scene: {} area primitives ({} via ring fallback), {} stroked, {} skipped -> {}",
        n_areas, n_fallback, n_lines, n_skipped, out_path
    );
}

/// Dump the S-52 symbology resolution for every feature in a chart or chart
/// directory, as NDJSON, for conformance testing against OpenCPN.
///
/// Writes two files:
///   <prefix>.features.ndjson — feature class/primitive/attributes, the input
///                              for tools/s52oracle
///   <prefix>.navcore.ndjson  — navcore's own LUP choice and expanded rules
///
/// Records are joined on `id`, so a line-by-line diff of navcore's stream
/// against the oracle's isolates every symbology divergence, independent of
/// rendering.
fn dump_ir_mode(chart_path: &str, out_prefix: &str, limit: Option<usize>) {
    use std::io::Write;
    use navcore2::s52::{DepthAreaIndex, GeometryType, S52Engine};
    use navcore2::senc::{AttributeValue, FeatureType};

    let path = PathBuf::from(chart_path);
    let chart_dir = if path.is_dir() {
        path.clone()
    } else {
        path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."))
    };

    let mut keys = KeyStore::new();
    if let Err(e) = keys.load_keylists_in_dir(&chart_dir) {
        eprintln!("Warning: could not load keys: {}", e);
    }

    let base_decryptor = match ChartDecryptor::new("license") {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Failed to create decryptor: {}", e);
            return;
        }
    };
    let mut decryptor = CachedDecryptor::new(base_decryptor);

    let engine = match S52Engine::load("assets/s52/chartsymbols.xml") {
        Ok(mut e) => {
            // Same mariner settings the renderer would use, so a conformance
            // run describes the picture actually being captured.
            e.set_settings(navcore2::s52::MarinerSettings::from_env());
            e
        }
        Err(e) => {
            eprintln!("Failed to load chartsymbols.xml: {}", e);
            return;
        }
    };

    // NAVCORE_VIEW_SCALE=<denominator> turns on the visibility verdict.
    let view_scale: Option<f64> = std::env::var("NAVCORE_VIEW_SCALE")
        .ok()
        .and_then(|v| v.parse().ok());

    let mut chart_files: Vec<PathBuf> = if path.is_dir() {
        std::fs::read_dir(&path)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .map(|x| x.eq_ignore_ascii_case("oesu"))
                    .unwrap_or(false)
            })
            .collect()
    } else {
        vec![path.clone()]
    };
    chart_files.sort();
    if let Some(n) = limit {
        chart_files.truncate(n);
    }

    let mut feat_out = std::io::BufWriter::new(
        std::fs::File::create(format!("{}.features.ndjson", out_prefix)).expect("create features file"),
    );
    let mut ir_out = std::io::BufWriter::new(
        std::fs::File::create(format!("{}.navcore.ndjson", out_prefix)).expect("create ir file"),
    );

    let mut total = 0usize;
    let mut no_lup = 0usize;

    for chart_file in chart_files.iter() {
        let name = chart_file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let Some(key) = keys.lookup(name).map(|k| k.to_string()) else {
            eprintln!("skip {} (no key)", name);
            continue;
        };
        let bytes = match decryptor.decrypt_chart(chart_file.to_str().unwrap(), &key) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skip {} (decrypt: {})", name, e);
                continue;
            }
        };
        let chart = match ChartData::parse(bytes) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("skip {} (parse: {})", name, e);
                continue;
            }
        };

        // Same neighbourhood information OpenCPN's GetAssociatedObjects would
        // supply, so UDWHAZ03 can decide isolated dangers.
        let depth_index =
            DepthAreaIndex::build(&chart.features, chart.header.ref_lat, chart.header.ref_lon);
        let chart_safety = engine.chart_safety_contour(&chart.features);

        for (fi, feature) in chart.features.iter().enumerate() {
            // Stable across chart ordering, filtering and single-chart runs, so
            // a divergence found in a full-set run can be reproduced from one
            // chart and kept as a regression fixture.
            let id = format!("{}#{}", name, fi);
            let acronym = s57_code_to_acronym(feature.type_code);
            let mut ctx = depth_index.context_for(feature);
            if feature.type_code == 43 {
                ctx.safety_contour = chart_safety;
            }
            let (prim, geom) = match feature.feature_type {
                FeatureType::Point | FeatureType::Multipoint => ("P", GeometryType::Point),
                FeatureType::Line => ("L", GeometryType::Line),
                FeatureType::Area => ("A", GeometryType::Area),
            };

            // Feature record: attributes carry their S-57 storage type so the
            // oracle rebuilds the same typed S57Obj that OpenCPN would.
            let mut attrs = serde_json::Map::new();
            for (k, v) in &feature.attributes {
                let pair = match v {
                    AttributeValue::Integer(i) => {
                        serde_json::json!(["I", i])
                    }
                    AttributeValue::Float(f) => serde_json::json!(["F", f]),
                    AttributeValue::String(s) => serde_json::json!(["S", s]),
                };
                attrs.insert(k.to_string(), pair);
            }
            // The surrounding depth areas, so tools/s52oracle can run OpenCPN's
            // UDWHAZ03 over the same neighbourhood instead of its no-chart
            // fallback. Emitted only where a CS procedure consults it.
            // Visibility: would this feature be drawn at all? Emitted only when
            // NAVCORE_VIEW_SCALE is set, so the harness can diff the decision
            // against OpenCPN's ObjectRenderCheckCat. This is a third decision
            // layer, separate from "which symbology" and "which geometry": a
            // feature can resolve perfectly and still never reach the screen.
            let visibility = view_scale.map(|vs| {
                let cat = engine.display_category_for(feature, geom);
                // DEPCNT02 promotes the selected safety contour to DISPLAYBASE
                // and clears its SCAMIN; the renderer does the same, so the
                // harness has to model it or it reports a divergence that is
                // really the promotion.
                let bypass = cat == Some(navcore2::s52::DisplayCategory::Displaybase)
                    || engine.is_promoted_safety_contour(feature, chart_safety);
                let scale_ok = navcore2::tiles::builder::should_render_at_scale_ex(
                    feature,
                    vs,
                    bypass,
                    false,
                    engine
                        .settings
                        .use_super_scamin
                        .then_some(chart.header.native_scale),
                ) > 0.0;
                let category_ok = engine.resolve_feature(feature, geom).is_some();
                (scale_ok && category_ok, scale_ok, category_ok)
            });

            let vis_json = visibility.map(|(v, s, c)| {
                serde_json::json!({"visible": v, "scale_ok": s, "category_ok": c})
            });
            let frec = if ctx.is_empty() {
                serde_json::json!({
                    "id": id, "obj": acronym, "prim": prim, "attrs": attrs,
                    "chart_scale": chart.header.native_scale,
                    "view_scale": view_scale,
                })
            } else {
                serde_json::json!({
                    "id": id, "obj": acronym, "prim": prim, "attrs": attrs,
                    "chart_scale": chart.header.native_scale,
                    "view_scale": view_scale,
                    "assoc": {
                        "area_drval1": ctx.area_drval1,
                        "line_drval2": ctx.line_drval2,
                    },
                })
            };
            writeln!(feat_out, "{}", frec).ok();

            // navcore's resolution of the same feature
            let (entry, expanded) = engine.resolve_ir(feature, geom, &ctx);
            let irec = match entry {
                Some(e) => {
                    let rules: Vec<String> = e
                        .instruction
                        .split(';')
                        .map(|s| s.trim_matches(|c: char| c == '\u{1f}' || c.is_whitespace()))
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .collect();
                    let exp: Vec<String> = expanded.iter().map(|i| i.to_s52()).collect();
                    serde_json::json!({
                        "id": id, "obj": acronym, "prim": prim,
                        "lup": {
                            "tnam": format!("{:?}", e.table_name),
                            "dpri": e.display_priority.as_u8(),
                            "disc": format!("{:?}", e.display_category),
                            "inst": e.instruction,
                        },
                        "rules": rules,
                        "expanded": exp,
                        "visibility": vis_json,
                    })
                }
                None => {
                    no_lup += 1;
                    serde_json::json!({
                        "id": id, "obj": acronym, "prim": prim,
                        "lup": serde_json::Value::Null,
                        "rules": Vec::<String>::new(),
                        "expanded": Vec::<String>::new(),
                    })
                }
            };
            writeln!(ir_out, "{}", irec).ok();
            total += 1;
        }
        eprintln!("dumped {} ({} features)", name, chart.features.len());
    }

    eprintln!(
        "navcore --dump-ir: {} features from {} charts, {} without a LUP",
        total,
        chart_files.len(),
        no_lup
    );
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

    let chart_cache: navcore2::tiles::builder::ChartCache = Mutex::new(HashMap::new());
    let coverage_cache: navcore2::tiles::builder::CoverageCache = Mutex::new(HashMap::new());
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

    let builder = TileBuilder::with_cache(&catalog, &keys, &decryptor, &chart_cache, &coverage_cache);
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
                    // Packet positions are relative to the tile centre.
                    let o = tile_id.origin();
                    println!(
                        "  first area vert: pos=({:.0},{:.0}) color_index={}",
                        o.x + v.position[0] as f64,
                        o.y + v.position[1] as f64,
                        v.color_index
                    );
                }
            }
            Err(e) => eprintln!("  build_cpu failed: {}", e),
        }
    }
}

fn inspect_tile_mode(dir_path: &str, chart_stem: &str, z: u8) {
    let dir = PathBuf::from(dir_path);
    if !dir.is_dir() {
        eprintln!("Error: {} is not a directory", dir_path);
        return;
    }

    let mut keys = KeyStore::new();
    if let Err(e) = keys.load_keylists_in_dir(&dir) {
        eprintln!("Warning: Could not load keys: {}", e);
    }

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

    let Some(info) = catalog
        .charts
        .iter()
        .find(|c| c.name == chart_stem || c.path.file_stem().and_then(|s| s.to_str()) == Some(chart_stem))
    else {
        eprintln!("Chart '{}' not found in catalog", chart_stem);
        return;
    };

    let center_lat = info.extent_wgs84.center_lat();
    let center_lon = info.extent_wgs84.center_lon();
    let (mx, my) = navcore2::tiles::latlon_to_mercator(center_lat, center_lon);
    let tile_id = TileId::from_mercator(mx, my, z);

    let chart_cache: navcore2::tiles::builder::ChartCache = Mutex::new(HashMap::new());
    let coverage_cache: navcore2::tiles::builder::CoverageCache = Mutex::new(HashMap::new());
    let decryptor = Mutex::new(decryptor);

    let s52_engine = match S52Engine::load("assets/s52/chartsymbols.xml") {
        Ok(engine) => Some(engine),
        Err(e) => {
            eprintln!("Warning: Could not load S-52 engine: {}", e);
            None
        }
    };

    let builder = TileBuilder::with_cache(&catalog, &keys, &decryptor, &chart_cache, &coverage_cache);
    let builder = if let Some(ref engine) = s52_engine {
        builder.with_s52_engine(engine)
    } else {
        builder
    };

    println!(
        "Inspecting chart '{}' at center ({:.5}, {:.5}) -> tile {:?} z={}",
        chart_stem, center_lat, center_lon, tile_id, z
    );

    match builder.inspect_tile_coverage(tile_id) {
        Ok(report) => println!("{}", report),
        Err(e) => eprintln!("inspect_tile_coverage failed: {}", e),
    }
}

/// `navcore --shop-list <email> [charts_dir]`
///
/// Signs in to o-charts and lists what the account owns, marking what is
/// installed in `charts_dir` and what has a newer edition waiting. The password
/// comes from `NAVCORE_SHOP_PASSWORD` or, failing that, is read from stdin —
/// never from the command line, where it would land in the shell history and in
/// every process listing on the machine.
fn shop_list_mode(email: &str, charts_dir: Option<&str>) {
    use navcore2::shop::{protocol, types::Edition, Fingerprint, ShopClient};

    let password = match std::env::var("NAVCORE_SHOP_PASSWORD") {
        Ok(p) if !p.is_empty() => p,
        _ => {
            eprint!("o-charts password for {email}: ");
            use std::io::Write;
            let _ = std::io::stderr().flush();
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() {
                eprintln!("could not read the password");
                return;
            }
            line.trim_end_matches(['\n', '\r']).to_string()
        }
    };

    let client = ShopClient::new();
    println!("Signing in as {email} (client version {})", protocol::client_version());
    let session = match client.login(email, &password) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Sign-in failed: {e}");
            return;
        }
    };

    // Identify this machine, so the listing can say which charts are usable here.
    let mut system_name = None;
    match ChartDecryptor::new("license") {
        Ok(d) => match Fingerprint::load(d.fpr_path()) {
            Ok(fpr) => match client.identify_system(&session, &fpr.bytes, &fpr.name) {
                Ok(Some(name)) => {
                    println!("This machine is registered as \"{name}\"");
                    system_name = Some(name);
                }
                Ok(None) => println!("This machine is not yet registered with the account"),
                Err(e) => eprintln!("Could not identify this machine: {e}"),
            },
            Err(e) => eprintln!("Could not read the fingerprint: {e}"),
        },
        Err(e) => eprintln!("No chart licence on this machine: {e}"),
    }

    // What is installed already, by chart-set directory name.
    let installed = charts_dir
        .map(|d| installed_editions(std::path::Path::new(d)))
        .unwrap_or_default();

    let (charts, systems) = match client.list_charts(&session) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Could not list charts: {e}");
            return;
        }
    };
    if !systems.is_empty() {
        println!("Machines on this account: {}", systems.join(", "));
    }
    println!("\n{} chart set(s):\n", charts.len());
    for c in &charts {
        let here = installed.get(&c.id).copied();
        let target = protocol::choose_download(here, c.edition);
        let assigned = system_name
            .as_deref()
            .and_then(|n| c.slot_for(n))
            .is_some();
        println!(
            "{:<28} {:<10} shop {:<12} {}{}{}",
            c.name,
            c.id,
            c.edition.to_string(),
            target.label(),
            if let Some(e) = here { format!(" (have {e})") } else { String::new() },
            match (assigned, c.expired) {
                (_, true) => "  [EXPIRED]",
                (false, _) => "  [not assigned to this machine]",
                _ => "",
            }
        );
    }
    let _ = Edition::default();
}

/// Editions already on disk, keyed by chart id, read from each set's ChartList.XML.
fn installed_editions(dir: &std::path::Path) -> HashMap<String, navcore2::shop::types::Edition> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let list = entry.path().join("ChartList.XML");
        let list = if list.exists() { list } else { entry.path() };
        if list.file_name().and_then(|n| n.to_str()) != Some("ChartList.XML") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&list) {
            // The set's own name is the directory; the edition is per chart.
            if let Some(id) = list.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()) {
                if let Some(ed) = text
                    .split("<RE>")
                    .nth(1)
                    .and_then(|s| s.split("</RE>").next())
                {
                    out.insert(id.to_string(), navcore2::shop::types::Edition::parse(ed));
                }
            }
        }
    }
    out
}

/// `navcore --pick <charts> <lat,lon> [tolerance_m]`
///
/// The same query the info bubble runs, on the command line, so the picking can
/// be checked against a chart without a window in the way.
/// M3's surface: plan a safe route on the real charts and drop it in the
/// route store, where the Routes window and GPX export pick it up.
/// M5's surface: fetch real wind, plan a real sailing route, store it.
fn weather_route_mode(dir_path: &str, a: (f64, f64), b: (f64, f64), hours: u32) {
    use navcore2::geo::LatLon;
    use navcore2::nav::autoroute::SafetyConfig;
    use navcore2::nav::wxroute;

    let start = LatLon::new(a.0, a.1);
    let finish = LatLon::new(b.0, b.1);
    let cache = dirs::config_dir()
        .map(|d| d.join("navcore").join("grib"))
        .unwrap_or_else(|| PathBuf::from("grib-cache"));
    let depart = chrono::Utc::now();
    // The same function the app's Sail button runs, so the command line
    // reproduces exactly what a user saw.
    let planned = match wxroute::plan_from_chart_dir(
        std::path::Path::new(dir_path),
        start,
        finish,
        hours,
        &cache,
        wxroute::DEFAULT_POLAR,
        "built-in cruiser",
        &navcore2::nav::RoutingConfig::default(),
        &SafetyConfig::default(),
        true,
        true,
        &format!("Wx {:.2},{:.2} to {:.2},{:.2}", a.0, a.1, b.0, b.1),
        |msg| println!("  {msg}"),
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("No route: {e}");
            std::process::exit(1);
        }
    };
    for w in &planned.warnings {
        println!("  note: {w}");
    }
    println!(
        "\nArrival {} ({:.1} h under way)",
        planned.arrival.format("%Y-%m-%d %H:%M UTC"),
        (planned.arrival - depart).num_minutes() as f64 / 60.0
    );
    for (i, leg) in planned.route.legs.iter().enumerate() {
        let plan = leg.plan.as_ref();
        println!(
            "  leg {:>2}: {:>6.2} nm  {:>5.1}°  {}",
            i + 1,
            leg.distance_nm,
            leg.initial_bearing_deg,
            plan.map(|p| format!(
                "TWA {:>4.0}  TWS {:>4.1} kt  STW {:.1} kt  {:?}  ETA {}",
                p.twa_deg,
                p.tws_kt,
                p.expected_stw_kt,
                p.point_of_sail,
                p.eta.format("%H:%M")
            ))
            .unwrap_or_default()
        );
    }
    save_cli_route(&planned.route, &planned.waypoints);
}

fn auto_route_mode(dir_path: &str, a: (f64, f64), b: (f64, f64), draft: Option<f64>) {
    use navcore2::geo::LatLon;
    use navcore2::nav::autoroute::SafetyConfig;

    let mut safety = SafetyConfig::default();
    if let Some(d) = draft {
        safety.draft_m = d;
    }
    let (start, finish) = (LatLon::new(a.0, a.1), LatLon::new(b.0, b.1));
    println!(
        "Planning {:.1} nm passage, draft {:.1} m…",
        navcore2::geo::distance_m(start, finish) / 1852.0,
        safety.draft_m
    );
    // The same function the app's Motor button runs.
    let planned = match navcore2::nav::wxroute::motor_plan_from_chart_dir(
        std::path::Path::new(dir_path),
        start,
        finish,
        &safety,
        |msg| println!("  {msg}"),
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("No route: {e}");
            std::process::exit(1);
        }
    };
    for w in &planned.warnings {
        println!("  note: {w}");
    }
    println!(
        "\n{}: {} waypoints, {:.1} nm",
        planned.route.name,
        planned.route.waypoints.len(),
        planned.route.total_distance_nm()
    );
    for (i, leg) in planned.route.legs.iter().enumerate() {
        println!(
            "  leg {:>2}: {:>6.2} nm  {:>5.1}°",
            i + 1,
            leg.distance_nm,
            leg.initial_bearing_deg
        );
    }
    save_cli_route(&planned.route, &planned.waypoints);
}

/// Into the route store (`NAVCORE_ROUTES`, or the app's own), so the Routes
/// window sees a command-line plan immediately.
fn save_cli_route(route: &navcore2::nav::Route, waypoints: &[navcore2::nav::model::Waypoint]) {
    let store_dir = std::env::var("NAVCORE_ROUTES")
        .map(PathBuf::from)
        .ok()
        .or_else(navcore2::nav::RouteStore::default_dir)
        .unwrap_or_else(|| PathBuf::from("routes"));
    match navcore2::nav::RouteStore::open(store_dir) {
        Ok((mut store, _)) => {
            for wp in waypoints {
                store.waypoints.insert(wp.clone());
            }
            match store.upsert_route(route.clone()) {
                Ok(()) => println!("saved to the route store"),
                Err(e) => eprintln!("could not save: {e}"),
            }
        }
        Err(e) => eprintln!("route store unavailable: {e}"),
    }
}

/// `--s57-dump <cell.000>`: the cell through navcore's whole S-57 path —
/// read, updates applied, encoded as SENC, parsed back — one JSON object per
/// feature, in the shape tools/s57oracle prints, for tools/s57diff.py.
fn s57_dump_mode(path: &str) {
    use navcore2::senc::{AttributeValue, ChartData, FeatureType};
    let cell = match navcore2::s57::cell::Cell::open(std::path::Path::new(path)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let (bytes, ids) = navcore2::s57::senc_encode::encode_with_ids(&cell);
    let chart = match ChartData::parse(bytes) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SENC parse: {e}");
            std::process::exit(1);
        }
    };
    let (rlat, rlon) = (chart.header.ref_lat, chart.header.ref_lon);
    let (rx, ry) = navcore2::tiles::latlon_to_mercator(rlat, rlon);
    let ll = |x: f64, y: f64| {
        let (lat, lon) = navcore2::tiles::mercator_to_latlon(x, y);
        serde_json::json!([(lon * 1e7).round() / 1e7, (lat * 1e7).round() / 1e7])
    };
    println!(
        "{}",
        serde_json::json!({"dsid": {"dsnm": cell.dataset.dsnm, "updn": cell.dataset.update,
            "cscl": cell.dataset.cscl, "updates_applied": cell.updates_applied,
            "features": chart.features.len()}})
    );
    for (f, rcid) in chart.features.iter().zip(ids) {
        let mut attrs = serde_json::Map::new();
        for (code, v) in f.attributes.iter_codes() {
            let name = navcore2::senc::s57_attribute_name(code)
                .map(str::to_string)
                .unwrap_or_else(|| format!("#{code}"));
            let s = match v {
                AttributeValue::Integer(i) => i.to_string(),
                AttributeValue::Float(x) => x.to_string(),
                AttributeValue::String(s) => s.clone(),
            };
            attrs.insert(name, serde_json::Value::String(s));
        }
        let geom = match f.feature_type {
            FeatureType::Point => f.point_geometry.as_ref().map(|p| {
                serde_json::json!({"type": "Point", "c": [p.y, p.x]})
            }),
            FeatureType::Multipoint => f.multipoint_geometry.as_ref().map(|m| {
                let c: Vec<_> = m
                    .points
                    .iter()
                    .map(|p| {
                        let mut v = ll(rx + p[0] as f64, ry + p[1] as f64);
                        v.as_array_mut().unwrap().push(serde_json::json!(p[2]));
                        v
                    })
                    .collect();
                serde_json::json!({"type": "MultiPoint", "c": c})
            }),
            FeatureType::Line => f.line_geometry.as_ref().map(|g| {
                let parts = g.resolve_global(&chart.edge_table, rlat, rlon);
                let len: f64 = parts
                    .iter()
                    .flat_map(|p| p.windows(2))
                    .map(|w| (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]))
                    .sum();
                serde_json::json!({"type": "Lines", "parts": parts.len(), "len": len})
            }),
            FeatureType::Area => f.area_geometry.as_ref().map(|g| {
                let mut area = 0.0;
                g.for_each_triangle_global(rlat, rlon, |t| {
                    area += ((t[1][0] - t[0][0]) * (t[2][1] - t[0][1])
                        - (t[2][0] - t[0][0]) * (t[1][1] - t[0][1]))
                        .abs()
                        / 2.0;
                });
                let rings = g.resolve_closed_rings(&chart.edge_table).len();
                serde_json::json!({"type": "Area", "area": area, "rings": rings})
            }),
        };
        println!(
            "{}",
            serde_json::json!({"rcid": rcid, "objl": f.type_code,
                "acronym": navcore2::senc::s57_code_to_acronym(f.type_code),
                "attrs": attrs, "geom": geom})
        );
    }
}

fn pick_mode(dir_path: &str, lat: f64, lon: f64, tolerance_m: f64) {
    let dir = PathBuf::from(dir_path);
    let mut keys = KeyStore::new();
    if let Err(e) = keys.load_keylists_in_dir(&dir) {
        eprintln!("Warning: Could not load keys: {}", e);
    }
    let Ok(base) = ChartDecryptor::new("license") else {
        eprintln!("Failed to create decryptor");
        return;
    };
    let mut decryptor = CachedDecryptor::new(base);
    let catalog = match ChartCatalog::from_directory(&dir, &keys, &mut decryptor) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to build catalog: {}", e);
            return;
        }
    };

    let (mx, my) = navcore2::tiles::latlon_to_mercator(lat, lon);
    let chart_cache: navcore2::tiles::builder::ChartCache = Mutex::new(HashMap::new());
    let coverage_cache: navcore2::tiles::builder::CoverageCache = Mutex::new(HashMap::new());
    let decryptor = Mutex::new(decryptor);
    let builder =
        TileBuilder::with_cache(&catalog, &keys, &decryptor, &chart_cache, &coverage_cache);

    let mut found = Vec::new();
    for info in catalog.charts.iter().filter(|c| c.contains_point(mx, my)) {
        match builder.load_chart(info) {
            Ok(chart) => found.extend(navcore2::pick::pick_at(&chart, info, [mx, my], tolerance_m)),
            Err(e) => eprintln!("  (skipping {}: {})", info.name, e),
        }
    }
    navcore2::pick::sort_picks(&mut found);
    let found = navcore2::pick::dedupe_picks(found);

    println!("{} object(s) within {:.0} m of {:.5},{:.5}\n", found.len(), tolerance_m, lat, lon);
    for o in &found {
        let also = if o.duplicates > 0 {
            format!("   (+{} identical)", o.duplicates)
        } else {
            String::new()
        };
        println!("{} ({})   1:{} {}{also}", o.title, o.acronym, o.chart_scale, o.chart);
        if let Some(summary) = &o.summary {
            println!("    {summary}");
        }
        for (k, v) in o.attributes.iter().filter(|(k, _)| navcore2::pick::is_worth_showing(k)) {
            println!("    {:<8} {}", k, v);
        }
        for n in &o.notes {
            for line in n.lines() {
                println!("    | {}", line);
            }
        }
        println!();
    }
}

fn inspect_latlon_mode(dir_path: &str, lat: f64, lon: f64, z: u8) {
    let dir = PathBuf::from(dir_path);
    if !dir.is_dir() {
        eprintln!("Error: {} is not a directory", dir_path);
        return;
    }

    let mut keys = KeyStore::new();
    if let Err(e) = keys.load_keylists_in_dir(&dir) {
        eprintln!("Warning: Could not load keys: {}", e);
    }

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

    let (mx, my) = navcore2::tiles::latlon_to_mercator(lat, lon);
    let tile_id = TileId::from_mercator(mx, my, z);

    let chart_cache: navcore2::tiles::builder::ChartCache = Mutex::new(HashMap::new());
    let coverage_cache: navcore2::tiles::builder::CoverageCache = Mutex::new(HashMap::new());
    let decryptor = Mutex::new(decryptor);

    let s52_engine = match S52Engine::load("assets/s52/chartsymbols.xml") {
        Ok(engine) => Some(engine),
        Err(e) => {
            eprintln!("Warning: Could not load S-52 engine: {}", e);
            None
        }
    };

    let builder = TileBuilder::with_cache(&catalog, &keys, &decryptor, &chart_cache, &coverage_cache);
    let builder = if let Some(ref engine) = s52_engine {
        builder.with_s52_engine(engine)
    } else {
        builder
    };

    println!(
        "Inspecting lat/lon ({:.5}, {:.5}) -> tile {:?} z={}",
        lat, lon, tile_id, z
    );

    // List every chart whose extent contains this point, finest scale first.
    let mut covering: Vec<&navcore2::senc::ChartInfo> = catalog
        .charts
        .iter()
        .filter(|c| c.contains_point(mx, my))
        .collect();
    covering.sort_by_key(|c| c.native_scale);
    println!("Charts whose extent contains the point ({}):", covering.len());
    for c in covering.iter().take(15) {
        println!(
            "  1:{:<8} {:<20} center=({:.4},{:.4})",
            c.native_scale,
            c.name,
            c.extent_wgs84.center_lat(),
            c.extent_wgs84.center_lon()
        );
    }

    match builder.inspect_tile_coverage(tile_id) {
        Ok(report) => println!("{}", report),
        Err(e) => eprintln!("inspect_tile_coverage failed: {}", e),
    }

    // Build the actual GPU packet and report renderable element counts so we can
    // see whether soundings / symbols / text glyphs are produced at this zoom.
    match builder.build_cpu(tile_id) {
        Ok(packet) => {
            let mut color_hist: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
            for v in &packet.area_vertices {
                *color_hist.entry(v.color_index).or_insert(0) += 1;
            }
            let mut colors: Vec<_> = color_hist.into_iter().collect();
            colors.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
            println!(
                "PACKET COUNTS @z{}: area_verts={} line_batches={} line_verts={} symbols={} soundings(text_instances)={}",
                z,
                packet.area_vertices.len(),
                packet.line_batches.len(),
                packet.total_line_vertices(),
                packet.symbol_instances.len(),
                packet.text_instances.len(),
            );
            println!("  area color_index histogram (index:count): {:?}", colors);
        }
        Err(e) => eprintln!("build_cpu failed: {}", e),
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
            let class_name = s57_code_to_acronym(feature.type_code).to_string();

            // Count
            *class_counts.entry(class_name.clone()).or_insert(0) += 1;

            // Attributes
            let attrs = class_attrs.entry(class_name.clone()).or_default();
            for (attr_name, _) in &feature.attributes {
                attrs.insert(attr_name.to_string());
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
    /// Where the left button went down, so a click can be told from a drag.
    drag_start: Option<PhysicalPosition<f64>>,
    /// The press that began this gesture landed on the interface, so the whole
    /// gesture belongs to it — press, drag and release alike. Decided once, at
    /// the press, because a drag that starts on a panel and wanders over the
    /// chart is still the panel's drag.
    gesture_on_ui: bool,
    /// The pointer has moved far enough since the press for this to be a drag
    /// rather than a click.
    ///
    /// Without it, every click panned the chart by the two or three pixels the
    /// hand moves between pressing and releasing: the object bubble opened and
    /// the chart slid out from under it at the same moment.
    panning: bool,
    /// The same, for a single finger.
    touch_start: Option<PhysicalPosition<f64>>,
    // Touch state for pinch-zoom
    touches: HashMap<u64, PhysicalPosition<f64>>,
    pinch_start_distance: Option<f32>,
    /// Where the two fingers' midpoint was, so a pinch can pan as it zooms.
    pinch_centroid: Option<(f32, f32)>,
    /// Shift, Ctrl, Cmd — for the keyboard shortcuts and Ctrl-scroll zoom.
    modifiers: winit::keyboard::ModifiersState,
    // Headless capture (NAVCORE_SHOT=path): render until tiles settle, save PNG, exit.
    shot_path: Option<String>,
    shot_requested: bool,
    frame_count: u32,
    /// NAVCORE_STRESS=coast: legs of the coastal tour done so far.
    stress_leg: usize,
}

/// Where a wheel or trackpad zoom should hold still: under the pointer, or
/// on the boat while the chart is following her — zooming anywhere else
/// would be undone by the next position report.
fn zoom_anchor(state: &RenderState, pointer: PhysicalPosition<f64>) -> (f32, f32) {
    if state.follow_on() {
        (state.camera.viewport_width * 0.5, state.camera.viewport_height * 0.5)
    } else {
        (pointer.x as f32, pointer.y as f32)
    }
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
            drag_start: None,
            gesture_on_ui: false,
            panning: false,
            touch_start: None,
            touches: HashMap::new(),
            pinch_start_distance: None,
            shot_path: std::env::var("NAVCORE_SHOT").ok().filter(|s| !s.is_empty()),
            shot_requested: false,
            pinch_centroid: None,
            modifiers: winit::keyboard::ModifiersState::empty(),
            frame_count: 0,
            stress_leg: 0,
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

        // Create decryptor with disk cache (will be moved to RenderState).
        // The o-charts helper if it starts; S-57 cells load either way.
        let mut decryptor = CachedDecryptor::open("license");

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
            // A downloaded set is unpacked beside the ones already here, so
            // the catalogue picks it up the same way.
            state.set_chart_root(dir.clone());
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
        // Headless capture mode: drive frames until tiles settle, capture, then exit.
        if let Some(path) = self.shot_path.clone() {
            if let Some(state) = &mut self.state {
                if self.shot_requested {
                    if state.capture_pending() {
                        state.window().request_redraw();
                        event_loop.set_control_flow(ControlFlow::WaitUntil(
                            std::time::Instant::now() + std::time::Duration::from_millis(16),
                        ));
                    } else {
                        event_loop.exit();
                    }
                } else {
                    // Warm-up frames + drained tile queue => view has settled.
                    // Also refuse to capture until the surface actually has the
                    // size NAVCORE_SIZE asked for: the window can come up at the
                    // default size and be resized a frame later, and a capture
                    // taken in between is geometrically wrong.
                    let size_ready = match std::env::var("NAVCORE_SIZE").ok().and_then(|s| {
                        let (w, h) = s.split_once('x')?;
                        Some((w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?))
                    }) {
                        Some((w, h)) => {
                            let got = state.window().inner_size();
                            got.width == w && got.height.abs_diff(h) <= 2
                        }
                        None => true,
                    };
                    // NAVCORE_SHOT_FRAMES lengthens the warm-up, for captures
                    // that must wait on something slower than the tiles — a
                    // Signal K connection, say.
                    let warmup: u32 = std::env::var("NAVCORE_SHOT_FRAMES")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(120);
                    // A routing worker still planning is the same kind of
                    // unsettled as an undrained tile queue: the capture
                    // would race it, and exiting would kill it mid-plan.
                    if self.frame_count >= warmup
                        && size_ready
                        && state.pending_tiles_empty()
                        && !state.routing_busy()
                    {
                        state.request_capture(path);
                        self.shot_requested = true;
                    }
                    state.window().request_redraw();
                    event_loop.set_control_flow(ControlFlow::WaitUntil(
                        std::time::Instant::now() + std::time::Duration::from_millis(16),
                    ));
                }
            }
            return;
        }

        // NAVCORE_STRESS=1: pan in a circle and zoom out ~64x and back, every
        // frame, forever. NAVCORE_STRESS=coast: follow the southern California
        // coast, San Diego Bay to LA harbour and back, zooming 0.3-8 m/px.
        // A GPU soak test that needs no hands on the mouse.
        let stress = std::env::var("NAVCORE_STRESS").unwrap_or_default();
        if !stress.is_empty() && stress != "0" {
            if let Some(state) = &mut self.state {
                let t = self.frame_count as f32 / 60.0;
                let (w, h) = (state.camera.viewport_width, state.camera.viewport_height);
                if stress == "coast" {
                    const COAST: [(f64, f64); 13] = [
                        (32.680, -117.235), (32.700, -117.225), (32.715, -117.175),
                        (32.680, -117.200), (32.680, -117.235), (32.765, -117.240),
                        (32.850, -117.280), (32.960, -117.280), (33.207, -117.400),
                        (33.460, -117.700), (33.600, -117.890), (33.750, -118.200),
                        (33.720, -118.270),
                    ];
                    // Leg index runs out and back: 0..12, 12..0, ...
                    let n = COAST.len();
                    let leg = self.stress_leg % (2 * (n - 1));
                    let i = if leg < n - 1 { leg + 1 } else { 2 * (n - 1) - leg - 1 };
                    let (tx, ty) = navcore2::render::Projection::to_mercator(COAST[i].0, COAST[i].1);
                    let d = glam::DVec2::new(tx, ty) - state.camera.position;
                    let step = 30.0 * state.camera.zoom as f64;
                    if d.length() <= step {
                        state.camera.position = glam::DVec2::new(tx, ty);
                        self.stress_leg += 1;
                    } else {
                        state.camera.position += d / d.length() * step;
                    }
                    let mpp = (0.3f32.ln() + (8.0f32.ln() - 0.3f32.ln()) * (0.5 - 0.5 * (0.4 * t).cos())).exp();
                    state.camera.zoom = mpp.max(state.camera.min_zoom);
                } else {
                    state.camera.zoom_at((-0.0175 * (0.5 * t).sin()).exp(), w * 0.5, h * 0.5);
                    state.camera.pan(w * 0.02 * (0.7 * t).cos(), h * 0.02 * (0.7 * t).sin());
                }
                state.window().request_redraw();
                event_loop.set_control_flow(ControlFlow::WaitUntil(
                    std::time::Instant::now() + std::time::Duration::from_millis(16),
                ));
                return;
            }
        }

        if let Some(ref state) = self.state {
            if state.needs_redraw() {
                // Immediate redraw for user interaction
                state.window().request_redraw();
            } else if !state.pending_tiles_empty()
                || state.pick_in_flight()
                || state.ui_wants_repaint()
            {
                // Fast polling while tiles load — ~60fps for responsive tile appearance
                event_loop.set_control_flow(ControlFlow::WaitUntil(
                    std::time::Instant::now() + std::time::Duration::from_millis(16),
                ));
                state.window().request_redraw();
            } else if state.routing_busy() {
                // A worker's answer is drained during a frame, and nothing
                // else would ask for one: a fetched route or a finished plan
                // would otherwise sit in its channel until the user happened
                // to touch the screen. Slow poll — these take seconds.
                event_loop.set_control_flow(ControlFlow::WaitUntil(
                    std::time::Instant::now() + std::time::Duration::from_millis(150),
                ));
                state.window().request_redraw();
            } else if state.signalk_live() {
                // Live data arrives on its own schedule, not the user's.
                // Four frames a second is enough for a plotter and cheap.
                event_loop.set_control_flow(ControlFlow::WaitUntil(
                    std::time::Instant::now() + std::time::Duration::from_millis(250),
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

            // NAVCORE_SIZE="WxH" requests an exact physical (pixel) surface size so
            // captures can be compared pixel-for-pixel with a reference screenshot.
            let attrs = Window::default_attributes().with_title(title);
            let attrs = match std::env::var("NAVCORE_SIZE").ok().and_then(|s| {
                let (w, h) = s.split_once('x')?;
                Some((w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?))
            }) {
                Some((w, h)) => attrs.with_inner_size(winit::dpi::PhysicalSize::new(w, h)),
                None => attrs.with_inner_size(winit::dpi::LogicalSize::new(1024, 768)),
            };

            let window = Arc::new(
                event_loop.create_window(attrs).expect("Failed to create window"),
            );

            let mut state = pollster::block_on(RenderState::new(window));
            state.init_ui();
            self.state = Some(state);

            // Load charts after renderer is ready
            self.load_charts();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = &mut self.state else { return };

        // The UI sees every event first. When it takes one — a click on a
        // panel, a drag of its title bar — the chart must not also act on it,
        // or dragging the object bubble would pan the map underneath.
        let consumed = state.ui_on_window_event(&event);

        // Bookkeeping that must happen whoever takes the event. The pointer
        // is where it is even when a window's title bar is being dragged —
        // skipping this left the next zoom anchored where the drag began —
        // and a finger lifted off a button has still left the glass, or the
        // next single-finger drag would pinch against a phantom.
        match &event {
            WindowEvent::CursorMoved { position, .. } if consumed => {
                self.last_mouse_pos = *position;
            }
            WindowEvent::Touch(Touch { id, phase: TouchPhase::Ended | TouchPhase::Cancelled, .. })
                if consumed =>
            {
                self.touches.remove(id);
                self.touch_start = None;
                self.gesture_on_ui = false;
                self.pinch_start_distance = None;
                self.pinch_centroid = None;
            }
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
            }
            _ => {}
        }

        if consumed
            && !matches!(
                event,
                WindowEvent::RedrawRequested
                    | WindowEvent::Resized(_)
                    | WindowEvent::CloseRequested
                    | WindowEvent::ScaleFactorChanged { .. }
            )
        {
            state.mark_dirty();
            return;
        }

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
                self.frame_count = self.frame_count.saturating_add(1);
            }

            // The keyboard, for a plotter at a chart table. Text fields take
            // their keys first (egui consumes them above), so typing a
            // position never pans the chart.
            //   arrows         pan            Shift+Up/Down  tilt
            //   + / -          zoom           0              plan view
            //   C              follow boat    Esc            back out
            WindowEvent::KeyboardInput {
                event: KeyEvent { logical_key, state: ElementState::Pressed, .. },
                ..
            } => {
                const STEP: f32 = 5.0_f32 * std::f32::consts::PI / 180.0;
                let shift = self.modifiers.shift_key();
                // A tenth of the window per press.
                let (w, h) = (state.camera.viewport_width, state.camera.viewport_height);
                let (pan_x, pan_y) = (w * 0.1, h * 0.1);
                let pan = |state: &mut RenderState, dx: f32, dy: f32| {
                    state.release_follow();
                    state.camera.pan(dx, dy);
                };
                let changed = match logical_key.as_ref() {
                    Key::Named(NamedKey::ArrowUp) if shift => {
                        state.camera.tilt = (state.camera.tilt + STEP).min(navcore2::render::MAX_TILT);
                        true
                    }
                    Key::Named(NamedKey::ArrowDown) if shift => {
                        state.camera.tilt = (state.camera.tilt - STEP).max(0.0);
                        true
                    }
                    Key::Named(NamedKey::ArrowUp) => {
                        pan(state, 0.0, pan_y);
                        true
                    }
                    Key::Named(NamedKey::ArrowDown) => {
                        pan(state, 0.0, -pan_y);
                        true
                    }
                    Key::Named(NamedKey::ArrowLeft) => {
                        pan(state, pan_x, 0.0);
                        true
                    }
                    Key::Named(NamedKey::ArrowRight) => {
                        pan(state, -pan_x, 0.0);
                        true
                    }
                    Key::Character("+" | "=") => {
                        state.camera.zoom_at(1.5, w * 0.5, h * 0.5);
                        true
                    }
                    Key::Character("-" | "_") => {
                        state.camera.zoom_at(1.0 / 1.5, w * 0.5, h * 0.5);
                        true
                    }
                    Key::Character("0") => {
                        state.camera.tilt = 0.0;
                        true
                    }
                    Key::Character("c" | "C") => {
                        state.set_follow(true);
                        true
                    }
                    Key::Named(NamedKey::Escape) => state.escape(),
                    _ => false,
                };
                if changed {
                    state.mark_dirty();
                }
            }

            WindowEvent::MouseInput { state: btn_state, button, .. } => {
                if button == MouseButton::Left {
                    let pressed = btn_state == ElementState::Pressed;
                    if pressed {
                        self.gesture_on_ui = state.ui_pointer_over();
                        self.drag_start = Some(self.last_mouse_pos);
                        self.panning = false;
                    } else if self.gesture_on_ui {
                        self.gesture_on_ui = false;
                        self.drag_start = None;
                        self.mouse_pressed = false;
                        return;
                    } else if let Some(start) = self.drag_start.take() {
                        // A click identifies what is under it; a drag pans. The
                        // two arrive as the same pair of events, so they are
                        // told apart by how far the pointer moved in between.
                        let moved = ((self.last_mouse_pos.x - start.x).powi(2)
                            + (self.last_mouse_pos.y - start.y).powi(2))
                        .sqrt();
                        if moved <= CLICK_SLOP * state.window().scale_factor() {
                            let (x, y) = (self.last_mouse_pos.x as f32, self.last_mouse_pos.y as f32);
                            state.tap_at_screen(x, y);
                        }
                    }
                    self.mouse_pressed = pressed;
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                if self.mouse_pressed && !self.gesture_on_ui {
                    // Only once the hand has clearly moved. Panning from the
                    // first pixel means a click drags the chart a little, which
                    // reads as the map slipping the moment you tap it.
                    if !self.panning {
                        if let Some(start) = self.drag_start {
                            let moved = ((position.x - start.x).powi(2)
                                + (position.y - start.y).powi(2))
                            .sqrt();
                            self.panning = moved > CLICK_SLOP * state.window().scale_factor();
                        }
                    }
                    if self.panning {
                        // Taking the chart in hand takes it off follow, or
                        // the next fix would undo the drag.
                        state.release_follow();
                        let dx = (position.x - self.last_mouse_pos.x) as f32;
                        let dy = (position.y - self.last_mouse_pos.y) as f32;
                        state.camera.pan(dx, dy);
                        state.mark_dirty();
                    }
                }
                self.last_mouse_pos = position;
            }

            // A wheel zooms. A trackpad's two-finger swipe arrives as pixel
            // deltas and pans, as it does in every Mac map; Ctrl or Cmd held
            // makes it zoom instead. (winit's PanGesture never fires on macOS,
            // so the swipe has to be read here.)
            WindowEvent::MouseWheel { delta, .. } => {
                let (ax, ay) = zoom_anchor(state, self.last_mouse_pos);
                match delta {
                    MouseScrollDelta::LineDelta(_, y) => {
                        state.camera.zoom_by_wheel(y, ax, ay);
                    }
                    MouseScrollDelta::PixelDelta(pos)
                        if self.modifiers.control_key() || self.modifiers.super_key() =>
                    {
                        state.camera.zoom_by_wheel(pos.y as f32 / 50.0, ax, ay);
                    }
                    MouseScrollDelta::PixelDelta(pos) => {
                        state.release_follow();
                        state.camera.pan(pos.x as f32, pos.y as f32);
                    }
                }
                state.mark_dirty();
            }

            // macOS trackpad gestures
            WindowEvent::PinchGesture { delta, .. } => {
                // delta is additive: positive = zoom in
                let factor = 1.0 + delta as f32;
                let (ax, ay) = zoom_anchor(state, self.last_mouse_pos);
                state.camera.zoom_at(factor, ax, ay);
                state.mark_dirty();
            }

            WindowEvent::PanGesture { delta, .. } => {
                state.release_follow();
                state.camera.pan(delta.x, delta.y);
                state.mark_dirty();
            }

            // Raw touch events (Pi touchscreen)
            WindowEvent::Touch(Touch { id, phase, location, .. }) => {
                match phase {
                    TouchPhase::Started => {
                        self.touches.insert(id, location);
                        // Where a single finger went down, so a tap can be told
                        // from a pan on release.
                        if self.touches.len() == 1 {
                            self.touch_start = Some(location);
                            // At the finger, not egui's hover: a touch screen
                            // has no hover, so egui forgets the pointer
                            // between touches and would answer "not over the
                            // interface" for every tap on a panel.
                            self.gesture_on_ui =
                                state.ui_covers(location.x as f32, location.y as f32);
                            self.panning = false;
                        } else {
                            self.touch_start = None;
                        }
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
                                    if state.follow_on() {
                                        // Following: zoom about the boat,
                                        // which is where the chart keeps her.
                                        let (w, h) = (
                                            state.camera.viewport_width,
                                            state.camera.viewport_height,
                                        );
                                        state.camera.zoom_at(factor, w * 0.5, h * 0.5);
                                    } else {
                                        state.camera.zoom_at(factor, cx, cy);
                                        // Two fingers move the chart as well
                                        // as scale it, as on any phone map.
                                        if let Some((px, py)) = self.pinch_centroid {
                                            state.camera.pan(cx - px, cy - py);
                                        }
                                    }
                                    state.mark_dirty();
                                }
                            }
                            self.pinch_start_distance = Some(new_distance);
                            self.pinch_centroid = Some((cx, cy));
                        } else if self.touches.len() == 1 && !self.gesture_on_ui {
                            // Single-finger pan, once it is clearly a drag.
                            if !self.panning {
                                if let Some(start) = self.touch_start {
                                    let moved = ((location.x - start.x).powi(2)
                                        + (location.y - start.y).powi(2))
                                    .sqrt();
                                    self.panning = moved > TOUCH_SLOP * state.window().scale_factor();
                                }
                            }
                            if let (true, Some(old_loc)) = (self.panning, old_location) {
                                state.release_follow();
                                let dx = (location.x - old_loc.x) as f32;
                                let dy = (location.y - old_loc.y) as f32;
                                state.camera.pan(dx, dy);
                                state.mark_dirty();
                            }
                        }
                    }
                    TouchPhase::Ended | TouchPhase::Cancelled => {
                        // A tap identifies what is under the finger; a drag
                        // pans. Ten pixels of slop, because a finger on glass
                        // never lifts from exactly where it landed.
                        if phase == TouchPhase::Ended {
                            if let Some(start) = self.touch_start.take() {
                                let moved = ((location.x - start.x).powi(2)
                                    + (location.y - start.y).powi(2))
                                .sqrt();
                                if moved <= TOUCH_SLOP * state.window().scale_factor()
                                    && self.touches.len() == 1
                                    && !self.gesture_on_ui
                                {
                                    state.tap_at_screen(location.x as f32, location.y as f32);
                                }
                            }
                        }
                        self.touches.remove(&id);
                        self.pinch_start_distance = None;
                        self.pinch_centroid = None;
                        // The finger left behind by a pinch carries on panning
                        // straight away, rather than having to be lifted and
                        // put down again. It can no longer be a tap.
                        self.touch_start = None;
                        if self.touches.len() == 1 && !self.gesture_on_ui {
                            self.panning = true;
                        } else {
                            self.gesture_on_ui = false;
                        }
                    }
                }
            }

            _ => {}
        }
    }
}
