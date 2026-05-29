# NavCore vs OpenCPN — Root Cause and Remediation Plan

**Status:** authoritative. Every claim below is backed by direct code inspection (NavCore file:line) and reference-source inspection (OpenCPN / qutenav / nautograf). Where a citation is external knowledge that could not be verified inside this repo, it is marked **[corroborating]**. Where the adversarial check corrected an analysis, it is called out explicitly under *Adversarial corrections*.

**The one-sentence root cause:** NavCore approximates OpenCPN's three load-bearing mechanisms — (1) coverage-region quilting, (2) global shared-edge max-priority, (3) painter's-order area fill — with three rectangle/heuristic/depth-buffer substitutes, and the visible artifacts are the gaps between the substitute and the real mechanism. Most of the "fixes" to date (commit 7dec920, the DRGARE `+1` hack) patched symptoms on the wrong layer.

---

## 1. How OpenCPN actually does it

### 1.1 Quilting / chart selection — coarse charts are never stacked under finer ones

OpenCPN does **not** draw every chart overlapping the viewport. It builds a *quilt*: a mosaic in which each screen region is owned by exactly one chart, chosen largest-scale-first, and each chart is clipped to the region left over after finer charts have claimed theirs.

- **Reference scale gate.** A reference chart is chosen from the zoom level (`AdjustRefOnZoom` / `GetNewRefChart`); its scale is the gate. Candidate selection rejects any chart more detailed than the reference and any chart that is excessively underzoomed (`Quilt.cpp:1454-1481`, `Quilt::Compose:1731`). **[corroborating — line numbers are external; only `LLRegion::Subtract` at `geoprim/LLRegion.cpp:323` was verifiable in-repo]**
- **Region subtraction.** Composition walks candidates largest-scale-first; each chart intersects its coverage region with the remaining viewport, and on a non-empty result the region is **subtracted** from the remainder. Later (coarser) charts can only fill what is left; the loop stops when the viewport is fully claimed (`Quilt.cpp:1854-1972`). At render time each patch is clipped to its `ActiveRegion` = its coverage minus all larger-scale charts' coverage (`Quilt.cpp:2281-2330`). **[corroborating]**
- **Coverage = M_COVR, not bbox.** A chart's region is its `M_COVR(CATCOV=1)`/PLY polygons minus `M_COVR(CATCOV=2)`/NoCovr holes — typically non-rectangular — never the bounding box (`Quilt.cpp:124-203`; verified analogue `s57chart.cpp:3182-3284`). `M_COVR` is a meta object and is **filtered out of rendering** (`s52plib.cpp:9420-9444`, `if (!strncmp(LUP->OBCL,"M_",2)) if (!m_bShowMeta) return false;`). It never produces an area fill.

### 1.2 Shared-edge priority — `PrioritizeLineFeature`

A geometric edge shared by a coastline and a caution-area boundary is **one** `VE_Element`, referenced by the `m_ls_list` of both objects. OpenCPN runs a global two-pass algorithm per chart at render time:

- **Pass 1** (`SetLinePriorities`, `s57chart.cpp:756-778` → `PrioritizeLineFeature`, `s52plib.cpp:6767-6793`): walk every line and area-boundary object in priority order; for each `TYPE_EE` segment stamp `pedge->max_priority = npriority`. Because objects are walked in priority order and the coastline outranks the caution boundary, the shared edge ends holding the **coastline's** priority.
- **Pass 2** (`s57chart.cpp:782-811`): copy each edge's `max_priority` back onto every referencing object's per-segment `priority` field.
- **Render gate** (`RenderLC` `s52plib.cpp:4677-4719`; `RenderLS` `:3964`,`:4564`,`:5146`): emit a segment only if `ls->priority == priority_current`. The CTNARE boundary's own (lower) priority does not match the coast-stamped segment, so it is skipped. **The pink LC pattern is drawn only on CTNARE edges not shared with the coast.** This is global, edge-based, and geometry-shared — not a per-object "is this a coastline" test.

