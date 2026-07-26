# NavCore2 vs OpenCPN: Rendering Gap Analysis v2 (Harbour Screenshot Audit)

**Date**: 2026-04-19
**Reference**: Brøndby / Ishøj harbour (Denmark), ~harbour-scale zoom
**Method**: Symbol-level navigation via serena (symbolic) + line-level cross-reference against `doc/openCPN/libs/s52plib/src/` and `assets/s52/chartsymbols.xml`
**Scope**: Focus on what is **different from openCPN in the user-provided screenshot**, not abstract S-52 compliance.

---

## Visible Symptoms → Root Cause Map

| # | Visible defect in navcore2 screenshot | Primary root cause | Rank below |
|---|---|---|---|
| A | Area-name labels (Krabben, Muslingen, …) too small / too thin / gray instead of bold black | 5x7 procedural bitmap font, no weight/bsize spec parsing | **Gap 1** |
| B | Soundings ("1.5", "2.7", …) look thin and spindly, not large bold black | Procedural 7-segment digit shader instead of SOUNDS11/SOUNDG25 atlas glyphs | **Gap 2** |
| C | No light-character text ("Fl 3s 4m 3Nm") at all | Hard-coded `scale: 0.75` on litdsn01 labels produces 6 px glyphs against the 5x7 bitmap — unreadable / sub-pixel | **Gap 3** |
| D | Many spurious green solid rectangle outlines and dashed colored lines where openCPN shows none | `show_other = true` default + explicit `bypass_display_category_filter` for M_COVR + forced UINFG chart-outline rectangle | **Gap 4** |
| E | Some lines that openCPN shows are missing in navcore | Score-based LUP matching absent (prior audit Gap 4 still unresolved) + TE text on LINES only uses bbox-center placement | **Gap 5** |
| F | Extra place-name labels ("Vallensbæk Lystbadehavn", "Ishøj Lystbadehavn", "Hummeren") openCPN hides | No `ShowImportantTextOnly` / text-viewing-group filter; `show_other = true` also brings in HRBFAC/LNDRGN names | **Gap 6** |
| G | Red sector arc at harbour entrance missing / inconsistent | Either sector light has valid SECTR1/2 but navcore's `lights06` routes it through all-round symbol (no arc), or SECTR1/SECTR2 not decoded as Float by SENC parser | **Gap 7** |
| H | Buoy/beacon symbols look less crisp than openCPN | Atlas is Day-only (prior Gap 6 still unresolved); symbols anchored by pivot but rotated with `pivot` not applied in shader the same way openCPN applies it; no simplified-symbol toggle | **Gap 8** |

---

## CRITICAL — Gaps that dominate the Brøndby screenshot diff

### Gap 1. Text rendering: 5x7 procedural bitmap, no weight / no bsize

This is the single biggest visual difference on the screenshot.

**navcore2 state (verified)**:
- Atlas is a procedurally-built 5x7 pixel monochrome bitmap, one per ASCII char 32-127.
- Cell size is 8x8 px (`CELL_W = 8`, `CELL_H = 8`) — `src/render/label.rs:6-7`.
- `build_font_atlas()` paints single-pixel strokes — `src/render/label.rs:270-297`.
- `layout_text()` scales the 8x8 cell by `params.scale` — `src/render/text_layout.rs:101-154`.
- Glyph color is multiplied by the bitmap alpha in the atlas shader — `assets/shaders/text_atlas.wgsl:73-77`. At small `scale`, single-pixel glyphs produce sub-pixel antialiasing against whatever background, giving the "grey not black" look even when `color = CHBLK = (0.027,0.027,0.027)`.
- Scale comes from `s52_text_scale(size)` — `src/tiles/builder.rs:2806-2808` — which is `size / 11.0` clamped to `[0.9, 4.0]`.
- **`size` is wrong**: the TX/TE parser in `src/s52/instruction.rs:157-179` treats the *last* argument as `size`. Per `_parseTEXT` in `s52plib.cpp:1566-1592` the last field is `text->dis` (text display group, values 21-29), not the font size. The font size lives inside the 5-character **CHARS** field (arg index 4, e.g. `'15110'` → style=1, weight=5, width=1, bsize=10). Navcore **does not parse CHARS at all** — `style`, `weight`, `bsize` are dropped on the floor.

