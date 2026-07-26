# Multi‑Chart Rendering: Reference Projects vs NavCore2 (Detailed Analysis)
Date: 2025‑12‑14  
Scope: Why multi‑chart directory/tile mode has been fragile in NavCore2, what OpenCPN/QuteNav/Nautograf do differently, and what NavCore2 is missing.

## 1) Executive Summary

OpenCPN, QuteNav, and Nautograf all solve the “multiple overlapping charts” problem by treating **coverage and scale selection** as a first‑class step, not as a side effect of drawing “everything that intersects”.

Across all three reference projects:

- **Chart selection is not “all charts intersecting the viewport/tile”.** It is “select the smallest set of charts needed to cover the view area”, strongly guided by **scale** and **coverage geometry** (COVR/NOCOVR polygons).
- **Stitching is not by simply drawing everything over everything.** They prevent holes and avoid wrong underlays using **coverage subtraction** and/or a **coverage ratio threshold**.
- **Decoding/decryption is typically streamed** (especially for encrypted OESU/OESENC), and “header vs full geometry” is explicitly separated.

In NavCore2, the implementation has historically lacked two things that the reference projects rely on:

1) A robust **coverage‑driven selection** strategy (avoid “22 charts per tile”, avoid “random cap causes holes”).  
2) Correct and consistent interpretation of **coverage extents** (WGS84 vs SM vs Mercator), particularly for spatial prefiltering.

As a result, NavCore2 can:

- Spend time decrypting/parsing charts that should never be drawn at the current zoom.
- Over‑compose too many charts, causing large tiles, cache churn, and long “time‑to‑first‑paint”.
- Produce empty tiles when prefiltering discards everything (often because the bbox/extent is interpreted in the wrong coordinate system).

## 2) What “multi‑chart” actually means (and why it’s tricky)

Nautical chart sets are not a single seamless layer:

- Charts overlap extensively across scales (harbor → approach → coastal → general).
- Many charts include **no‑coverage regions** (NOCOVR) which explicitly represent “do not show underlying chart here”.
- Many charts include **coverage regions** (COVR / M_COVR) which represent “this area is valid coverage”.
- Some charts are “overlay” style (e.g., special purpose layers), which should be drawn on top and should not count toward coverage.

Therefore, “stitching together” is not geometric vertex stitching. It is:

- Selecting which charts apply where (by scale and region).
- Masking/ordering so the correct chart is visible in overlaps.

## 2.1) A key vocabulary alignment (COVR vs NOCOVR vs “extent”)

All three reference projects rely on *at least two* geometric layers per chart:

- **Coarse extent**: a simple SW/NE rectangle used as a fast prefilter (cheap).
- **Coverage region(s)** (COVR / M_COVR): polygon(s) describing where the chart is valid (expensive, but critical for correctness).
- **No‑coverage region(s)** (NOCOVR): polygon(s) describing holes/exclusions where underlying charts must show (critical).

This matters because:

- “Extent intersects viewport/tile” is *necessary but not sufficient* for chart selection.
- A chart can intersect the viewport but cover only a small fraction of it.
- A chart can cover the viewport but declare a NOCOVR island where it must not hide the background.

## 3) Reference Project: QuteNav

### 3.1 Data model: outlines + coverage stored as polygons

QuteNav builds/uses a chart database which contains:

- chart extents (SW/NE)
- scale
- coverage polygons and no‑coverage polygons

The `ChartCover` abstraction has:

- `coverage()` and `nocoverage()` (WGS84 polygon sets)
- methods to build **outer** and **inner** regions in the current projection

Evidence:

- Coverage fields are explicit: `doc/reference projects/qutenav/src/chartcover.h` (coverage/nocoverage accessors).
- Coverage is stored in DB tables: `doc/reference projects/qutenav/qutenavlib/src/chartdatabase.cpp`.

### 3.2 Chart selection algorithm: “subtract remaining area until covered”

Core selection is in `ChartManager::updateCharts()` and `ChartManager::findCharts()`:

- Compute a **view area** larger than the viewport (hysteresis):  
  `viewportFactor = 1.6`, `marginFactor = 1.08`  
  (so small pans don’t constantly reload charts).
- Determine candidate scales near the target camera scale using a ratio constraint:  
  `maxScaleRatio = 25`, `maxScale = 8000001`.
- Maintain a `remainingArea` region (initially the view area).
- Query charts by scale and coarse extent, then for each candidate:
  - Convert chart cover to projected regions.
  - Intersect with view area.
  - Subtract the chart’s **inner coverage** from `remainingArea`.
  - Track coverage `cov = 1 - remainingArea.area()/totarea`.
  - Stop once `cov >= minCoverage` where `minCoverage = 0.98`.

Evidence:

- `minCoverage = .98` and scale rules: `doc/reference projects/qutenav/src/chartmanager.h`.
- The subtraction loop: `doc/reference projects/qutenav/src/chartmanager.cpp` (see `findCharts()` and the `remainingArea -= delta` logic).

### 3.3 Background fill for uncovered regions

If coverage is insufficient (`cov < minCoverage`), QuteNav optionally fills remaining areas with background sources (e.g. GSHHS) using `createBackground()`.

Evidence:

- `createBackground(regions, ...)` called when `cov < minCoverage`: `doc/reference projects/qutenav/src/chartmanager.cpp`.

### 3.4 Decryption/decoding strategy: explicit “ReadHeader” vs “ReadSENC”

Encrypted `.oesu` is handled by a reader which talks to `oexserverd` over a pipe:

- `ReadHeader` mode to read only header/outlines/projection.
- `ReadSENC` mode to read full geometry only when needed.

Evidence:

- `OesuDevice device(path, ReadMode::ReadHeader)` for header/outlines:  
  `doc/reference projects/qutenav/oesureader/src/oesureader.cpp`.
- Mode command mapping (header vs SENC):  
  `doc/reference projects/qutenav/qutenavlib/src/ochelper.cpp` (ReadHeader→cmd 9, ReadSENC→cmd 8).

### 3.5 Stitching model

QuteNav does not “stitch vertices”. It stitches by:

- Selecting chart regions which cover the view.
- Ordering charts by priority.
- Rendering each chart proxy clipped to its region (via chart proxy lifecycle and GL).

## 4) Reference Project: Nautograf

Nautograf is the closest conceptual match to a “tile” pipeline, but its critical difference is **chart selection and coverage‑based early stop**.

### 4.1 Tile generation and selection is scale‑aware

Nautograf’s `TileFactory`:

- Computes tiles covering the viewport using WebMercator tiles (max tile size 1024).
- Determines “source candidates” (charts) for the viewport tile using:
  - `extent().intersects(rect)` coarse filter
  - a scale gating rule to avoid too‑detailed charts when zoomed out.

Evidence:

- `sourceCandidates()` in `doc/reference projects/nautograf/src/tilefactory/tilefactory.cpp`:
  - `scaleActual = 52246 / (pixelsPerLon / 2560 * 0.6)`
  - `if (scaleActual / 4 > tileSource->scale() && next exists) continue;`

### 4.2 Coverage ratio early stop (0.98)

When building data for a tile, Nautograf accumulates coverage and stops once it’s “good enough”:

- `CoverageRatio coverageRatio(rect);`
- After each chart tile, `coverageRatio.accumulate(tileData->coverage())`
- Break when `coverageRatio.ratio() >= 0.98`

Evidence:

- `coverageAccpetanceThreshold = 0.98f` and loop: `doc/reference projects/nautograf/src/tilefactory/tilefactory.cpp`.

This is the key practical insight:

> Nautograf does not try to stack “all charts intersecting the tile”. It stacks “just enough charts to cover the tile at the current scale”.

### 4.3 Disk caching and “decode once, clip many”

Nautograf also avoids re‑decrypting/re‑parsing the full chart every tile:

- It reads/decrypts a chart and converts it once into an internal format (Cap’n Proto).
- Per tile, it clips from that internal format and writes tile results to disk.

Evidence:

- `convertChartToInternalFormat(...)` reads chart fully once and writes internal chart file:  
  `doc/reference projects/nautograf/src/tilefactory/oesenctilesource.cpp`.
- `create(...)` returns cached tile if present; otherwise generates and writes it.

### 4.4 Decryption strategy: server stream (not “decrypt to Vec<u8>”)

Nautograf opens encrypted charts through server streams:

- `ServerReader::openOesu(pipeName, path, key)` and `ChartFile.readHeaders()`
- only one stream reading from oexserverd at a time (asserted)

Evidence:

- `doc/reference projects/nautograf/src/tilefactory/catalog.cpp` (`openChart()` and `readHeaders()`).

### 4.5 Stitching model

Nautograf stitches by:

- scale‑selecting a chart subset
- accumulating coverage to avoid gaps
- returning the selected chart tiles in reversed order (`rbegin`→`rend`) for correct layering

It is not vertex stitching.

## 5) Reference Project: OpenCPN

OpenCPN’s multi‑chart mode is “Quilting”.

### 5.1 Quilt candidates are region‑based, not rectangle‑based

OpenCPN computes a candidate region for each chart:

- Uses **aux ply tables** when available (fine coverage).
- Falls back to reduced ply points.
- Subtracts **NoCovr** (NOCOVR) regions, with heuristics to avoid pathological performance.

Evidence:

- Candidate region building and NOCOVR subtraction in `doc/reference projects/OpenCPN/gui/src/Quilt.cpp` (see `QuiltCandidate::GetCandidateRegion()`).
- Explicit perf limits: `NOCOVR_PLY_PERF_LIMIT` / `AUX_PLY_PERF_LIMIT` in `doc/reference projects/OpenCPN/gui/src/Quilt.cpp`.

### 5.2 Quilt composition

OpenCPN builds a set of “patches” that cover the viewport:

- Filters candidates by projection, skew, scale, and chart family rules.
- Composes a quilt by iteratively subtracting already‑covered regions and building patches.
- Tracks overlays separately so overlays don’t “consume” coverage area.

Evidence:

- Quilt logic is extensive in `doc/reference projects/OpenCPN/gui/src/Quilt.cpp` (look for “patches”, “overlay cells”, and “subtract from quilt coverage” comments).

### 5.3 Scale selection rules

OpenCPN’s quilt candidate list is strongly scale‑sorted and contains rules for how the reference chart changes when zooming.

Evidence:

- `CompareScales` / `CompareQuiltCandidateScales`: `doc/reference projects/OpenCPN/gui/src/Quilt.cpp`.

### 5.4 Decryption/decoding approach

OpenCPN’s core parses SENC as a stream from disk, with a clear “ingest header” vs “ingest full geometry” separation:

- `Osenc::ingestHeader(...)` loads enough metadata/coverage tables to drive chart database/quilt decisions.
- `Osenc::ingest200(...)` reads and builds full feature geometry for rendering.

Encrypted chart decryption is not done in core OpenCPN (it is handled by the o‑charts plugin + `oexserverd` externally), which means OpenCPN itself largely interacts with *decrypted OSENC/SENC streams*.

Evidence:

- Streaming SENC ingest: `doc/reference projects/OpenCPN/gui/src/Osenc.cpp`.

## 6) Decryption & caching across reference projects (what they *don’t* do)

The common misunderstanding is “they must decrypt everything up front”. In practice:

- They separate **catalog/selection data** from **render geometry**.
- They use **streaming from oexserverd** so they can read “just header/outlines” without fully materializing the file.
- They rely heavily on **caching** (in-memory proxies and/or on-disk intermediate formats).

Concrete comparisons:

- **QuteNav**: opens `.oesu` via `oexserverd`, explicitly using `ReadHeader` for outline/projection and only using `ReadSENC` for selected charts in view.
  - See `doc/reference projects/qutenav/oesureader/src/oesureader.cpp` and `doc/reference projects/qutenav/qutenavlib/src/ochelper.cpp`.
