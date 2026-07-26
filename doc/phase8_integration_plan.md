# Phase-8 PortrayalPipe integration plan (NavCore)

## Integration map
- Feature source: `src/senc/features.rs:240` (`ChartData.features`), geometry types in `src/senc/geometry.rs:164` and `src/senc/geometry.rs:495`.
- Tile build: `src/tiles/builder.rs:302` (`build_cpu`) and `src/tiles/builder.rs:318` (`build_cpu_impl`).
- Render entry: `src/render/state.rs:923` (`RenderState::render`) and tile draw path `src/render/state.rs:1200`.
- Viewport: camera/scale in `src/render/camera.rs:11` and uniforms in `src/render/state.rs:931`; mercator transforms in `src/tiles/mod.rs:228`.
- Style: S-52 lookup/ops in `src/tiles/builder.rs:598`, instruction parsing in `src/s52/instruction.rs:190`, symbol lookup in `src/senc/symbol_lookup.rs:26`, style tables in `src/render/s52_styles.rs:84`.

## Adapter types (PortrayalPipe)
- Target types: `FeatureLine`, `FeatureArea`, `FeaturePoint`, `Attribute`, `AttrValue`, `ViewportParams` in `third_party/portrayalpipe/crates/s52_backend/src/types.rs:123` and `third_party/portrayalpipe/crates/s52_backend/src/types.rs:181`.
- Attributes mapping: `AttributeValue` in `src/senc/features.rs:80` to `AttrValue` in `third_party/portrayalpipe/crates/s52_backend/src/types.rs:164`; `Integer->Int`, `Float->Real`, `String->String`, keep room for list types (not currently emitted by NavCore).
- Lines: build `FeatureLine` from `LineGeometry::resolve_global` in `src/senc/geometry.rs:659`, using object class acronym from `feature.object_class` and attributes from `feature.attributes`.
- Areas: PortrayalPipe expects rings (`FeatureArea.rings`, `third_party/portrayalpipe/crates/s52_backend/src/types.rs:299`), while NavCore has triangles (`AreaGeometry.triangles`, `src/senc/geometry.rs:164`); minimal adapter needs area outlines, so extend `AreaGeometry::parse` in `src/senc/geometry.rs:201` to retain edge refs (currently skipped at `src/senc/geometry.rs:255`) and resolve rings via `EdgeTable` (`src/senc/geometry.rs:678`), then convert to global Mercator with `tiles::sm_to_global` (`src/tiles/mod.rs:236`) before feeding `FeatureArea`.
- Points: build `FeaturePoint` from `PointGeometry` in `src/senc/geometry.rs:403` using `latlon_to_mercator` (`src/tiles/mod.rs:228`) or the existing point conversion logic in `src/tiles/builder.rs:931`.
- Text: PortrayalPipe compiles text from `FeaturePoint` (`compile_text` in `third_party/portrayalpipe/crates/s52_backend/src/engine.rs:608`), so reuse the point feature list and ensure text-bearing attributes are present (`OBJNAM`, `INFORM`, `NINFOM` are already mapped in `src/senc/features.rs:38`).
- Coordinate space: keep global Mercator meters as feature points and let NavCore’s camera do world->screen transform (consistent with `src/tiles/builder.rs:954` and `src/render/state.rs:935`); add `mercator_to_latlon` near `src/tiles/mod.rs:228` to populate `ViewportParams.center_lat/center_lon` for PortrayalPipe.

Implementation locations:
- New adapter module: `src/portrayal/adapter.rs` for `to_feature_lines/areas/points` and `attributes_to_portrayal`.
- Call sites: `src/tiles/builder.rs:318` (inside `build_cpu_impl`) right after `chart` is loaded and before line/area/symbol assembly.

## Tile-time compilation (PortrayalPipe only, cached)
- Current rebuild trigger: missing tile key in `RenderState::build_visible_tiles` (`src/render/state.rs:1069`) and style invalidation through `TileCacheKey` (`src/tiles/cache.rs:11`).
- Proposal: run `compile_lines/areas/symbols/text/pattern_*` once per tile build inside `TileBuilder::build_cpu_impl` (`src/tiles/builder.rs:318`), stash results into `TilePacket` (extend `src/tiles/builder.rs:163`), and upload via `TileGpuCache::upload` (`src/tiles/cache.rs:111`).
- Invalidation: bump `RenderState.style_hash` (`src/render/state.rs:245`) on mariner settings/color-scheme changes so tiles rebuild without per-frame compile.