### 1.3 Area / basin fill — CS-computed `AC` token, pure painter's order, no depth buffer

Areas are filled by an `AC` (area color) instruction resolved to one S-52 color and drawn as the pre-tessellated polygon (`RenderToGLAC_GLSL`, `s52plib.cpp:8018-8060`). The fill color for `DEPARE`/`DRGARE` is **computed** by `DEPARE01` (`s52cnsy.cpp:617-687`) from `DRVAL1`/`DRVAL2` — there is always a color (`DEPIT`/`DEPVS`/`DEPMS`/`DEPMD`/`DEPDW`); a `DRGARE` with no `DRVAL1` still gets `AC(DEPMD);AP(DRGARE01)`. Crucially:

- OpenCPN draws areas in **pure painter's order** over `razRules[priority][table]` with **no depth buffer**. Within a priority, order is the LUP-table load order.
- OSENC `DEPARE` polygons are **mutually-exclusive, non-overlapping tessellations** — the inner basin is its own `DEPARE` carved out of the surround, so neighboring same-priority areas never cover each other regardless of order. Inner-basin water is a separate filled feature painted over the surround by priority, not a polygon hole.
- `DEPARE`/`DRGARE` are GROUP1/DISPLAYBASE so SCAMIN can never drop the fill (`s52plib.cpp:9492-9495`, `:9530-9538`).

### 1.4 How tiled reference renderers do the same in a tiled context

- **qutenav** — rigorous region-subtraction quilt with inner/outer coverage. `findCharts` (`chartmanager.cpp:597-644`): `remainingArea = viewArea`; for each scale, `region = remainingArea ∩ outer`, then `remainingArea -= inner`, stop at `minCoverage`. `ChartCover` (`chartcover.cpp:27-61`) builds `outer` (liberal) and `inner` (conservative) regions from `cov − nocov`; the inner/outer gap is the safe seam band. Objects are region+category filtered at render via `PaintDataFilter::canPaint` (`s57chart.cpp:255-287`). **[corroborating — qutenav not present in this repo]**
- **nautograf** — sorts sources coarsest-first, accumulates real coverage polygons per tile and **stops at 98% coverage** (`tilefactory.cpp:171-202`, threshold `:22`), then reverses so the finest chart draws last. Geometry is clipped to the tile with Clipper2 **preserving holes** before tessellation (`chartclipper.cpp:38-127`). Fill vs stroke are semantically separated in the capnp schema (`LandArea`/`DepthArea`/`CoverageArea` are fills; `CoastLine`/`DepthContour` are strokes), so a coastline is one geometry drawn once and a basin is filled by `DepthArea.depth`. Coverage renders as the **white water base layer**, never as land (`tessellator.cpp:592-597`). **[corroborating]**

The common pattern across all three: **coverage is a region used for selection and clipping, never drawable fill geometry; selection is largest-scale-first with region subtraction; fills and strokes are distinct.**

---

## 2. Where NavCore diverges

### 2.1 Selection uses axis-aligned extent rectangles, not coverage regions

`ChartInfo::intersects`/`contains_point` test only the Mercator bounding rectangle (`catalog.rs:113-123`). `charts_for_tile_scaled` (`catalog.rs:293-365`) runs a 5×5 sample-grid quilt over those rectangles, keeping a chart if it covers any uncovered sample point (`:346-360`). There is no inner/outer region and no region subtraction. This is the live render path (via `build_cpu_impl:738`); the unscaled `charts_for_tile:496` with `f64::INFINITY` is only the `inspect_tile_coverage` debug path and does **not** reproduce production selection — a debugging trap.

### 2.2 Coverage falls back to the bbox rectangle, which feeds the stencil

`extract_coverage_polygons` (`builder.rs:3123-3165`) reads real `M_COVR(CATCOV=1)` rings (`:3133`) but **falls back to the full extent rectangle when a chart has no usable M_COVR** (`:3153-3162`). That rectangle is fan-triangulated into `coverage_vertices` (`:1172-1188`), drawn into the stencil (`state.rs:2017-2035`), and used for centroid masking (`:1362-1366`). So masking happens in rectangular patches, not along true coverage.

