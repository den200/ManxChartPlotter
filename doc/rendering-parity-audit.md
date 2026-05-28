# Rendering Parity Audit: NavCore2 vs. Reference Projects (OpenCPN, QuteNav)

## Executive Summary
This audit evaluated the NavCore2 chart plotter rendering engine against OpenCPN and QuteNav to pinpoint why NavCore2’s visual output does not yet match the reference projects. 

### Why the Gap Exists: The Architectural Mismatch
The profound differences between NavCore2 and OpenCPN stem from a fundamental architectural divergence between the **S-52 standard's assumptions** (immediate-mode, global CPU rendering) and **NavCore2's design** (retained-mode, tiled GPU rendering via WGPU):

1. **Global Viewport vs. Local Tiles**: S-52 assumes the entire screen is drawn at once. OpenCPN uses a "Painter's Algorithm", looping through priorities 0-9 globally. NavCore2 processes and batches geometry into isolated geographic tiles for performance. This causes severe Z-fighting across tile seams (a high-priority symbol in Tile A is drawn over by a low-priority background in Tile B) and breaks continuous elements like text labels and dashed boundary lines at tile edges.
2. **CPU Procedural vs. GPU Static Baking**: S-52 styling heavily relies on runtime procedural evaluation (e.g., measuring exact millimeters for line width based on current DPI, or continuous scaling for SCAMIN). OpenCPN does this in C++ per-frame. NavCore2 attempts to "bake" these dynamic properties (pixel widths, static RGBA colors, binary SCAMIN visibility) into static WGPU vertex buffers during a background tile-build step. This makes NavCore2 unable to react instantly to Day/Night mode swaps or smooth zooming without expensive rebuilds.
3. **Robustness of Geometry Processing**: S-57 polygons (like coastlines) often contain extreme edge cases, self-intersections, and complex nested holes. OpenCPN relies on the battle-tested, CPU-heavy `GLU Tesselator`. NavCore2 uses a simpler, custom ear-clipping triangulator that frequently fails on these anomalies, leaving visual artifacts (slivers, missing landmasses).
4. **Decades of Edge-Case Handling**: OpenCPN (`s52plib.cpp`) represents 15+ years of accumulated fixes for badly authored ENCs and exhaustive implementation of the 6,000+ line S-52 Conditional Symbology (CS) spec. NavCore2 is building from scratch, relying on a manually ported, incomplete subset of CS rules (missing critical procedures like `DATCVR01`).

*   **S-52 Procedural Gaps**: NavCore2 lacks exhaustive implementation of complex Conditional Symbology (CS) procedures (e.g., `DATCVR01`, `SYMINS01`), leading to dropped coverage boundaries and over-simplified obstruction/depth area styling.
*   **Layering and Z-Fighting**: NavCore2 sorts geometry by priority *per tile* without utilizing a global Z-buffer. This causes Z-fighting and incorrect overlapping across tile seams, whereas OpenCPN uses a strict 10-pass Painter's algorithm (`ObjRazRules`).
*   **Line Scaling Precision**: NavCore2 calculates line widths natively in pixels (`width_px`). S-52 standards dictate 0.3mm units, which OpenCPN honors dynamically using continuous scaling (`glLineWidth` factored by DPI/ppmm).
*   **Palette Rigidity**: Line and text colors in NavCore2 are resolved and baked into WGPU buffers statically at tile build time. OpenCPN uses dynamic palette lookups at draw time, enabling instant Day/Dusk/Night swaps.
*   **SCAMIN Brittleness**: NavCore2 enforces a binary SCAMIN (Scale Minimum) cutoff. OpenCPN employs a "soft SCAMIN" with zoom modifiers (scaling down to 50%) and "SUPER_SCAMIN" fallbacks, preventing abrupt clutter pop-in/pop-out.
*   **Tile-Bound Decluttering**: NavCore2 declutters and layouts text purely within the bounds of a single tile, which arbitrarily cuts off long labels. OpenCPN evaluates text placement globally.
*   **Triangulation Robustness**: NavCore2 uses a custom Sutherland-Hodgman clipper and ear-clipping triangulator that struggles with complex self-intersecting S-57 holes, whereas OpenCPN relies on standard, robust GLU Tesselation.
*   **LC Pattern Stamping**: NavCore2 caps LC pattern stamps aggressively (`MAX_LC_STAMPS_PER_TILE`), potentially omitting dashed symbols on complex coastlines to save performance, unlike OpenCPN's procedural line following.