**openCPN state (verified)**:
- `s52plib.cpp:1579-1581`: `text->weight = buf[1]; text->bsize = atoi(buf + 3)`.
- `s52plib.cpp:2416-2424`: `spec_weight > 5 → wxFONTWEIGHT_BOLD`.
- `s52plib.cpp:2465-2488`: font size = user's ChartText default size + normalized `bsize` contribution, clamped to min 10 pt.
- `s52plib.cpp:2093-2097`: glyphs cached per-font via `TexFont::Build()` — actual TTF outlines rasterized to a sub-pixel atlas with proper stroke weight.

**Why the screenshot looks the way it does in navcore2**:
1. For SEAARE "Krabben" the instruction is `TX(OBJNAM,1,2,3,'15110',0,0,CHBLK,26)` (chartsymbols.xml:2084). `size=26` in navcore → scale = `(26/11).clamp(0.9,4.0) = 2.36` → glyph cell 8·2.36 ≈ **19 px tall** of 5-px-wide single-pixel strokes. OpenCPN instead parses `'15110'` → weight=5 normal, bsize=10, and adds that to the user's default font (~12 pt), rendering a crisp anti-aliased ~14 pt text.
2. For LNDRGN / HRBARE (`'15120'`, bsize=20) and RESARE (`'14108'`, bsize=8) navcore produces the same mid-grey scaled bitmap regardless; openCPN renders 20 pt Arial vs 8 pt as specified.
3. The "bold" sea-area names ("Krabben") in openCPN come from either (a) a custom style that uses weight 6 for marquee-prominent area names or (b) user's preference with bold ChartText font. Either way, navcore cannot produce bold regardless of what CHARS says.

**Fix**:
- Parse CHARS field in both TX and TE branches of `RenderInstruction::parse_single` — extract style/weight/width/bsize as additional fields on `RenderInstruction::Text`.
- Replace the 5x7 procedural bitmap with a real rasterized font atlas. Two low-risk options:
  - Pre-bake a texture atlas per (weight, size) on startup using `fontdue` (pure Rust, no C deps) or `ab_glyph` — two permutations (Normal, Bold) × 3-4 sizes is ~8 small atlases. Or:
  - Use `glyphon` / `wgpu_glyph` for dynamic signed-distance-field text — allows runtime sizing without rebuilding atlases.
- Map `bsize` → pixel size via the same normalization openCPN uses (see `s52plib.cpp:2465-2488`): clamp bsize to [0, 20], remap to [0, 4], add to a 12-pt default, floor at 10 pt.
- Stop using `dis` as the scale input.

**Effort**: Medium (font infrastructure + layout rewrite, but `layout_text` already has correct justification math, just swap the glyph source).
**Expected impact**: Massive. This alone closes the biggest visual gap in the screenshot — all OBJNAM labels, all depth-contour VALDCO labels, all bridge-clearance annotations become legible and bold where they should be.

---

### Gap 2. Soundings: procedural 7-segment digits instead of atlas glyphs

**navcore2 state**:
- `assets/shaders/sounding.wgsl:81-141` draws each digit via 7-segment geometry in the fragment shader. Segment width `seg_w = 0.15`, segment thickness `seg_h = 0.08` (lines 106-107).
- Fixed quad is `TEXT_WIDTH = 40, TEXT_HEIGHT = 14` — `sounding.wgsl:36-37`.
- `build_soundings` at `src/tiles/builder.rs:2382` instances the quad with `scale: scamin_scale * 1.8`. With SCAMIN=1.0 → 72 × 25.2 px for a 3-character reading. Each digit ends up ~14 px wide, but the horizontal strokes of the 7-segment are only `0.08 * 25 ≈ 2 px` — visually anaemic.
- Depth color in the shader is hard-coded: shallow (`FLAG_IS_SHALLOW`) = (0.027, 0.027, 0.027, 1.0), deep = (0.490, 0.537, 0.549, 1.0) — `sounding.wgsl:282-290`. This ignores the CHBLK/SNDG1/SNDG2 palette lookup entirely, so switching to Dusk/Night does not re-tint soundings (a latent palette bug the screenshots do not yet show).

**openCPN state**:
- Renders soundings via chartsymbols glyphs. SOUNDS11 for shallow digits, SOUNDG25 for deep digits, SOUNDG01-SOUNDG05 for subscript / prefix markers. `chartsymbols.cpp:` and the XML `<pattern>` / `<symbol>` entries define actual bitmap glyphs (pre-rendered as bold serif-weight digits).
- `s52cnsy.cpp:2661-2932` (SNDFRM02) emits a chain of SY() instructions, one per digit.
- Atlas glyphs are ~10 × 14 px each with bold strokes.