- **Nautograf**: opens charts via `oexserverd` as `std::istream` streams, reads headers to build catalog, then converts charts to an internal format and persists per-tile caches to disk.
  - See `doc/reference projects/nautograf/src/tilefactory/catalog.cpp` and `doc/reference projects/nautograf/src/tilefactory/oesenctilesource.cpp`.
- **OpenCPN**: reads already-produced SENC/OSENC files from disk (streaming); encrypted chart handling is delegated to a plugin/tooling layer.
  - See `doc/reference projects/OpenCPN/gui/src/Osenc.cpp` and `doc/reference projects/OpenCPN/gui/src/Quilt.cpp`.

## 6) NavCore2: Current Multi‑Chart Rendering Model

### 6.1 Intended architecture (tile system)

NavCore2 aims for:

- WebMercator z/x/y tiles (256px)
- Catalog of chart extents
- TileBuilder builds per‑tile geometry packets and caches on GPU

### 6.2 What NavCore2 actually does today (directory mode)

1) **Catalog build**
   - Enumerates `.oesu` files.
   - Decrypts each file into memory (`Vec<u8>`) and parses header (today there is no “ReadHeader stream” equivalent).
   - Computes `extent_mercator` from WGS84 extent corners.

2) **Per frame**
   - Computes a zoom `z` from camera `meters_per_pixel`.
   - Computes all visible tiles in view bounds (`visible_tiles`), often hundreds (e.g. `visible_tiles=494` at z=9).
   - Builds only `MAX_TILES_PER_FRAME` tiles (currently 4), then draws whatever is in cache.

3) **Per tile build**
   - `charts_for_tile()` uses chart header extents (WGS84‑>Mercator) for coarse intersection.
   - Loads charts on demand (decrypt + parse full SENC) into an in‑memory cache.
   - For each feature:
     - Spatial prefilter using feature geometry bbox (`geom.extent`) against the tile.
     - Clip triangles/lines to tile bounds.

4) **Render**
   - Draws only area triangles in tile mode (lines/symbols/soundings are not integrated in tile rendering yet).

### 6.3 Critical divergence from reference projects: “selection”

NavCore2 currently treats “tile intersects chart extent” as the selection rule, and then:

- either processes *all* intersecting charts, or
- uses a hard cap (`MAX_CHARTS_PER_TILE`) which can create holes by arbitrarily excluding charts.

This is fundamentally different from:

- QuteNav: subtract‑coverage selection until 98% coverage, scale‑bounded candidates.
- Nautograf: scale‑bounded candidates + coverage ratio until 98%.
- OpenCPN: quilt patches + NOCOVR subtraction + overlay handling.

## 7) Why NavCore2 still struggles where the reference projects don’t

### 7.1 Missing: coverage‑driven chart selection (the biggest “devil in the details”)

Symptoms in NavCore2:

- A tile can “intersect 22 charts” at a mid zoom. Some of those charts may be:
  - too detailed for the current view,
  - partial coverage,
  - intended overlays,
  - or have NOCOVR exclusions.
- If you draw all of them, you overwork the CPU/GPU and risk churn.
- If you cap them arbitrarily, you get holes or missing detail.

The reference projects solve this by a deterministic selection process:

- Prefer a target scale (or a scale bracket).
- Add charts until coverage is good enough (≈98%).
- Do not let overlays consume coverage.
- Subtract NOCOVR from candidate regions.

NavCore2 currently does none of these at selection time.

### 7.2 Missing: NOCOVR/coverage masking

Even if NavCore2 draws something, it can be “wrong” in overlaps:

- Without NOCOVR subtraction, underlying charts can appear in regions where the foreground chart explicitly says “no coverage”.
- Without a quilt‑like region map, you can’t cleanly assign “this pixel belongs to chart X”.

The reference projects’ “stitching” is mostly region logic, not geometry logic.