---

## Current-State Architecture Map

| Pipeline Stage | NavCore2 (Rust + WGPU) | OpenCPN (C++ + OpenGL / wxDC) | QuteNav (C++ + Qt) |
| :--- | :--- | :--- | :--- |
| **Parsing & Geometry** | AABB pre-filtering, custom ear-clipping & Sutherland-Hodgman per tile (`src/tiles/clip.rs`). | Global view-port bounding box, GLU Tesselator for complex polygons with holes (`s52plib.cpp`). | Qt QPolygon, Qt-native intersection clipping and path rendering. |
| **S-52 Styling** | `S52Engine` lookup with rudimentary, manually ported CS expansion (`src/s52/cs/mod.rs`). | `s52plib.cpp` / `s52cnsy.cpp`: 6000+ lines of exhaustive CS procedural evaluations and recursive LUP lookups. | `s52presentation_p.cpp` with parsed HPGL/instruction interpreters. |
| **Layer Ordering** | WGPU buffers generated per tile, sorted locally by S-52 `disp_prio`. No depth buffer testing. | Multi-pass rendering loops using `ObjRazRules`, iterating through 10 strict display priorities. | Qt Painter Paths, utilizing QPainter's inherent sequential layering. |
| **Labels & Decluttering** | Deferred rendering, two-phase AABB decluttering strictly bound by tile geometry (`src/render/text_layout.rs`). | Screen-space global `CheckTextRectList` tracking, complex font rasterization and caching across view. | Qt-native font rendering, relies primarily on scene graph rendering order. |
| **Palette / Day-Night** | `palette_buffer` updated dynamically for *areas*, but lines/text hardcode RGB via `s52_styles.rs`. | Global palette switching; entire canvas repaints dynamically mapping tokens to RGB per frame. | Global QPalette substitution. |

---

## Parity Gap Table

| Symptom | Likely Cause | NavCore2 Refs | Reference Refs | Fix Steps | Verification |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **Chart Data Coverage boxes missing** | `DATCVR01` is unimplemented. | `src/s52/cs/mod.rs` : `execute_cs` missing branch. | OpenCPN `s52cnsy.cpp:89` | Implement `DATCVR01` to return `LC(HODATA01)` for `M_COVR`. | Zoom out; observe dashed bounding boxes indicating chart limits. |
| **Labels cut off at tile edges** | `TileBuilder::build_areas` clips text to `bounds.min_x/max_x`. | `src/tiles/builder.rs` : `label_candidates` filtered. | OpenCPN global label layout logic avoiding tile constraints. | Defer label layout to a global post-tile pass or allow overflow. | Pan slowly across a long label; text remains whole without truncation. |
| **Z-Fighting on overlapping landmasses** | Tiles draw priority batches locally without depth-testing against adjacent tiles. | `src/render/state.rs` : `draw_tiles()` loops. | OpenCPN `s52plib.cpp` : `RenderS57Objects()` globally sequential loop. | Enable WGPU `DepthStencilState` and inject `disp_prio` into vertex Z-coordinates. | Inspect tile seams; priorities correctly overlap without flickering. |
| **Harsh popping of symbols on zoom** | Binary SCAMIN threshold check without scaling interpolation. | `src/tiles/builder.rs` : `should_render_at_scale_ex()`. | OpenCPN `s52plib.cpp` : `g_scaminScale` modifier. | Implement "soft SCAMIN" multiplier and "SUPER_SCAMIN" fallbacks. | Slowly zoom out; dense soundings gracefully fade/scale before disappearing. |
| **Lines remain Day-colored in Night mode** | Line pipeline relies on static RGBA values baked during tile build. | `src/render/s52_styles.rs` : `style_for_key()`. | OpenCPN `s52plib.cpp` dynamically looks up colors per frame. | Move lines to use `color_index: u32` mapped via `palette_buffer` in `line.wgsl`. | Switch to Night mode; line boundaries dim immediately without rebuild. |