**Verification for navcore2**: `atlas.json` already contains SOUNDS11 (4x10 px), SOUNDG25 (6x10 px), SOUNDG01 (4x10 px) — they are parsed and baked into the atlas already, but **never referenced** in the source (`grep -r "SOUNDS11" src/` returns nothing). The procedural path in `sounding.wgsl` is the sole renderer.

**Fix**:
- Have SNDFRM02 emit atlas-symbol sequences instead of a packed flag blob. For "1.5" at shallow depth: emit SOUNDS11_digit_1, SOUNDS11_digit_subscript_5 at the correct offsets.
- Retire `sounding.wgsl`'s 7-segment fragment path; route soundings through the existing symbol-instance pipeline (same atlas + bind group as other point symbols).
- The swept / drying / uncertainty markers are already in the atlas (UNDRLN01, SWEPT01, QUESMRK1 equivalents) — emit them as supplementary SY() calls.

**Effort**: Medium. The SNDFRM02 formatting in `src/s52/cs/sndfrm.rs` already computes the pieces; wiring the output through `SymbolInstance` is straightforward.
**Expected impact**: Large. Soundings become the large bold black digits matching openCPN. Also fixes the palette-switch latent bug (Dusk/Night soundings stay the wrong color today).

---

### Gap 3. Light description text ("Fl 3s 4m 3Nm") completely invisible

**User-reported**: "No light character text displayed at all."

**navcore2 state (verified)**:
- `litdsn01()` in `src/s52/cs/lights.rs:369-496` is fully implemented and tested (unit tests pass — `test_litdsn01_basic`, etc.). It produces "Fl 3s 4m 3Nm" format correctly.
- Called at `src/tiles/builder.rs:2195`. The result is pushed into `label_candidates` at `builder.rs:2208-2218`:
  ```
  scale: 0.75,   // ← THIS IS THE BUG
  ```
  Hard-coded `scale: 0.75` × `CELL_H = 8` → a **6 pixel** glyph cell. The 5x7 bitmap rendered into a 6-pixel cell is sub-pixel; the shader's `if (sample < 0.1) discard` (line 74 of `text_atlas.wgsl`) culls most fragments. What survives is almost invisible on the user's screenshot.
- The companion "orient_text" at line 2229 uses `scale: 0.65` → 5.2 px → even more invisible.

**openCPN state**:
- Light description uses `'15110'` bsize=10 (per lookup 1965 etc.) — not spec-coded as "small text". Rendered at ~12 pt.
- `RenderText()` path at `s52plib.cpp:2061-2193` draws via TexFont at the spec-derived size.

**Fix**:
- Change `scale: 0.75` to something derived from a sensible default (e.g. scale that produces ~11 pt equivalent, or use bsize=10 like the OBJNAM spec).
- Once the real-font fix in Gap 1 lands, replace both hardcoded 0.75 and 0.65 with a proper TextParams spec sourced from the LIGHTS LUP's TX instruction.

**Effort**: Trivial in isolation (1 line) for immediate visibility restoration; Medium once coupled to Gap 1 font rewrite.
**Expected impact**: High and immediate — "Fl 3s 4m 3Nm" appears next to every light. Even with the 5x7 bitmap, at scale ~1.5 it becomes clearly legible.

---

### Gap 4. Display-category default + M_COVR override → chart-boundary clutter

The user screenshot shows many spurious outlines that openCPN does not draw.

**navcore2 state**:
- `MarinerSettings::default()` at `src/s52/settings.rs:55-72` sets `show_other: true`. Comment claims "matches OpenCPN 'Show All' mode" — but OpenCPN's on-screen default is STANDARD, not SHOW_ALL.
- `S52Engine::bypass_display_category_filter` at `src/s52/engine.rs:90-93` explicitly forces type codes `{69, 301, 302}` (Lake, M_CSCL, M_COVR) to pass the category filter regardless of user setting. So even if the user toggles `show_other = false`, M_COVR still renders.
- `src/tiles/builder.rs:1498-1512`: when an area feature is an `ObjectClass::Coverage`, it emits a **solid UINFG green line** of width 2 at priority 9 for its bounding rectangle. OpenCPN in STANDARD mode would skip M_COVR entirely.
- With `show_other = true`, many inherently-dashed LUPs render:
  - M_QUAL areas (`chartsymbols.xml:1373-1441`) → `AP(DQUAL…);LS(DASH,2,CHGRD)` — dashed green boundaries
  - HRBARE (`chartsymbols.xml:1051`) → `LS(DASH,2,CHGRD)`
  - HRBFAC areas (`chartsymbols.xml:2928-2994`) → CATHAF5 marina and friends — brown area fills + symbols
  - MARCUL, LOCMAG, and many others also flagged `display-cat>Other</display-cat>`