## Renderer impact (wgpu)
- Lines: current pipeline in `src/render/state.rs:405` with `LineUniforms` (`src/render/state.rs:121`) supports width, dash, color; map PortrayalPipe `LineStyle` to existing uniforms in `draw_tiles` (`src/render/state.rs:1235`), and extend batching key to include `LineStyleKey` style_key from PortrayalPipe.
- Areas: currently already triangulated in `build_areas` (`src/tiles/builder.rs:462`); for PortrayalPipe rings, add tessellation (earcut or fan triangulation) at tile-time, then reuse area pipeline in `src/render/state.rs:364`.
- Symbols: atlas-based instancing in `src/render/symbols.rs:114`; need a PortrayalPipe symbol-name->atlas-id map and per-instance rotation/scale metadata (PortrayalPipe `SymbolOp` includes scale/rotation in `third_party/portrayalpipe/crates/s52_backend_sys/cpp/src/s52_backend.cpp:411`).
- Text: existing renderer is soundings-only (`src/render/text.rs:1`); PortrayalPipe `TextOp` (`third_party/portrayalpipe/crates/s52_backend/src/types.rs:653`) will need a new text renderer or extend `TextRenderer` to accept glyph atlas, font size, halo, and justification.
- Patterns (LC/AP): NavCore already has LC stamping (`src/render/lc_pattern.rs:1`, used in `src/tiles/builder.rs:752`); first integration: pattern-line support by mapping PortrayalPipe `PatternLineOp` to the existing LC stamping pipeline; for pattern areas (AP), start with texture atlas tiling in a dedicated area pattern pipeline.

## Declutter / collision
- Current placement: symbols rendered in `src/render/state.rs:1286` and soundings in `src/render/text.rs:166` with no collision checks.
- Minimal declutter system: add a lightweight grid or quadtree in a new `src/render/declutter.rs`, run it after PortrayalPipe compile and before building instance buffers, ordered by `DeclutterInfo.priority` (`third_party/portrayalpipe/crates/s52_backend/src/types.rs:516`), never suppress `ALWAYS_VISIBLE`, and apply a simple sounding thinning rule (one per grid cell at tile scale, prefer shallow/unsafe values).

## Implementation sequence (10–20 commits)
1) Add PortrayalPipe path dependency and module wiring.
- Files: `Cargo.toml`, `src/lib.rs`.
- Verify: `cargo check`.

2) Add mercator<->lat/lon helpers.
- Files: `src/tiles/mod.rs` (add `mercator_to_latlon`).
- Verify: `cargo test -p navcore2 tiles`.

3) Add portrayal adapter scaffolding.
- Files: `src/portrayal/adapter.rs`, `src/portrayal/mod.rs`.
- Verify: `cargo check`.

4) Extend area geometry parsing to retain edge refs.
- Files: `src/senc/geometry.rs`.
- Verify: `cargo test -p navcore2 senc`.

5) Implement FeatureArea ring conversion.
- Files: `src/portrayal/adapter.rs`, `src/senc/geometry.rs`.
- Verify: `cargo check`.

6) Add PortrayalPipe engine initialization.
- Files: `src/render/state.rs` (store engine and settings), `src/portrayal/mod.rs`.
- Verify: `cargo check`.

7) Extend TilePacket for portrayal outputs.
- Files: `src/tiles/builder.rs`, `src/tiles/cache.rs`.
- Verify: `cargo check`.

8) Integrate tile-time compile in `build_cpu_impl`.
- Files: `src/tiles/builder.rs`, `src/portrayal/adapter.rs`.
- Verify: `cargo run -- --tile-debug <chart_dir>`.

9) Map PortrayalPipe line styles to renderer.
- Files: `src/render/state.rs`, `src/tiles/cache.rs`.
- Verify: `cargo run -- <chart_dir>`.

10) Add tessellation for PortrayalPipe area rings.
- Files: `src/tiles/builder.rs` (tessellator), `src/render/state.rs` (reuse pipeline).
- Verify: `cargo run -- <chart_dir>`.

11) Integrate symbol ops with atlas.
- Files: `src/render/symbols.rs`, `src/tiles/cache.rs`, `src/portrayal/adapter.rs`.
- Verify: `cargo run -- <chart_dir>`.

12) Add text renderer for PortrayalPipe `TextOp`.
- Files: `src/render/text.rs`, `src/render/state.rs`, `src/tiles/cache.rs`.
- Verify: `cargo run -- <chart_dir>`.

13) Pattern line integration.
- Files: `src/tiles/builder.rs`, `src/render/lc_pattern.rs`.
- Verify: `cargo run -- <chart_dir>`.

14) Pattern area integration (AP).
- Files: `src/render/state.rs`, `assets/shaders/*`, `src/tiles/cache.rs`.
- Verify: `cargo run -- <chart_dir>`.

15) Declutter grid.
- Files: `src/render/declutter.rs`, `src/render/state.rs`, `src/tiles/builder.rs`.
- Verify: `cargo run -- <chart_dir>`.

16) Settings-driven invalidation.
- Files: `src/render/state.rs`, `src/tiles/cache.rs`.
- Verify: `cargo run -- <chart_dir>`.

## Minimal demo + verification
- Tile-time compile: `cargo run -- --tile-debug <chart_dir>` (uses `src/bin/navcore.rs:58`).
- Full render: `cargo run -- <chart_dir>` or `cargo run -- <chart.oesu>` (usage in `src/bin/navcore.rs:4`).

