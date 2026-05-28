# OpenCPN S-52 Rendering Logic — Reference & NavCore Parity Map

**Date:** 2026-05-28
**Source studied:** `doc/openCPN/libs/s52plib/src/` (the vendored S-52 Presentation Library:
`s52plib.cpp` 11 339 lines, `s52cnsy.cpp` 6 258 lines, `chartsymbols.cpp`, `mygeom.cpp`,
`s52shaders.cpp`, `s52utils.cpp`, headers).
**Method:** six parallel deep reads of the OpenCPN source (render dispatch, conditional
symbology, LUP/SCAMIN, line/area/pattern, text/symbol/declutter, color/settings), then
cross-referenced against the current NavCore implementation.

> **Scope caveat:** this subset is the *presentation library only*. The per-frame loop that
> walks `razRules[priority][table]`, the object→table assignment, and **chart quilting /
> coverage selection** live in OpenCPN's `s57chart.cpp` / `quilt.cpp`, which are **not** in
> this directory. Those are reconstructed from the well-known OpenCPN structure and flagged.

---

## Part A — How OpenCPN Renders (the rules)

### 1. Render dispatch & display priority
- **10 priority levels** `PRIO_NODATA..PRIO_MARINERS` = chars `'0'..'9'` (`s52s57.h:67-81`),
  decoded `DPRI-'0'`. **5 LUP tables** `SIMPLIFIED='L', PAPER_CHART='R', LINES='S',
  PLAIN_BOUNDARIES='N', SYMBOLIZED_BOUNDARIES='O'` (`s52s57.h:50-57`). Objects bucket into
  `razRules[priority][table]`. Effective priority = `LUP->DPRI` unless `obj->m_DPRI>=0`
  overrides (CS can promote) (`s52plib.cpp:3671`).
- **Frame order (in the external s57chart loop):** priorities **0→9**; within each priority
  **areas → lines → points**; then a **separate final text-only pass** (`DoRenderObjectTextOnly`,
  `s52plib.cpp:6553`) so labels always sit on top.
- **Per-object dispatch** = walk the matched LUP's `ruleList`, switch on rule type
  (TX/TE/SY/LS/LC/AC/AP/CS/MPS/ARC) (`DoRenderObject`, `s52plib.cpp:6429`). Areas honor only
  `AC`/`AP` (`RenderAreaToGL`, `:8725`). CS expands once and is cached (`bCS_Added`), **except
  SOUNDG which re-runs every frame**.
- **No GPU depth buffer.** Zero `glDepth*`/`DEPTH_TEST` in the codebase — pure painter's
  algorithm.
- **`PrioritizeLineFeature` (`s52plib.cpp:6767`) — the key non-obvious rule:** a physical edge
  shared by several features (coastline = LNDARE boundary + COALNE, DEPARE boundary = DEPCNT,
  …) is stamped with the **max priority** of any feature using it
  (`pedge->max_priority`, `pcs->max_priority_cs`). At draw time an edge is **skipped unless
  `max_priority == current priority`** (`:4172`, `:5002`), so each shared edge is painted
  exactly once at its dominant styling. This is OpenCPN's anti-double-blend / anti-z-fight
  mechanism in lieu of a depth buffer.

### 2. Conditional Symbology (≈30 dispatched procedures, `s52cnsy.cpp` `condTable[]` @3899)
Active (non-stub) procedures and purpose:

| CS | line | purpose |
|---|---|---|
| DATCVR01 | 89 | coverage limit → `LC(HODATA01)`; overscale/scale-boundary **FIXME/unimpl even in OpenCPN** |
| DEPARE01 (=02) | 617 | depth-area fill from DRVAL1/2 vs contours (SEABED inlined) |
| DEPCNT02 | 709 | safety-contour selection, highlight, promote DISPLAYBASE, QUAPOS dash |
| LIGHTS05→06 | 1012/1328 | light flares, sector arcs, colors, leglines, description text |
| OBSTRN04 | 1655 | obstruction/UWTROC incl. isolated-danger (UDWHAZ03); +OBJNAM text |
| QUAPOS01→QUALIN01/QUAPNT01 | 2064/2093/2158 | positional-accuracy line/point styling |
| SLCONS03 | 2224 | shoreline construction styling (QUAPOS/CONDTN/CATSLC/WATLEV) |
| RESARE02 | 2323 | restricted-area symbol+boundary (reads `SYMBOLIZED_BND`) |
| RESTRN01→_RESCSP01 | 2501/2539 | restriction symbol from RESTRN ranking |
| SOUNDG02/03 | 2630/2646 | multipoint sounding split → per-point SNDFRM02 |
| SNDFRM02 | 2661 | builds sounding **digit-glyph** string; safety-depth → SOUNDS vs SOUNDG |
| TOPMAR01 | 2934 | topmark by TOPSHP + rigid/floating detection |
| UDWHAZ03 | 521/3195 | **isolated danger** → `SY(ISODGR51)` + **promote DISPLAYBASE** |
| WRECKS02 | 3239 | wreck symbology + 'Wk' text + UDWHAZ03 |
| SYMINS01 | 3881 | virtual-AIS ATON: returns raw `SYMINS` string |