**openCPN state**:
- `s52plib.cpp:309`: constructor default is OTHER (shows everything), but the on-disk UI default is STANDARD (`opencpn.conf` / `toolbar`). When STANDARD, M_COVR, M_QUAL, HRBARE, HRBFAC, LAKARE, M_NSYS (conditional) are all hidden.
- DATCVR01 is still called for M_COVR only when category filter passes — i.e. only when the user turns coverage display on.

**Fix**:
- Change navcore2 default to the openCPN-on-screen default: `show_other: false`.
- Remove the type-code bypass for 302 (M_COVR) in `bypass_display_category_filter`. M_COVR should honor display-cat filtering. If the project wants to visualize chart edges, add a *separate* mariner setting `show_chart_boundaries: bool` — and gate the UINFG rectangle path on that boolean, not on object class.
- Re-audit the bypass list to keep only truly mandatory overrides (DISPLAYBASE features are already handled by the normal category check; the bypass list is redundant for them).

**Effort**: Trivial.
**Expected impact**: Massive. This single change removes the green chart-outline rectangles, the dashed M_QUAL green, harbour-facility brown fills, and a lot of other visual debris instantly.

---

## HIGH — Fidelity gaps close behind the above

### Gap 5. Score-based LUP matching still absent (regression of prior audit Gap 4)

Prior audit "Summary, row 6" was unresolved and remains so. `src/s52/lookup.rs:393-396` still does first-match selection among LUP candidates instead of OpenCPN's scored attribute matching (`s52plib.cpp:306`, `FindBestLUP`).

**Why it matters on this screenshot**: in Danish ENCs a BOYLAT with CATLAM=1 + COLOUR=3,1 may match a less-specific (generic CATLAM=1) LUP first, yielding the wrong symbol (e.g. paper-style starboard buoy instead of the port/starboard-preferred combined-colour symbol). That is plausibly why the user says "buoy/beacon symbols look different style". A few buoys are picking the wrong LUP.

**Fix**: replace first-match with score = matched_attrs / total_filter_attrs; on tie, prefer the LUP with more filter attributes; fall back to the no-attr default.

**Effort**: Small-Medium (one function in `lookup.rs`, plus tests).
**Expected impact**: Medium — resolves category mismatches for buoys/beacons; does not shift the aggregate screenshot diff much but makes symbol choice match openCPN exactly.

---

### Gap 6. No text-group ("dis") filter / no ImportantTextOnly

**navcore2 state**:
- Every label with a non-null OBJNAM is pushed through `declutter_and_layout_labels`. There is no equivalent of `m_bShowS57ImportantTextOnly`.
- The `dis` field is *currently* being parsed into `RenderInstruction::Text::size` and then mis-used for scale (see Gap 1). It is not available as a filter.

**openCPN state** (`s52plib.cpp:2408-2411`):
```
if (m_bShowS57ImportantTextOnly && (text->dis >= 20)) {
    if (b_free_text) delete text;
    return 0;
}
```
With ImportantTextOnly on, only text with dis < 20 renders — meaning OBJNAM for things like LNDRGN (dis=26), HRBARE (no text), and most other clutter disappears, while critical annotations (dis=11, 21 "important OBJNAM") remain.

**Why it matters on this screenshot**: the extra "Vallensbæk Lystbadehavn", "Ishøj Lystbadehavn", "Hummeren" labels that openCPN hides are almost all in text groups 26-29. Filter by dis and they vanish.

**Fix**:
- Promote `dis` to its own field on `RenderInstruction::Text` (separate from the CHARS-derived bsize introduced in Gap 1).
- Add `MarinerSettings::show_important_text_only: bool` (default false; user-toggleable).
- Filter at `declutter_and_layout_labels` (or earlier at collection time).