### 2.3 `MAX_OVERZOOM_OUT=16` drops the finer cells that would mask the coarse one

`catalog.rs:304` `const MAX_OVERZOOM_OUT: f64 = 16.0;`, `:312-316` drops charts with `native_scale < tile_scale_denom/16`. At regional/overview zoom the finer cells are dropped, leaving the coarse cell as sole survivor.

### 2.4 Masking is gated on a finer chart being present — and silently off when it isn't

Coverage stencil triangles are emitted only when `loaded_charts.len() > 1` (`builder.rs:1172`); centroid NODTA/PRTSUR masking is gated on `!detailed_coverages.is_empty()` (`:1363`). **When `MAX_OVERZOOM_OUT` drops the finer cells, the coarse cell is the sole survivor and NO masking runs at all** — its tan LANDA / NODTA areas and SCAMIN-passing soundings draw clipped to tile bounds. This is the confirmed causal link behind the rectangular blocks.

### 2.5 Per-chart edge topology, not global

`EdgeTable` (`geometry.rs:884-890`) is per-chart, keyed by chart-local edge id. `collect_coast_edges` (`builder.rs:2956-2973`) collects LNDARE/COALNE/SLCONS edge ids **within one chart only**. `resolve_rings_excluding` (`geometry.rs:500`) compares chart-local indices (`:525`), so it can never match across cells. NavCore has the storage right but is missing the CE/EE/EC connector model (`geometry.rs:508-594`,`:778-861`): it walks edge interior vertices and only falls back to node positions when an edge has zero interior vertices, dropping the connector segments OpenCPN emits — areas can fail to close, lines fragment.

### 2.6 A depth buffer collapses all GROUP1 areas onto one plane

`chart.wgsl:31` sets `clip_position.z = 1.0 - (disp_prio/10.0)`, i.e. z=1.0 for every priority-0 area. Area pipelines use `depth_write_enabled:true` + `depth_compare:LessEqual` (`state.rs:523-526` fg, `:620-623` bg). With equal z, a later-drawn priority-0 area passes `LessEqual` and **overwrites** the earlier one. Within priority 0, ordering is pure painter's order — and that order is raw OSENC feature-insertion order, because the global sort is keyed on priority only: `all_area_ranges.sort_by_key(|r| r.0)` (`builder.rs:1072`, stable sort). OpenCPN avoids this entirely (no depth buffer; razRules table order).

### 2.7 Code clutter / patch scars

- **Two live render paths.** `state.rs:1684` `if self.tile_mode { draw_tiles } else { legacy single-chart }`. The legacy path hardcodes `color_index = 0` and only renders a subset of classes — a large semi-dead surface that confuses reasoning.
- **Third feature-iteration copy.** `inspect_tile_coverage` (`builder.rs:494-721`) reimplements the area/line/point loop with the wrong (INFINITY) chart selection.
- **DRGARE `+1` priority hack** (`builder.rs:1412-1423`) — bumps DRGARE priority to beat the enclosing DEPARE z-fight. Symptom of 2.6, applied only to DRGARE, never plain DEPARE.
- **7dec920 coast-edge exclusion** (`builder.rs:1658-1664`, `geometry.rs:500`) — attacked the wrong layer (see §4.2).
- **9-tuple `lc_patterns`** threaded through three functions with `#[allow(clippy::type_complexity)]`. Dead stats (`styled_by_cs*` never incremented). PRTSUR01 special-cased away from the generic AP path because it "turns into large false translucent blocks."

---

## 3. The OSENC format & whether to convert it

**Recommendation: keep OSENC as the on-disk format; build a per-chart normalized resolved-topology intermediate in memory. Do NOT pre-quilt into vector tiles.**