---

## Deep Dives

### 1) S-52 Resolution + Instruction Parsing Parity
*   **NavCore2 Strategy**: Translates standard LUP entries to `RenderInstruction` via `src/s52/engine.rs`. Conditional Symbology is routed through `execute_cs` in `src/s52/cs/mod.rs`.
*   **Reference Strategy (OpenCPN)**: `s52plib.cpp` and `s52cnsy.cpp` evaluate S-52 rules completely procedurally. If an object triggers a CS, OpenCPN evaluates 6,000+ lines of fallback logic, generating precise patterns for edge cases. QuteNav utilizes an instruction parser in `s52presentation_p.cpp` (`parseInstruction`).
*   **Root Cause**: NavCore2 has manually implemented only a fraction of necessary CS procedures (`DEPARE02`, `OBSTRN04`, `DEPCNT02`, etc.). Missing procedures like `DATCVR01` (Coverage) and `SYMINS01` (Complex symbol boundaries) cause entirely missing features.

### 2) Layer Ordering and Priority/Depth Strategy
*   **NavCore2 Strategy**: `src/tiles/builder.rs` batches geometry per tile into `TilePacket`s sorted by `LineBatchKey::disp_prio`. When drawn via `src/render/state.rs`, it loops through priorities *per tile*.
*   **Reference Strategy (OpenCPN)**: `s52plib.cpp` parses into `ObjRazRules` and `LUPrec`. The main rendering loop iterates sequentially from priority 0 to 9, drawing *all* objects of that priority across the global viewport before advancing.
*   **Root Cause**: NavCore2's tile-centric approach breaks global draw order. Because WGPU draws tile A (priorities 0-9) then tile B (priorities 0-9) without depth testing, a priority 1 area in tile B can draw over a priority 9 symbol in tile A. 

### 3) Symbol Atlas Mapping + Visibility Thresholds
*   **NavCore2 Strategy**: Uses `should_render_at_scale_ex()` which compares `view_scale` directly against the feature's `scamin` integer.
*   **Reference Strategy (OpenCPN)**: Implements a nuanced SCAMIN algorithm. If a view scale slightly exceeds SCAMIN, the engine applies a "zoom modifier" scaling the symbol down gradually. It also utilizes "SUPER_SCAMIN" fallbacks (e.g., `native_scale * 4`) for poorly authored ENCs.
*   **Root Cause**: The binary cut-off in NavCore2 results in visually jarring "pop-in" behavior, causing screens to transition from empty to overly cluttered in a single zoom tick.

### 4) Line Rendering Differences
*   **NavCore2 Strategy**: Converts S-52 width units statically into pixel widths via `width_scaled` in `src/render/s52_styles.rs`. Complex `LC()` borders generate heavy geometric stamps bound by `MAX_LC_STAMPS_PER_TILE`. Colors are resolved to `[f32; 4]` RGBA floats.
*   **Reference Strategy (OpenCPN)**: Strict adherence to S-52 specs (1 unit = 0.3mm). Dynamically updates `glLineWidth` taking physical screen DPI (`m_GLMinCartographicLineWidth`) into account. Colors are referenced by index.
*   **Root Cause**: NavCore2 bakes static colors into vertex buffers, meaning `line.wgsl` cannot respond to Day/Night mode swaps without discarding and rebuilding the entire tile cache. 