**Effort**: Small.
**Expected impact**: High — removes the "extra area labels the user does not want" without hiding the important names openCPN keeps.

---

### Gap 7. Light sector rendering reliability (red arc)

**navcore2 state**:
- `light_sector_info` (`src/s52/cs/lights.rs:210-254`) reads SECTR1/SECTR2 via `feature.attribute_float`. If either is missing, returns None → no arc.
- `feature.attribute_float` (`src/senc/features.rs:177-184`) accepts `AttributeValue::Float` and `::Integer`, but **not** `::String`. If SENC encodes sector angles as strings (the S-57 spec says they are floats, but some SENC encoders stringify them), arcs silently disappear.
- `add_light_sector_lines` (`src/tiles/builder.rs:2400`) then needs sweep ≥ 1° and ≠ 360°; it discards edge cases otherwise.
- `lights06` at `src/s52/cs/lights.rs:172-199`: if `SECTR1.is_some() && SECTR2.is_some()`, the symbol becomes all-round (LIGHTS91-93). That is correct.

**openCPN state**:
- `_LIGHTS05/_LIGHTS06` in `s52cnsy.cpp:3532+` constructs a `CA(OUTLW,4,colour,2,sectr1,sectr2,radius)` private arc instruction. Rendered by `RenderCARC` which uses OpenGL arc segments with fixed pixel width regardless of zoom.

**Why it matters on this screenshot**:
- The user sees a red arc in openCPN near the harbour entrance. If navcore is either (a) seeing SECTR1/2 as strings and failing, (b) seeing sweep=360 (full-circle edge case) and returning None, or (c) deduplicating the sector-light feature against a nearby non-sector light (`last_light_pos` radius 15 m at `builder.rs:2184`) — the arc drops.

**Fix, in priority order**:
1. Make `attribute_float` accept `AttributeValue::String` and attempt `str::parse::<f64>()` as a fallback (one-line guard).
2. Log a DEBUG message when a feature has SECTR1/SECTR2 but `light_sector_info` returns None, so the ingest path can be confirmed.
3. Never dedupe a sector-carrying LIGHTS against a non-sector neighbor — gate the `last_light_pos` skip on `light_sector_info(feature).is_none()`.

**Effort**: Trivial-Small.
**Expected impact**: Medium — restores sector arcs to parity.

---

### Gap 8. Day-only symbol atlas (prior audit Gap 6, still unresolved)

Verified again in the current tree: `assets/symbols/atlas.png` is the single Day atlas; `state.rs:switch_palette` updates palette buffer only (`src/render/state.rs:1289-1292`). Confirms prior audit. Not screenshot-critical for the Day mode the user is viewing, but will materially affect Night/Dusk.

---

## MEDIUM — Gaps that slightly shift the diff but are not dominant

### Gap 9. TOPMAR01 still incomplete (TOPSHP 14-33 + no buoy/beacon platform discrimination)

Unchanged from prior audit Gap 9. `src/s52/cs/topmar.rs:9-52` maps 1-13, everything else → TOPMAR01 default. For the Brøndby screenshot this affects the yellow X cardinals (TOPSHP 7 = X is mapped correctly) but not a lot else. Still a spec divergence.

### Gap 10. Line feature TX/TE label placement is bbox-center

`src/tiles/builder.rs:1858-1860` places line text at the centroid of the line's **extent rectangle**, not along the line. OpenCPN places text along the line path (for VALDCO on depth contours this is very visible — opencpn shows depth values repeated along each contour). For dense small features this means a single label far from where openCPN would show several.