The OSENC reader is faithful at the byte level — every renderable-geometry record matches OpenCPN's writer and qutenav's reader exactly (FEATURE 64/65, POINT 80, LINE 81, AREA 82 + TriPrim, MULTIPOINT 83, VE table 96, VC table 97; sign-of-index reversal and v200/v201 stride all correct). Fill is pre-tessellated TriPrims, not rings; this is handled correctly. The defects are one level up.

**What the intermediate must add:**
1. Resolve each feature once into a typed segment chain `{CE(node→edge), EE/EE_REV(edge), EC(edge→node)}`, closing the connector gaps NavCore currently drops (fixes §2.5: areas close, coastlines stop fragmenting into diagonals).
2. Stamp each shared edge with `max_priority` across all referencing features — the correct replacement for the 7dec920 hack and the basis for §4.2.
3. Parse the currently-ignored `CELL_COVR_RECORD` (98) / `CELL_NOCOVR_RECORD` (99) — dropped at `reader.rs:171` — into per-chart coverage regions, enabling real quilt masking instead of the bbox fallback.
4. Keep geometry in chart-local SM coords; resolve once, clip per tile.

**Why not pre-quilted vector tiles:** quilting at an intermediate stage bakes in chart selection and discards the per-cell edge ids the priority model needs. It fixes none of the three artifacts (which are within-cell topology, attribute-decode, and selection/ordering problems). Quilting is a render-time selection concern (`catalog.rs:293`); keep it there.

**Cost:** a contained rewrite of `resolve()`/`resolve_rings()` into the CE/EE/EC model plus a chart-level resolved cache (`src/senc/geometry.rs` + new pass). Topology storage roughly doubles (acceptable; OpenCPN does it).

**Secondary parser gaps** (do not block the artifacts): `feature_ID` discarded (`features.rs:602`); `value_type 1/3` list attributes dropped (`features.rs:688`) — the likely source of garbled `"olr"` labels; `AREA_EXT`/`VECTOR_*_EXT` (84/85/86) ignored (acceptable — v201 does not emit them).

---

## 4. Root causes of the three artifacts

### 4.1 Tan rectangles / vertical bars / grey diagonal patches / "olr 22.0" labels in open water

**Verdict: confirmed. Compound LOD/quilt-selection defect — NOT data, NOT a fill-parse bug. Confidence: high.**

The features are real (ground truth: at z10 over Køge Bugt the renderer draws the 1:90000 cell with LANDA(21)=6276, NODTA(0)=285, plus soundings/labels; finest cell over Brøndby is only 1:22000 — no 1:4000/1:2000 harbour cell exists there). They appear as floating blocks because compositing diverges from OpenCPN's quilt at two points:

1. **Selection by rectangle, not coverage** (`catalog.rs:113-123`, `:346-360`). A coarse cell whose rectangle covers open water is kept and painted; there is no region subtraction (§2.1, §2.2).
2. **`MAX_OVERZOOM_OUT=16` drops the finer masking cells** (`catalog.rs:304`,`:312-316`), leaving the coarse cell as sole survivor. With `loaded_charts.len()==1`, masking is **entirely off** (`builder.rs:1172`,`:1363` — §2.4), so the coarse cell's LANDA/NODTA areas draw full-size clipped to tile edges → rectangular/vertical-bar look. The bbox-coverage fallback (`builder.rs:3153-3162`) feeds rectangular stencil masks where masking does run → grey diagonal NODTA patches.

The garbled "olr 22.0" labels are the coarse cell's soundings/clearance labels fed through `format_s52_text` (`builder.rs:3302`) to TX (`:2122`), likely the `features.rs:688` list-attribute drop. **Data vs rendering: rendering** (selection + masking). The label garbling is a separate, secondary text-decode bug — but it only becomes visible because the coarse cell is wrongly composited; fix the compositing and the labels disappear from open water regardless.

*Adversarial corrections:* the analysis originally only **implied** the masking-off link; the check **confirmed** it (stencil gated on `len()>1` at `builder.rs:1172`; centroid masking gated on `!detailed_coverages.is_empty()` at `:1363`). The OpenCPN/qutenav/nautograf line numbers are external and unverifiable in-repo (only `LLRegion::Subtract` was verified) — corroborating, not load-bearing. The "olr" sub-claim is asserted, not proven to be the exact decode path, and is explicitly secondary.