Stubs (handled by the C++ renderer, not CS): CLRLIN, LEGLIN, OWNSHP, PASTRK, VESSEL, VRMEBL,
DEPVAL, LITDSN (real work in `_LITDSN01`), SEABED01 (inlined into DEPARE01).

**DEPARE01 / SEABED01 depth shades (`s52cnsy.cpp:617-687`):**
- base `AC(DEPIT)`; `drval1>=0 && drval2>0` → `AC(DEPVS)`.
- **2-shade:** `drval1>=SAFETY && drval2>SAFETY` → `AC(DEPDW)`.
- **4-shade (default):** `>=SHALLOW`→`DEPMS`, `>=SAFETY`→`DEPMD`, `>=DEEP`→`DEPDW`
  (cascading; threshold is `drval1>=c && drval2>c`).
- bad-chart guard `drval2=drval1+0.01`. DRGARE no-DRVAL1 → `DEPMD` + `AP(DRGARE01)` +
  `LS(DASH,1,CHGRF)`. Legacy `_SEABED01` also appends `AP(DIAMOND1)` when `SHALLOW_PATTERN`
  set — **active code omits the shallow diamond pattern**.

**DEPCNT02 safety contour (`:709`):** uses a **pre-computed `m_chart_context->safety_contour`**
(the entered value or the next deeper contour that actually exists in the data). Safe contour →
`LS(SOLD,2,DEPSC)` (reliable) / `LS(DASH,2,DEPSC)` (QUAPOS-poor), **promoted to DISPLAYBASE with
`Scamin=1e8+1` (SCAMIN disabled)**. Not-safe → `LS(..,1,DEPCN)`.

### 3. LUP selection, display category, SCAMIN
- **`_LUP2rules` (`:1117`)** lazily parses `LUP->INST` to a `Rules` list (`StringToRules`,
  `:958`). `CS()` records the procedure pointer; expansion happens at render in
  `GetAndAddCSRules` (`:9271`), dedup-cached in `condSymbolLUPArray`.
- **`FindBestLUP` (`:766`):** scan the class's contiguous LUP slice; per candidate
  `score = matched_attrs / total_attrs`; **first candidate scoring exactly 1.0 wins**
  (`:914`). Wildcard value `" "` always matches; `"?"` matches only if the object lacks the
  attr; reals within `1e-6`. Array sorted **most-attributes-first** (`CompareLUPObjects`,
  `:186`) so the most specific match is hit first.
- **Table choice:** points → `m_nSymbolStyle` (SIMPLIFIED/PAPER, default PAPER), areas →
  `m_nBoundaryStyle` (PLAIN/SYMBOLIZED, default PLAIN), lines → LINES. Mirrored into CS params
  `SYMPLIFIED_PNT`/`SYMBOLIZED_BND` (`:634`).
- **Display category** ladder DISPLAYBASE⊆STANDARD⊆OTHER (`ObjectRenderCheckCat`, `:9409`);
  MARINERS uses per-OBJL `nViz`; `M_*` meta hidden unless ShowMeta; SOUNDG gated by
  ShowSoundings. CS can promote category → re-check after CS (`:9677`).
- **SCAMIN (`:9483`):** hide when `chart_scale > Scamin`. **Exempt DISPLAYBASE & GROUP1.**
  **Soft band:** `mod = clamp(pow(8, zoom_mod/5), .2, 8)`; in `(Scamin, Scamin*mod]` the object
  renders **shrunk** via `g_scaminScale` (1.0→0.5). **SUPER_SCAMIN:** synthesize SCAMIN from
  `chart_scale*4` (×2 for some sentinels) for ENC-undefined SCAMIN, with a class exclusion
  list. `$TEXTS` hidden past Scamin for clutter.