### 5) Area Rendering Differences
*   **NavCore2 Strategy**: Employs an ear-clipping triangulator and a custom Sutherland-Hodgman clipper (`src/tiles/clip.rs`).
*   **Reference Strategy (OpenCPN)**: Maps complex S-57 topologies to standard OpenGL `GLU Tesselator`.
*   **Root Cause**: The custom ear-clipping algorithm struggles with highly complex self-intersecting geometries or multi-layered holes common in coastal ENC data, leaving artifact "slivers" or transparent holes in landmasses.

---

## Instrumentation Plan

To guide and verify the remediation phase, add these non-destructive instrumentation tools:

1.  **Environment Variables**:
    *   `NAVCORE_AUDIT_Z_BUFFER=1`: Override the WGPU fragment shader to output grayscale values representing normalized Z-depth. (Verifies layering fixes).
    *   `NAVCORE_DEBUG_SCAMIN=1`: Print console logs when a feature is culled by SCAMIN, including `view_scale`, `scamin`, and `object_class`.
    *   `NAVCORE_LOG_CS_MISS=1`: Log warnings in `execute_cs()` whenever an unknown CS procedure is encountered (e.g., `DATCVR01`).
2.  **Captures**:
    *   Use **RenderDoc** to inspect the WGPU `DepthStencilState` pipeline configuration. Ensure depth-write is enabled during priority batching.
    *   Generate a golden-image test suite locked to a specific viewport `(lat, lon, zoom)` simulating OpenCPN's Day Bright mode for automated pixel-diffing.

---

## Step-by-Step Fix Plan for Claude Code

> **Rule:** Implement these sequentially. Do not merge steps.

### Phase 1: Correctness Parity

1.  **Implement missing CS Procedures (Coverage & Boundaries)**
    *   *Symptom*: Missing chart bounds and metadata outlines.
    *   *Action*: Add `DATCVR01` to `execute_cs` in `src/s52/cs/mod.rs` which returns a `LineComplex` instruction for `HODATA01`.
    *   *Acceptance*: Render a zoomed-out view; dashed chart boundary rectangles must appear.
2.  **Global Layering via Depth Buffer**
    *   *Symptom*: Z-fighting at tile seams where high-priority symbols are hidden by adjacent low-priority areas.
    *   *Action*: In `src/render/state.rs`, configure `wgpu::DepthStencilState` with `Less` compare. In `chart.wgsl`, `line.wgsl`, and `symbol.wgsl`, map the `disp_prio` (0-9) to a Z-coordinate (`z = 1.0 - (prio / 10.0)`).
    *   *Acceptance*: RenderDoc capture confirms depth writes. Tile seams show perfectly overlapping priorities regardless of WGPU batch draw order.

### Phase 2: Performance & Scaling Parity

3.  **Dynamic Palette Mapping for Lines**
    *   *Symptom*: Lines do not change color when toggling Night mode.
    *   *Action*: Modify `LineVertex` in `src/render/state.rs` to replace `color: [f32; 4]` with `color_index: u32`. Update `line.wgsl` to sample colors from the `palette_buffer` binding.
    *   *Acceptance*: Toggling from Day to Night mode dims coastlines and contours immediately without triggering tile worker rebuilds.
4.  **Soft SCAMIN Interpolation**
    *   *Symptom*: Dense sounding layers pop into existence harshly.
    *   *Action*: In `src/tiles/builder.rs` `should_render_at_scale_ex()`, introduce a continuous scale factor based on OpenCPN's zoom modifier logic. Reduce symbol scale matrix uniformly when `view_scale` is within 25% of the `scamin` threshold.
    *   *Acceptance*: Zooming out smoothly shrinks symbol/text elements before they disappear entirely.

### Phase 3: Text & Lifecycle

5.  **Global Label Decluttering**
    *   *Symptom*: Text labels on tile boundaries are cut in half.
    *   *Action*: Refactor `TileBuilder` to output raw `label_candidates` without AABB culling against `bounds.min_x/max_x`. Move the `declutter_and_layout_labels` invocation to a global frame-level pass in `RenderState::draw_tiles()`.
    *   *Acceptance*: Panning horizontally across a long region name preserves the entire text string without visual truncation.