### 7.3 Too many visible tiles + too few built tiles per frame

NavCore2 can have `visible_tiles=494` at z=9 while building only 4 tiles per frame.
This makes first‑paint slow and produces large ocean areas until enough tiles are cached.

Reference projects mitigate thrash via:

- QuteNav: viewportFactor/marginFactor hysteresis and asynchronous chart proxy management.
- Nautograf: persistent on‑disk tile caches and internal caches per chart.
- OpenCPN: quilt recomposition rules and caching of SENC/regions.

### 7.4 Parsing differences (non‑selection correctness)

NavCore2 must interpret record payloads exactly like OSENC:

- OSENC geometry record extents are WGS84 degrees (as in QuteNav’s `osenc.h`).
- Multipoint soundings payload is `3×f32` per point (as in QuteNav).

Any mismatch here causes silent “feature filtered out” or parse errors, which then reduces coverage and makes tiles appear empty.

## 7.5 The “why is nothing visible” failure mode (what the logs mean)

Based on the observed debug output:

- `Found 22 charts for tile` confirms the *catalog/extent* prefilter is working.
- `Chart : ... area features, 0 intersecting, 0 triangles clipped` indicates the *per-feature spatial prefilter* rejects everything, so no geometry reaches clipping.

In reference-project terms, this is equivalent to “every feature is outside the tile”, which is almost always a **coordinate system semantic bug** (units mismatch), not a “render pipeline” bug:

- If the feature extents are in **WGS84 degrees**, but the code treats them as **SM meters**, then converting them to Mercator meters by adding `(ref_mx, ref_my)` produces nonsense and intersection always fails.
- QuteNav’s OSENC struct layout documents that these extents are in degrees for area/line geometry records (see `doc/reference projects/qutenav/qutenavlib/src/osenc.h`).

This class of bug does not exist in the reference projects because they carry WGS84 degrees for coverage and extents up to the point where they explicitly project them.

## 8) How the reference projects select charts (concise comparison)

| Project | Unit of selection | Scale gating | Coverage gating | NOCOVR handling | Overlays |
|---|---|---:|---:|---:|---:|
| QuteNav | Region map over view area | Yes (`maxScaleRatio`, `maxScale`) | Yes (`minCoverage=0.98`) | Yes (inner/outer regions derived from cov/nocov) | Yes (handled separately) |
| Nautograf | Per‑tile chart list | Yes (skip too‑detailed when zoomed out) | Yes (`coverageRatio >= 0.98`) | Indirectly via stored coverages | Not fully clear here, but coverage prevents over‑stacking |
| OpenCPN | Quilt patches | Yes (sorted candidates + ref chart rules) | Yes (patch composition fills view) | Yes (subtract NOCOVR with perf limits) | Yes (overlay pass) |
| NavCore2 | “all charts intersecting tile bbox” | No (except manual cap) | No | No | No |

## 9) Implications for NavCore2’s tile approach

NavCore2’s tile clipping approach can work, but to match reference correctness it needs:

1) **A real chart selection layer** that is scale‑aware and coverage‑aware.  
2) **A region/coverage model** (COVR/NOCOVR) to prevent holes and incorrect underlays.  
3) A clear policy for overlaps:
   - background charts (small scale) fill gaps,
   - detail charts override within their coverage,
   - overlays draw last and do not count toward “coverage fill”.

## 10) Recommended Next Debug/Validation Steps (no code changes described)

These are “confirm the pipeline is correct” steps which mirror the reference projects’ mental model:

### 10.1 A deterministic “pipeline invariant” checklist

This is the shortest path to finding “where geometry disappears”. Each item is a yes/no invariant with the most likely root cause if it fails.

1) **Viewport -> tile set**
   - Invariant: `visible_tiles > 0`, `z` is stable when standing still.
   - If fails: camera zoom math or Mercator bounds math is wrong.

2) **Tile -> chart candidates**
   - Invariant: for a tile inside the catalog combined extent, `Found K charts for tile` yields `K > 0`.
   - If fails: chart header extents are wrong (WGS84 -> Mercator conversion), or tile math/axis inversion is wrong.