### 4. Line, area & pattern rendering
- **Tessellation (`mygeom.cpp`):** GLU (`GLU_TESS_WINDING_POSITIVE`, `:437`) **or** libtess2
  (`TESS_WINDING_POSITIVE`, `:951`) + a Striper pass. Exterior CCW / interior CW holes; a
  **combine callback synthesizes new vertices at self-intersections** (`:1434`) — this is why
  OpenCPN survives messy S-57 polygons. Douglas-Peucker LOD on rings >20 pts.
- **AC fill (`RenderToGLAC_GLSL`, `:8018`):** color resolved **per-frame** via `getColor`,
  drawn from the TriPrim list, per-prim BBox culling.
- **LS lines (`:3627`):** color `getColor(str+7)`, width `atoi(str+5)`; **width law** — below
  7 px/mm use nominal `w` as px, above use `(w/6)*PPMM`, floored by
  `m_GLMinCartographicLineWidth`. DASH/DOTT via a dash shader with mm-based period anchored to
  screen start (continuous phase).
- **LC complex lines (`:4651`/`draw_lc_poly` `:5280`):** stitch connected polyline, advance
  `sym_len = isym_len*PPMM/100`, stamp HPGL symbol per segment **rotated to segment heading**,
  tail drawn as plain line; color from `colRef` pen-letter lookup.
- **AP patterns (`:8411`/`:8815`):** symbol rasterized once to a texture, **tiled in the
  fragment shader by `gl_FragCoord`** with staggered rows (`PATP=='S'`→0.5 shift), anchored to
  a world reference point via `fmod`. Cached per `m_colortable_index`.
- **Everything color/scale-dependent is computed per-frame, never baked** — palette, line
  width in mm, dash period, pattern tile size/anchor, LOD.

### 5. Text, symbols, decluttering
- **TX = single attribute string; TE = printf** over a comma attr list (`:1595`/`:1653`);
  abort label if a numeric attr == `2147483641` (S-57 null). Supports `ATTR=default`, quoted
  literals, NATSUR→name, VERCLR/ELEVAT→feet. **National text:** OBJNAM→NOBJNM if enabled.
- **Font size** is normalized, not raw: `clamp10(user_pt + min(bsize,20)/20*4)`; weight <5/5/>5
  → light/normal/bold. Anchor lower-left; `xoffs` in avg-char-widths, `yoffs` in char-heights;
  hjust 1/2/3 = center/right/left, vjust 1/2/3 = bottom/center/top.
- **ShowImportantTextOnly:** skip text when `m_bShowS57ImportantTextOnly && text->dis >= 20`
  (`:2408`). `dis` = per-text display group (distinct from object category). **LIGHTS text
  declutter:** only the first LIGHTS at a given lat/lon gets text (`:2336`).
- **Global declutter:** one viewport-wide `m_textObjList`, cleared once per full render pass
  (`ClearTextList`, `:10039`); reject a candidate whose (rotation-aware) rect `Intersects` any
  already-placed rect (`CheckTextRectList`, `:2310`). **Global, not per-tile.**
- **Symbols:** `RenderSY` (`:3271`) → HPGL vector or raster atlas; angle from inline param,
  overridden by `ORIENT`, **LIGHTS +180°**; scale = `ChartScaleFactorExp * g_scaminScale *
  ContentScaleFactor / dipfactor`; transform translate(pos)·rotate(-vp.rot)·translate(-pivot).
- **Soundings = composed digit glyphs (SNDFRM02 `:2661`), not text.** Prefix `SOUNDS`
  (≤ safety depth, red) vs `SOUNDG`; emit `SY(<prefix><pos><digit>)` per digit. **Position
  group: 1=ones (baseline), 0=tens (right), 2/3=higher (left), 4/5=subscript (drawn LOW,
  `pivot_y=height/5` vs `height/2`)** — this is the small-decimal-as-subscript look. `B1`=swept,
  `C2`=low-reliability, `A1`=drying. DepthFont (`DepthFont.cpp`) builds a 10-digit alpha atlas.

### 6. Colors, palettes, settings, contours
- **Palettes** parsed as raw **8-bit** `S52color{R,G,B}` (`chartsymbols.cpp:85`). Day/Dusk/Night
  = **re-index the color table** (`SetPLIBColorScheme`, `:1378`; v3.2 alias DAY→DAY_BRIGHT).
  Tokens resolve per-frame via `getColor` → `GetColor(name, m_colortable_index)`, **5-char key
  truncation** (`:815`).
- **★ OpenCPN renders raw 8-bit sRGB values — NO gamma, NO linearization, NO `GL_SRGB`.**
  (Grep for gamma/srgb/pow/to_linear: none in the color path.) Confirms the
  [[srgb-washout-bug]] fix direction: write palette bytes verbatim, encode to sRGB at most once.