### 4.2 Magenta CTNARE caution symbols stamped along the coastline

**Verdict: confirmed. Wrong boundary-style default. NOT data. Confidence: high.**

NavCore's `MarinerSettings` defaults to `symbolized_boundaries: true` (`settings.rs:80`) with a comment falsely claiming "Default: true (matches OpenCPN)" (`settings.rs:51`). OpenCPN's default is **PLAIN** (`s52plib.cpp:308 m_nBoundaryStyle = PLAIN_BOUNDARIES`). With Symbolized, `TableName::preferred_ext` (`lookup.rs:147-153`) returns the Symbolized table, so CTNARE Area resolves to `SY(CTNARE51);LC(CTNARE51)` (chartsymbols.xml id 374). `build_areas` queues the LC op and `generate_stamps_along_polyline_with_phase` (`builder.rs:1013`) stamps the magenta pattern along the boundary — verified live as `lc_stamps=13/17/207` on coastal tiles. With PLAIN, CTNARE resolves to LUP id 34 `SY(CTNARE51);LS(DASH,2,TRFCD)` — a single dashed line, no LC stamps.

**Why 7dec920 did not fix it:** it added `resolve_rings_excluding` gated on coast_edges + `boundary_priority<7` to drop boundary *segments* reusing coast edge ids. But the CTNARE perimeter's edge ids do not match the LNDARE/COALNE ids collected by `collect_coast_edges`, so the rings stay intact and the LC stamps emit anyway. 7dec920 attacked the wrong layer: the clutter is the LC pattern *existing at all* (Symbolized table), not coast-edge overlap.

**Data vs rendering: rendering** (boundary-style setting). **Note:** `build_areas` also silently drops the legitimate area-centroid `SY(CTNARE51)` — Symbol falls into `_ => {}` (`builder.rs:1611-1632`). After fixing the default, the caution area shows a dashed outline but no centroid advisory symbol; full parity needs one centroid symbol emitted (never stamped along the boundary).

*Adversarial corrections:* none to the root cause. The check bounded the fix: DEPARE/DRGARE/RESARE resolve to the same CS procedure in both tables, so depth shading is unaffected by the flag; only ~36 direct-instruction area classes switch. The CTNARE Plain `LS(DASH,2,TRFCD)` does render (verified `LineStyle` arm at `builder.rs:1613`).

### 4.3 Inner harbour basins render white instead of light-blue water

**Verdict: confirmed. Render-time draw-order ambiguity among same-priority fills. NOT data, NOT fill-parse. Confidence: high.**

Ground truth proves the basin is built correctly: z14 Brøndby packet = 378 verts color 30 (DEPMD, light blue 186,213,225) + 27 verts color 31 (DEPMS); correct DEPARE02 logic (`depare.rs:42-92`). The defect is render-side. All water/depth areas are GROUP1 → priority 0 → z=1.0 in the shader (`chart.wgsl:31`). With `LessEqual` + `depth_write` (`state.rs:525-526`), a later-drawn priority-0 area overwrites the earlier one (§2.6). Within-priority order is raw OSENC order (`builder.rs:1072` keyed on priority only). If the large enclosing deep-water DEPARE (DEPDW ≈ 212,234,238, near-white) is iterated **after** the inner-basin DEPARE, it paints over the basin → basin reads white. The developers band-aided this for DRGARE only (`builder.rs:1412-1423`, `+1` priority); plain DEPARE basins (Brøndby) are not covered.

OpenCPN does not have this because it draws in pure painter's order over razRules with no depth buffer, and OSENC DEPARE polygons are mutually-exclusive non-overlapping tessellations. NavCore breaks it by adding a depth buffer that collapses GROUP1 to one plane (turning overlap into last-writer-wins) and by the DRGARE `+1` hack that puts DRGARE on a different z-plane than the DEPARE it sits inside.