3) **Chart candidates -> selected charts**
   - Invariant: selection step is deterministic and scale‑aware (even if initial version is crude).
   - If fails: you’ll see excessive `K` (e.g. 20–100) and first‑paint stalls; a hard cap will create holes.

4) **Selected chart -> per-feature prefilter**
   - Invariant: for at least one chart in a tile that is known to contain land/depth, `intersecting > 0`.
   - If fails: per-feature bbox semantics mismatch (WGS84 vs SM vs Mercator), or bbox is uninitialized/garbled due to record parsing.

5) **Per-feature -> clipping**
   - Invariant: `triangles clipped > 0` for at least some tiles at mid zoom.
   - If fails: triangle iteration logic or the clipper rejects everything (often because bounds are in different units than vertices).

6) **CPU packet -> GPU cache**
   - Invariant: cached tile shows `area verts > 0` (or line verts later) and is not immediately evicted.
   - If fails: cache key mismatch, byte accounting bug, or “skip upload” logic mistakenly triggers.

7) **GPU cache -> draw**
   - Invariant: `Drew N tiles` where `N > 0` for a view that overlaps chart coverage.
   - If fails: render loop chooses wrong path (single vs tile), pipeline mismatch (vertex layout/shader), or bind groups/uniforms are not set for tile draw.

### 10.2 Symptom -> cause -> next place to look

| Symptom in logs | Most likely cause | Next file(s) to inspect |
|---|---|---|
| `Found 0 charts for tile` for tiles clearly within the catalog extent | tile bounds mismatch (y inversion, clamp) or catalog extents wrong | `src/tiles/mod.rs`, `src/senc/catalog.rs` |
| `Found K charts` but `0 intersecting` for every chart | per-feature bbox semantics wrong | `src/senc/geometry.rs` (extent parsing + intersects), `doc/reference projects/qutenav/qutenavlib/src/osenc.h` |
| `intersecting > 0` but `triangles clipped = 0` | clipper unit mismatch or triangle iteration bug | `src/tiles/clip.rs`, `src/senc/geometry.rs` |
| `packet has verts` but cache shows `0 area verts` | upload path bug or vertex layout mismatch | `src/tiles/cache.rs`, `src/render/state.rs` |
| cache shows `area verts > 0` but screen is ocean only | render uses wrong pipeline/uniforms or camera space mismatch | `src/render/state.rs`, `src/render/camera.rs`, shaders |

1) **Validate per‑feature bbox semantics**
   - Confirm that for a known chart, a known tile, `geom.extent` in WGS84 intersects expected tiles.
   - Confirm that clipped triangles exist for those features.

2) **Validate coverage objects**
   - Identify whether M_COVR / COVR / NOCOVR features are present in NavCore2’s parsed feature list.
   - If present, verify they can be extracted into WGS84 polygons (like QuteNav/Nautograf do).

3) **Measure “charts intersecting per tile” distribution**
   - If many tiles have > 20 intersecting charts at mid zoom, selection must be improved (don’t hard cap randomly).

4) **Define a deterministic selection rule**
   - Mimic Nautograf: pick candidates by intersection and scale, then accumulate coverage until ≥0.98.
   - Mimic QuteNav/OpenCPN: subtract remaining area and stop at ≥0.98.

5) **Only after selection is correct: optimize decryption**
   - Adopt QuteNav‑style `ReadHeader` vs `ReadSENC` streaming to avoid decrypting whole files for catalog scanning.
   - Consider optional disk caching (Nautograf/OpenCPN pattern) once correctness is proven.

## 11) Key Takeaway

The reference projects “have no problem” because they **do not attempt to render every overlapping chart**. They:

- pick the right scales,
- use coverage regions to decide where each chart applies,
- and stop once the view is sufficiently covered.

NavCore2’s tile engine will continue to be fragile until it adopts a similarly explicit selection model.