- **Vector color-ref = 6-char records (1 pen letter + 5-char token)**, e.g. `"ACHMGD"` = pen A +
  `CHMGD`. HPGL `SPx` → `findColorNameInRef` returns the matching record's 5-char token
  (`:10510`). Raster bitmaps index the color array by `char-'A'`.
- **Mariner params (`s52utils.h:59`)** + library defaults (`s52utils.cpp:134`): TWO_SHADES
  FALSE (4-shade), SAFETY_CONTOUR/SHALLOW/DEEP (lib fallback 3/2/6; "Chart No 1" reference
  10/5/30), SHALLOW_PATTERN TRUE, FULL_SECTORS TRUE, etc. App layer overrides from user.
- **Safety contour** uses the per-chart precomputed `m_chart_context->safety_contour`
  (next-deeper-available), not the raw setting.

---

## Part B — NavCore Parity Map (what we match / miss)

Legend: ✅ done · 🟡 partial · ❌ missing. Severity = visual impact at typical harbour zoom.

| # | OpenCPN rule | NavCore status | Sev |
|---|---|---|---|
| C1 | Raw sRGB color, single encode | ✅ UNORM surface (fixed this session) | — |
| C2 | Token→RGB per-frame, palette re-index for Day/Dusk/Night | ✅ palette GPU storage buffer; areas+lines `color_index` | — |
| C3 | Vector color-ref pen-letter prefix → token | ✅ strip pen letter (fixed this session, `lc.rs`) | — |
| C4 | 4-shade DEPARE thresholds (drval1≥c && drval2>c) | ✅ `cs/depare.rs` matches | — |
| C5 | DRGARE `AP(DRGARE01)` + dashed boundary | 🟡 dashed boundary only; pattern fill suppressed by choice | low |
| C6 | Shallow `AP(DIAMOND1)` pattern | ❌ (OpenCPN active code also omits) | low |
| L1 | LUP first-100%-match, most-specific-first sort | 🟡 has matching+attr; verify tie-break/sort parity | med |
| L2 | Point table SIMPLIFIED vs PAPER by setting | 🟡 `simplified_points` exists; default PAPER | low |
| L3 | Area PLAIN vs SYMBOLIZED boundary | ✅ `symbolized_boundaries` default true | — |
| P1 | Priorities 0–9, areas→lines→points→**text last** | 🟡 per-tile priority + **GPU depth buffer** (z=1−prio/10) instead of painter's; text is a later global pass ✅ | med |
| P2 | `PrioritizeLineFeature` shared-edge max-priority | ❌ not implemented (depth buffer hides most cases) | med |
| S1 | SCAMIN hard cutoff, exempt DISPLAYBASE/GROUP1 | ✅ `should_render_at_scale_ex` | — |
| S2 | SCAMIN soft zoom-modifier band (shrink to 0.5×) | ❌ binary cutoff only | med |
| S3 | SUPER_SCAMIN from chart native scale | 🟡 has `chart_native_scale` super-scamin path | low |
| CS1 | ~20 active CS procedures | 🟡 **14** present (depare, depcnt, lights, obstrn, qualin, quapos, resare, restrn, slcons, sndfrm, topmar, wrecks, datcvr) | med |
| CS2 | UDWHAZ03 isolated danger → ISODGR51 + DISPLAYBASE | ❌ no dedicated isolated-danger promotion | med |
| CS3 | DATCVR01 coverage line | 🟡 `datcvr.rs` exists; coverage handling partial | low |
| CS4 | SYMINS01 virtual AIS ATON | ❌ | low |
| CS5 | Safety-contour = next-deeper-available, highlight DEPSC | 🟡 raw setting; DEPCNT highlight partial | med |
| T1 | Labels show at detail; ShowImportantTextOnly scale-aware | ✅ scale-aware z≥14 (fixed this session) | — |
| T2 | TE printf light descriptions ("Fl G 3s 4m 4Nm") | 🟡 light text renders; verify TE format fidelity | med |
| T3 | National OBJNAM→NOBJNM | ❓ unverified | low |
| T4 | Global text declutter across viewport | ✅ global across visible tiles (`build_global_text_buffers`) | — |
| T5 | LIGHTS first-at-position text declutter | ❓ unverified | low |
| T6 | Font size normalization + justification/offset convention | 🟡 renders; fidelity to OpenCPN sizing unverified | low |
| SY1 | Symbol rotation (ORIENT, LIGHTS+180°) | 🟡 lights rotation present; audit others | low |
| SY2 | Symbol scale chain incl. g_scaminScale | 🟡 fixed scale; no soft-SCAMIN shrink | low |
| SN1 | **Soundings as subscript digit-glyphs** (2₅) | ❌ decimal text "2.5" | **high** |
| SN2 | SOUNDS (≤safety, red) vs SOUNDG; swept/reliability markers | 🟡 has sndfrm flags; verify color/markers | med |
| LN1 | LS width law (mm-based on hi-DPI), min-width floor | 🟡 pixel width; mm/DPI law partial | low |
| LN2 | LC stamp advance + per-segment rotation + colors | ✅ renders (colors fixed this session) | — |
| AR1 | Robust tessellation (GLU/libtess2 + combine for self-intersect) | ✅ **CORRECTED 2026-05-28**: area fills consume OpenCPN's **pre-tessellated** OSENC triangles (`for_each_triangle_global`); fallback uses **`earcutr` with hole indices** (concave+holes); `triangulate_fan` only runs on convex clip fragments. Robust. Residual: earcut (not GLU) on self-intersecting polygons in the rarely-hit fallback. | low |
| AR2 | AP pattern screen-space tiling, staggered, world-anchored | 🟡 patterns exist; capped (`MAX_LC_STAMPS`), heavier than ref | med |
| Q1 | Chart quilting / coverage masking (no visible seams) | 🟡 partial masking; **hard depth-shade seam** at chart edges | **high** |
| Q2 | Per-frame LOD (Douglas-Peucker) by view scale | ❌ no LOD; full geometry per tile | med |