**Data vs rendering: rendering** (depth buffer + non-deterministic sort).

*Adversarial corrections (important):* the analysis's stated minimal fix "1(a)+3" is **incomplete**. Removing the depth tie-break and the DRGARE hack falls back to pure CPU painter's order — which is still arbitrary OSENC order. The artifact is only reliably fixed when the priority-only sort (`builder.rs:1072`) is **also** replaced with a deterministic `(priority, lup_table_order/object-class rank, feature_index)` key so basin/DRGARE fills order after the enclosing DEPARE, the way razRules[prio][table] does. **The correct minimal fix is steps 1(a)+2+3 together; step 2 is mandatory.** The check also confirmed the depth buffer provides no cross-priority ordering (the CPU loop `for priority in 0..10` at `state.rs:2075` does) — its only operative effect is the within-priority tie-break that is the artifact source. Coarse-first chart processing (`builder.rs:795`,`:830`) means the artifact bites when enclosing DEPARE and basin DEPARE are in the **same** cell.

---

## 5. Remediation plan

**Sequencing principle:** stop the symptom-patching cycle by fixing each artifact on its *real* layer, smallest-correct-fix first, then land the one architectural change that removes the class of bugs. Do NOT add another heuristic on top of an existing heuristic.

### Phase 0 — Quick, correct, low-risk (do FIRST; each removes a symptom on its real layer)

**Q1. Boundary style → PLAIN (fixes 4.2).** `settings.rs:80` set `symbolized_boundaries: false`; fix the wrong comment at `:51`. Expose as a runtime toggle (already hashed into the CS cache key at `engine.rs:46`, so switching invalidates cached resolutions). *Risk: low.* Verify depth-contour / TSS / fairway boundaries at multiple zooms.

**Q2. Deterministic area draw order + kill the depth tie-break + remove DRGARE hack (fixes 4.3). All three together — step 2 is mandatory.**
- `chart.wgsl:31`: write a constant z for areas; set area pipelines (`state.rs:523-526`, `:620-623`) to `depth_compare:Always` / `depth_write:false`. Painter's order via the CPU priority loop becomes authoritative (OpenCPN's model).
- `builder.rs:1072`: replace `sort_by_key(|r| r.0)` with `(priority, lup_table_order, feature_index)`. Derive the rank from actual S-52 LUP load order, not an ad-hoc list. Add a unit test asserting basin/DRGARE fills sort after the enclosing DEPDW DEPARE.
- `builder.rs:1419-1423`: delete the DRGARE `+1` hack. **Do not ship this alone** — it regresses without the deterministic sort.
*Risk: medium (sort reorders all same-priority fills cell-wide).* Bound: before/after renders at z14/z16 (`test_brondby.sh`) and at a coarse single-DEPARE zoom; confirm DEPVS/DEPMS/DEPMD/DEPDW bands unchanged. Keep the bg-then-fg draw sequence intact (do not change bg pipeline ordering).

**Q3. (Optional, parity polish) Emit one area-centroid symbol.** Add a `RenderInstruction::Symbol` arm in `build_areas` (`builder.rs:1611-1632`) emitting exactly ONE symbol at the area centroid — never stamped along the boundary (OpenCPN `s52plib.cpp:3194`). Needed for full CTNARE parity after Q1. *Risk: low.*

### Phase 1 — The architectural change (fixes 4.1; warranted, removes the whole artifact class)

This is the change that stops the cycle. Do **(1)+(2) only** as the minimal first cut; defer (3)/(4) which have larger surface area.