**Effort**: Medium (needs polyline path walking; could start with a single label at each polyline's midpoint).
**Expected impact**: Medium — adds "5m", "10m", "20m" style depth-contour labels along contours. Not dominant in the user's harbour screenshot, but noticeable on the larger chart.

### Gap 11. DRVAL2 not checked in 4-shade depare (prior Gap 16, unresolved)

`src/s52/cs/depare.rs:74-87`: 4-shade decisions use drval1 only. OpenCPN's `_SEABED01` checks both drval1 and drval2 at each threshold. For charts where drval2 is materially higher than drval1 (wide-range DEPARE polygons, rare), the color assignment can flip.

### Gap 12. Label priority sort uses feature priority, not text priority

`src/render/text_layout.rs:403` sorts by `candidates[i].disp_prio`, which is the feature's S-52 priority (0-9), not the text's `dis` field. OpenCPN's CheckTextRectList does first-rendered-wins based on the order RenderTX is called, which in turn follows priority-ordered rendering. For lights with dis=24 (ATON description) vs an area with dis=26, the light text should win — in navcore today they tie at feature priority 4 / 2-3 respectively, so it usually works, but not for the right reason.

### Gap 13. Symbol pivot vs rotation interaction

`assets/shaders/symbol.wgsl` and the pivot field in `atlas.json` (e.g. TOPMAR02 pivot `[0.125, -3.0]`) work together. When a topmark is placed on a buoy, OpenCPN multiplies the pivot offset by the symbol scale. If navcore2's shader does not apply scale to pivot offsets (`src/render/symbols.rs:84-135`), topmarks detach from buoys at non-unit scales. Unverified in current session but worth auditing — this is a plausible reason user says buoys look "less crisp" — the topmark is drifting.

### Gap 14. No Simplified symbol toggle (prior Gap 20, unresolved)

`src/s52/lookup.rs:TableName::preferred` hard-codes Paper for Points. Simplified table is parsed but unused. OpenCPN's "simplified symbols" user setting would render far more compact buoys/beacons — noticeably different style. This is part of why the user says "buoys look different style".

### Gap 15. Sounding dedupe radius & SCAMIN

`src/tiles/builder.rs:2328-2385` runs SCAMIN per-sounding using the tile's z-implied scale. That's fine for tile stability, but it means soundings can disappear suddenly at tile-boundary zoom changes (one tile at z=12 filters them out, neighbour at z=13 keeps them). OpenCPN uses the camera's live scale. Minor visual.

---

## LOW — Items inherited from prior audit, still applicable

Items 17 (pattern offset alignment never applied), 19 (no line endpoint caps), 23 (LC circle approximation), 24 (PRTSUR01 excluded) are unchanged from `rendering-gap-analysis.md`. Low priority for the harbour screenshot.

Items 25 (missing CS procedures: SYMINS01, OWNSHP02, VESSEL01/02, LEGLIN02, CLRLIN01, PASTRK01) remain absent. None are chart-rendering gaps — they affect AIS / route overlays.

---

## Previously-"DONE" items re-verified

| Prior claim | Current status |
|---|---|
| Gap 1: S-57 attribute mapping (marked DONE) | Verified DONE. `src/senc/features.rs:14-100` has 54 explicit mappings including CATWRK, CATOBS, TOPSHP, QUASOU, TECSOU, STATUS, RESTRN, EXPSOU, CONRAD, CATREA, CATCAM, BOYSHP, BCNSHP, VERCLR, FUNCTN. Good. |
| Gap 2: Line feature TX/TE text emission (marked DONE) | Partially DONE. `src/tiles/builder.rs:1841-1884` emits labels for TX/TE on lines — but positions them at **bbox-center**, not along the polyline. VALDCO depth contour labels appear only once per polyline regardless of length (see Gap 10 above). |
| Gap 3: Priority-sorted label declutter (marked DONE) | Verified DONE. `src/render/text_layout.rs:402-404` sorts `sorted_indices` descending by `disp_prio`. But the priority is feature-priority, not text-dis (see Gap 12). |
| Gap 5: DASD line pattern (marked DONE) | Not re-audited in this session; `src/s52/instruction.rs` still needs inspection for the added DASD variant if claim is true. Not screenshot-dominant. |

---

## Top fixes to close the visible gap between the two screenshots

Ordered by the *delta in the harbour screenshot* if applied one at a time, largest first.

1. **Change `show_other` default to `false` + remove M_COVR bypass** (Gap 4). *Trivial effort, massive impact*. Deletes green chart-edge rectangles, M_QUAL dashed borders, HRBFAC marina fills, LAKARE fills, and the bulk of "extra dashed lines everywhere". Five minutes of edits. This alone gets ~40-50% of the way to parity on the screenshot.

2. **Fix light-text scale constant (Gap 3)**. *One-line change*. Replace `scale: 0.75` (and `0.65`) at `src/tiles/builder.rs:2212,2229` with the output of `s52_text_scale(11)` — equivalent to scale 1.0. Immediately restores "Fl 3s 4m 3Nm" legibility. No architectural risk.

3. **Replace 5x7 procedural bitmap with a real rasterized font** (Gap 1). *Medium effort, massive impact*. Also requires parsing CHARS in the TX/TE instruction. This is the single largest quality delta after (1) — every label goes from "spindly grey" to "bold black" matching openCPN. Recommend `fontdue` for the rasterizer (pure Rust, no C deps), Arial or DejaVu Sans Bold as the face, two cached atlases (Normal, Bold) at 12pt, scaled dynamically in the vertex shader.

4. **Route soundings through atlas glyphs instead of 7-segment shader** (Gap 2). *Medium effort, high impact*. SOUNDS11/SOUNDG25/SOUNDG01 are already in `assets/symbols/atlas.json` — just emit a chain of SY() instructions from SNDFRM02 and reuse the existing symbol pipeline. Makes "1.5", "2.7", "0.6" appear as the bold black digits the user expects.

5. **Implement ImportantTextOnly filter on `dis` field** (Gap 6). *Small effort*. Separate `dis` from CHARS-bsize (done together with fix 3). Add a setting default=false; flip it to true if the user wants "only the names that really matter". Removes "Lystbadehavn" clutter without touching SEAARE names.

6. **Fix sector light edge cases** (Gap 7). *Trivial*. Make `attribute_float` accept Strings; gate sector-light dedupe on having a sector. Restores the red arc at the harbour entrance if SENC attribute encoding is the culprit.

7. **Score-based LUP matching** (Gap 5). *Small-Medium*. Pays off on buoys with many attributes; probably changes 5-10 specific symbols in the screenshot.

8. **Complete TOPMAR01** (Gap 9). *Small*. Low priority for this screenshot; matters more on charts with rare topmark shapes.

9. **Build Dusk/Night symbol atlases** (Gap 8). *Small-Medium*. Not needed for the current Day-mode screenshots but is a 5-minute-a-line bulk fix.

---

## Appendix — File index of concrete change sites

| What to change | Where (absolute paths) |
|---|---|
| `show_other` default | `src/s52/settings.rs:61` |
| M_COVR category bypass | `src/s52/engine.rs:90-93` |
| UINFG rectangle gate | `src/tiles/builder.rs:1498-1512` |
| Light-text scale hardcode | `src/tiles/builder.rs:2212, 2229` |
| 5x7 bitmap font | `src/render/label.rs:4-7, 270-297`; `assets/shaders/text_atlas.wgsl:73-77` |
| TX/TE parser (add CHARS + dis separation) | `src/s52/instruction.rs:157-179` |
| Sounding procedural shader retirement | `assets/shaders/sounding.wgsl` entire file; `src/s52/cs/sndfrm.rs` emission; `src/render/text.rs:SoundingInstance` |
| `attribute_float` to accept Strings | `src/senc/features.rs:177-184` |
| Sector-light dedupe | `src/tiles/builder.rs:2184-2192` |
| LUP scoring | `src/s52/lookup.rs:393-396` |
| Line label along polyline | `src/tiles/builder.rs:1858-1860` |

---

## Appendix — OpenCPN cross-references for the fixes above

| Fix | OpenCPN source |
|---|---|
| `_parseTEXT` (CHARS parsing) | `doc/openCPN/libs/s52plib/src/s52plib.cpp:1566-1592` |
| Text weight → wx font weight | `doc/openCPN/libs/s52plib/src/s52plib.cpp:2416-2424` |
| Body size normalization | `doc/openCPN/libs/s52plib/src/s52plib.cpp:2465-2488` |
| ImportantTextOnly filter | `doc/openCPN/libs/s52plib/src/s52plib.cpp:2408-2411` |
| `m_nDisplayCategory` default | `doc/openCPN/libs/s52plib/src/s52plib.cpp:309` and on-screen default is STANDARD |
| `FindBestLUP` scored match | `doc/openCPN/libs/s52plib/src/s52plib.cpp:306` |
| `SNDFRM02` atlas glyph chain | `doc/openCPN/libs/s52plib/src/s52cnsy.cpp:2661-2932` |
| `_LIGHTS05/06` sector arc | `doc/openCPN/libs/s52plib/src/s52cnsy.cpp:3532+` (CA(OUTLW,…) private instruction; `RenderCARC` in `s52plib.cpp`) |
| `TextRenderCheck` LIGHTS dedup | `doc/openCPN/libs/s52plib/src/s52plib.cpp:2324-2366` |
| DATCVR01 | `doc/openCPN/libs/s52plib/src/s52cnsy.cpp:89-168` |