---

## Part C — Prioritized remaining work (by visual impact)

1. **Soundings subscript rendering (SN1)** — adopt OpenCPN's digit-glyph + position-group
   model (main number baseline, decimal as lowered subscript). High visual signal, bounded
   change in `text_layout`/`sndfrm`.
2. **Tessellation robustness (AR1)** — replace fan triangulation with a proper tessellator
   (earcut-with-holes or a libtess2 port). Fixes sliver/hole artifacts on complex coastlines &
   basins; also unblocks correct pattern fills.
3. **Multi-chart coverage seam (Q1)** — the hard depth-shade edge where a detailed chart meets
   a coarser one. Needs the coverage-mask/quilt step that OpenCPN does in `s57chart`/`quilt`.
4. **Safety contour selection + DEPCNT highlight (CS5)** — compute the available-contour set
   per chart, highlight the safety contour in DEPSC at double width, promote to DISPLAYBASE.
5. **UDWHAZ03 isolated danger (CS2)** — ISODGR51 + DISPLAYBASE promotion for dangerous
   rocks/obstructions/wrecks. Safety-relevant.
6. **SCAMIN soft band (S2)** + **PrioritizeLineFeature (P2)** — smoother declutter on zoom and
   correct shared-edge styling.
7. **LS width-in-mm law (LN1)**, light TE text fidelity (T2), national text (T3), LOD (Q2).

---

## Part D — Parity completion estimate

Weighting subsystems by their contribution to the on-screen result at harbour/approach scale:

| Subsystem | Weight | NavCore | Contribution |
|---|--:|--:|--:|
| Color / palette / depth shades | 22% | 95% | 20.9 |
| Areas (fill + tessellation robustness) | 15% | 85% | 12.8 |
| Lines (LS/LC, colors, width) | 12% | 82% | 9.8 |
| Text labels (names, light desc, declutter) | 13% | 78% | 10.1 |
| Soundings (subscript format + reliability) | 10% | 85% | 8.5 |
| Point symbols (buoys, beacons, lights) | 10% | 80% | 8.0 |
| CS procedure coverage | 8% | 70% | 5.6 |
| SCAMIN / LOD / display category | 6% | 65% | 3.9 |
| Multi-chart quilting / coverage | 4% | 55% | 2.2 |

**Weighted parity ≈ 82%.** (Updated 2026-05-28 after the subscript-sounding fix and
correcting AR1 — area tessellation is already robust via pre-tessellated OSENC triangles.)

Interpretation: the **dominant, chart-wide correctness** (colors, depth shades, line/area
colors, label visibility, symbol presence) is essentially solved — that's why the latest
captures look close to OpenCPN. The remaining ~25% is concentrated in **a few high-impact
specifics**: sounding subscript styling, tessellation robustness, and multi-chart coverage
seams, plus a long tail of fidelity items (soft SCAMIN, shared-edge line priority, full CS set,
safety-contour selection, mm-accurate line widths).

Session 2026-05-28 fixes that moved the needle: sRGB washout (UNORM surface), LC complex-line
colors (pen-letter strip), and scale-aware important-text labels.