**A1. Build per-chart coverage REGIONS from real M_COVR.** Parse `CELL_COVR`/`CELL_NOCOVR` (98/99, currently dropped at `reader.rs:171`) and/or use `extract_coverage_polygons` M_COVR rings; store `CATCOV=1` minus `CATCOV=2`/NoCovr per chart (mirror `s57chart.cpp:3182-3284`, qutenav `chartcover.cpp:27-61`). **Remove the extent-rectangle fallback** at `builder.rs:3153-3162` — but pair the removal with explicit no-M_COVR special-case handling (OpenCPN's "strange case"), not a blind delete (see risk below).

**A2. Largest-scale-first region-subtraction selection.** Replace `contains_point` (`catalog.rs:118`) and the 5×5 grid quilt (`:346-360`): sort largest-scale-first, walk assigning `region = remaining ∩ coverage`, then `remaining -= coverage`, stop when empty (OpenCPN `Quilt.cpp:1854-1972`; qutenav inner/outer at `chartmanager.cpp:597-644`). A coarse chart is included only for area not already claimed by a finer chart.

**A3 (defer). Reference-scale gate** replacing `MAX_OVERZOOM_OUT=16` (`catalog.rs:304`). Candidates must be scale ≥ reference. **Must preserve the existing safety valve** (`catalog.rs:318-324`, keep coarsest when all candidates too detailed) or zoomed-out views go blank.

**A4 (defer). Geometric clip before tessellation** (nautograf Clipper2, `chartclipper.cpp:38-127`) replacing centroid `point_in_any_polygon` (`builder.rs:1362,2444,2692`) and the GPU coverage stencil (`state.rs:2017-2035`). This introduces polygon-region boolean ops the codebase lacks — tension with KISS; defer until 1+2 are proven.

**Risks to bound before shipping A1+A2:**
- Deleting the bbox fallback (A1) is the riskiest single change. A coarse chart with no M_COVR must still *render its own features* (confirm not skipped by `tile_region_fully_covered` `builder.rs:824` / `is_background` `:830`); and a finer chart with no M_COVR would no longer mask the coarse one (reintroduces leak in the no-M_COVR case). **Measure how many real cells lack `CATCOV=1` M_COVR** before relying on this.
- A2 needs polygon-region boolean ops not currently present (only `point_in_any_polygon` + fan triangulation). Implement behind a flag.
- Regression-test legitimate cases: offshore CTNARE/caution boundary must still draw over water; DEPARE/DEPCNT boundaries must not be over-masked at tile seams; screenshot-compare z8/z10/z13 over Køge Bugt/Brøndby vs current and vs OpenCPN, confirming (a) tan/NODTA/sounding blocks gone from open water at overview, (b) coastline and depth areas unchanged at harbour zoom, (c) no blank tiles at coarsest zoom.

### Phase 2 — Structural topology (removes the 7dec920 / DRGARE hack class permanently)

**S1. Normalized resolved-topology intermediate (§3).** Resolve features into CE/EE/EC segment chains; add the global shared-edge `max_priority` pass (OpenCPN `s57chart.cpp:756-811`, `s52plib.cpp:6767-6793`). Gate boundary segment emission on `segment.priority == feature.priority`. This **replaces** the per-chart-local `coast_edges`/`boundary_priority<7` special case (`builder.rs:1658-1664`, `geometry.rs:500`) and generalizes its intent to all shared edges. Also fixes area-closure / line-fragmentation from the dropped connector segments. *Risk: medium-high; land independently of Phase 0.*

### Phase 3 — Cleanup (reduce future confusion; not artifact-blocking)

Remove or quarantine the legacy single-chart render path (`state.rs:1684`), the third feature-iteration copy `inspect_tile_coverage` (`builder.rs:494-721`) or at least fix its INFINITY selection, dead stats (`styled_by_cs*`), and refactor the 9-tuple `lc_patterns` into a struct. Fix the secondary list-attribute drop (`features.rs:688`) and `format_s52_text` (`builder.rs:3302`) for the "olr" labels.

**Order of operations:** Q1 → Q2 (1a+2+3 together) → Q3 → A1+A2 (flagged, verified) → A3 → S1 → A4 → Phase 3. Do Q1/Q2 first: they are correct, low-risk, and immediately stop two of three symptoms without touching the quilt; A1+A2 is the architectural fix for the third and the one that ends the rectangle/heuristic patching cycle.
