# NavCore2 vs OpenCPN: Rendering Gap Analysis v3 (Post-round-2 regression audit)

**Date**: 2026-04-19
**Method**: Symbolic navigation (serena MCP), XML LUP cross-reference, openCPN `s52plib.cpp` FindBestLUP source reading
**Scope**: Triage the three user-visible regressions introduced by the uncommitted round-2 fixes:
1. White inner-harbour basins (image D, marinas Søhesten/Hummeren/Muslingen/Krabben)
2. White rectangular holes on medium-zoom water (image B)
3. Overview sparseness at startup (image A) — largely a pre-existing issue, but amplified.

**TL;DR**: A single two-line bug in `src/s52/lookup.rs::matches_attributes` combined with the new `lookup_best` scoring logic causes every DEPARE feature to match the **wrong LUP** (the NODTA/PRTSUR01 "no-data" lookup instead of `CS(DEPARE01)`). The round-2 `has_nodata_fill`/`has_prtsur01` suppression in `build_areas` then amplifies this into **deleting most DEPARE fills in any tile where multiple charts overlap**. Fixing `matches_attributes` alone restores all three symptoms.

---

## 1. Root cause — the white basins and white rectangles (HIGHEST PRIORITY)

### 1.1 What changed in round 2

| File | Change | Role in regression |
|---|---|---|
| `src/s52/lookup.rs:405-440` | `lookup_best` was rewritten from "first match wins" to "OpenCPN-style scoring: highest specificity wins, where specificity = count of attribute_codes filters satisfied" | **Direct cause — flips which DEPARE LUP is selected** |
| `src/s52/lookup.rs` | Added `TableName` to the hash key; `lookup_best` now takes it as a parameter | Unrelated to regression (correctness improvement, no symptom shift) |
| `src/s52/engine.rs:153-175` | Added `resolve_cache` so repeated resolutions hit cache; switched from `feature.object_class.acronym()` to `s57_code_to_acronym(feature.type_code)` | Side-effect: every feature now resolves to a canonical acronym (good); doesn't itself cause white areas, but means every DEPARE now reliably hits the broken LUP logic |
| `src/tiles/builder.rs:1289-1307` | Added `has_nodata_fill`, `has_prtsur01` detection and the `continue;` that **drops the feature entirely** when a more-detailed chart overlaps the tile | **Amplifier — turns a soft styling glitch into a deletion** |
| `src/tiles/builder.rs:1518-1519` | Added `suppress_area_boundary_lines = feature.is_depth_area() && (has_nodata_fill || has_prtsur01)` | Deletes DEPARE boundary lines in every DEPARE, because `has_nodata_fill` is now universally true (see 1.3) |
| `src/s52/cs/depare.rs:74-88` | Four-shade DEPARE02 promotion now requires BOTH `drval1 >= threshold` AND `drval2 > threshold` | Neutral for the regression; doesn't cause white. See side-effect audit in §4. |
| `src/s52/settings.rs:61` | Comment-only update. `show_other: false` was already the default on HEAD. | No behavior change |

### 1.2 The S-52 LUP intent for DEPARE

`assets/s52/chartsymbols.xml:710-729` defines two DEPARE Plain/Area entries:

```xml
<lookup id="39" RCID="32075" name="DEPARE">     <!-- "DEPARE without depth attributes" -->
    <table-name>Plain</table-name>
    <attrib-code index="0">DRVAL1?</attrib-code>
    <attrib-code index="1">DRVAL2?</attrib-code>
    <instruction>AC(NODTA);AP(PRTSUR01);LS(SOLD,2,CHGRD)</instruction>
    <display-cat>Displaybase</display-cat>
</lookup>
<lookup id="40" RCID="32076" name="DEPARE">     <!-- "DEPARE with depth attributes" -->
    <table-name>Plain</table-name>
    <instruction>CS(DEPARE01)</instruction>
    <display-cat>Displaybase</display-cat>
</lookup>
```

The `?` suffix on `DRVAL1?` / `DRVAL2?` is **S-52 negative attribute filter syntax**: the LUP matches only if the attribute is **absent** from the feature. This is how OpenCPN distinguishes a DEPARE with no depth attributes (unsurveyed cell — paint NODTA grey) from a normal DEPARE (paint via `CS(DEPARE01)`). Every real DEPARE on a Danish ENC has DRVAL1/DRVAL2, so LUP 39 is meant to match **only unsurveyed polygons**; LUP 40 is meant to match **everything else**.

Source of this interpretation: `doc/openCPN/libs/s52plib/src/s52plib.cpp:810-825`:

```c++
// special case (ii)
// TODO  Find an ENC with "UNKNOWN" DRVAL1 or DRVAL2 and debug this code
if (!strncmp(slatv, "?", 1)) {    // if LUP attribute value is "undefined"
    //  Match if the object does NOT contain this attribute
    goto next_LUP_Attr;
}
```

(OpenCPN's implementation of `?` is itself subtly wrong — the `goto` sits inside the "attribute is present" branch, so it fails to increment `countATT`, and the outer while-loop that runs when the attribute is absent also never increments. Net effect: LUP 39 **never reaches `candidate_score == 1.0`** and always loses to LUP 40. That's the de-facto behaviour we have to reproduce. See scoring at `s52plib.cpp:905-919`.)

### 1.3 What navcore actually does with `?`

`src/s52/lookup.rs:208-252` `matches_attributes()` — unchanged from round 2:

```rust
for code in &self.attribute_codes {
    if code.len() < 6 { continue; }
    let attr_name = &code[..6];          // "DRVAL1"
    let value_str = &code[6..];          // "?"
    if value_str.is_empty() {
        if !attributes.contains_key(attr_name) { return false; }
        continue;
    }
    let Ok(expected_value) = value_str.parse::<i32>() else {
        continue;                         // <-- ★ "?" lands here and is SKIPPED
    };
    ...
}
```

`"?".parse::<i32>()` errors → the branch executes `continue;`, meaning **the filter is silently dropped**. The for-loop ends without having returned `false`, so the outer function returns `true` (line 252). That is, **DRVAL1?/DRVAL2? filters are universally satisfied** regardless of whether the feature has DRVAL1/DRVAL2.

On HEAD this was already broken in the same way. What saved us on HEAD was the **old** `lookup_best` which used first-match among *filtered* entries and then fell back to the generic no-attr LUP **only** if no filtered entry matched. Because LUP 39 came first in the XML, the old code returned LUP 39 for **all** DEPARE features too — this has been latently broken since the parser was written. **But round-2's downstream additions turned it from latent into visible.**

### 1.4 The round-2 amplifier

`src/tiles/builder.rs:1289-1307` (NEW in round 2):

```rust
let has_nodata_fill = resolved.instructions.iter().any(|instr| {
    matches!(instr, RenderInstruction::AreaColor { color } if color == "NODTA")
});
let has_prtsur01 = resolved.instructions.iter().any(|instr| {
    matches!(instr, RenderInstruction::AreaPattern { pattern } if pattern == "PRTSUR01")
});
if !detailed_coverages.is_empty() && (has_nodata_fill || has_prtsur01) {
    continue;    // <-- drops the entire feature
}
```

Since LUP 39 is always selected, every DEPARE's `resolved.instructions` contains `AC(NODTA)` and `AP(PRTSUR01)` → both predicates are always true. For every chart in a multi-chart tile that is not the most detailed, `detailed_coverages` is non-empty, so **every DEPARE is skipped**. The chart renders with DEPARE-shaped holes.

The user sees:
- **Image B (white rectangles)**: The skipped DEPARE polygons are typically chart-cell-shaped because ENC producers often draw a single large DEPARE covering the "deep water" part of a cell. When it is skipped, `OCEAN_BACKGROUND` (RGB 212,234,238 — defined in `src/render/colors.rs:15`, identical to DEPDW/CHWHT in the Day palette) shows through as the clear-colour. That is **slightly whiter/paler than DEPVS (115,182,239)**, which is the color of the shallower DEPARE that did render in neighbouring cells → visible rectangular "brighter than the water" holes with straight sharp edges.
- **User's verbatim complaint "inland waters ... disappeared"**: Not LAKARE (already hidden by `show_other=false`, unchanged from HEAD), but the **harbour-water DEPARE** and **outer-harbour DEPARE** polygons that lie inside a more-detailed chart's extent. When the user zooms in to image D the detailed chart is the most detailed and `detailed_coverages` is empty → DEPARE survives → image D looks almost right again.

The matching `suppress_area_boundary_lines` at `builder.rs:1518-1519` also fires for every DEPARE (again because `has_nodata_fill = true` universally), deleting depth-contour-style LS() strokes even when the fill is drawn.

### 1.5 The white basins in image D

Image D is zoomed-in, single-chart territory, so `detailed_coverages` is empty and the skip in §1.4 does NOT trigger. The DEPARE fills do render. The marina basins themselves (`Søhesten`, `Hummeren`, …) are **DRGARE** (dredged) and go through a different code path — DRGARE has only one LUP (id 45) with zero `attribute_codes`, so the new `lookup_best` returns it directly and `CS(DEPARE01)` expands to `AC(DEPVS);LS(DASH,1,CHGRF)`. DRGARE should render correctly as DEPVS.

However, there is a **second, independent bug** visible here: at `src/render/state.rs:13` and `1605-1610` the frame is cleared to `OCEAN_BACKGROUND = (0.831, 0.918, 0.933)` = **exactly CHWHT/DEPDW in the Day palette**. If a marina's DRGARE is drawn at the **same priority** as the enclosing harbour DEPARE (both are Group 1 → priority 0, see `src/s52/lookup.rs:83` `Group1 → 0`), the last-drawn wins. Draw order within a priority band is vertex-buffer order, which is feature-insertion order from the SENC. If DEPARE is ingested after DRGARE for this chart, **the deep-water DEPARE overpaints the shallow DRGARE** → marinas look like DEPDW (pale blue) — the very colour that is also the clear colour. The user reads it as "pure white".

OpenCPN solves this via a stable rule: DRGARE's LUP is placed in an explicitly higher slot in its `razRules` 2-D table than DEPARE. See `doc/openCPN/libs/s52plib/src/s52plib.cpp` search-tag `FindBestLUP`, and the LUP-loading order in `chartsymbols.cpp` — DRGARE is loaded after DEPARE so that its razRule sits on top. navcore lumps both into the same priority bucket (0) and sorts only on `disp_prio`, so OSENC feature order leaks into render order.

**Confirm this hypothesis** by instrumenting `build_areas` to log DRGARE and DEPARE priorities and sequence numbers inside the Ishøj harbour tile — if DRGARE is emitted before DEPARE into `all_area_ranges`, that's the ordering bug.

### 1.6 Fixes

Ordered by impact on the user's screenshot.

#### Fix A (highest leverage, ~5 lines) — make `matches_attributes` interpret `?` correctly

In `src/s52/lookup.rs:208-252`, insert a `?` branch **before** the `value_str.is_empty()` check. Per S-52 spec the `?` value means "attribute must not be present":

```rust
if value_str == "?" {
    if attributes.contains_key(attr_name) {
        return false;
    }
    continue;
}
```

Effect on the three symptoms:
- LUP 39 now fails for every DEPARE that has DRVAL1 or DRVAL2 (i.e. all normal DEPARE) → `lookup_best` falls through to LUP 40's `CS(DEPARE01)` → depare02 gets invoked → resolved instructions contain `AC(DEPVS)` (or DEPMS/MD/DW), not NODTA.
- `has_nodata_fill` and `has_prtsur01` become false for every normal DEPARE → the overzealous skip in `builder.rs:1305` no longer fires → DEPARE renders in multi-chart tiles.
- `suppress_area_boundary_lines` likewise becomes false → depth-area boundaries render again.

This single change is expected to restore both the white rectangles in image B and the "inland waters disappeared" symptom (where "inland waters" refers to harbour DEPARE polygons that lay inside more-detailed chart overlays).

#### Fix B (safety net, also in `matches_attributes`) — treat unknown filter values as no-match, not skip

Still in `src/s52/lookup.rs:236-238`:

```rust
let Ok(expected_value) = value_str.parse::<i32>() else {
    return false;   // was: continue;
};
```

This keeps silent permissive behavior from ever resurrecting through some new filter syntax the XML may introduce. Defence-in-depth only; Fix A alone resolves the observed symptoms.

#### Fix C (scope-reduce the `has_nodata_fill` skip)

Even after Fix A, the skip at `builder.rs:1305` is conceptually overreaching: it drops a NODTA polygon whenever *any* more-detailed chart overlaps the tile, not only where the detailed chart's coverage actually covers that polygon. If a large harbour DEPARE is partly inside a detailed overlay and partly outside, the outside half should still render. Replace the blanket `continue` with:

```rust
// Only skip when this specific feature's centroid falls inside the detailed
// overlay; otherwise we should still portray it.
if has_nodata_fill || has_prtsur01 {
    let (cx, cy) = feature_centroid_mercator(...);  // using actual_bounds below
    if point_in_any_polygon([cx, cy], &detailed_coverages) {
        continue;
    }
}
```

Effort: trivial — `point_in_any_polygon` already exists (`builder.rs:536`, `591`, etc.). This makes the behaviour match OpenCPN's fine-grained quilting rather than a per-tile blanket.

#### Fix D (DRGARE-over-DEPARE draw order — the marina white basins)

Two options; pick the cheapest first.

1. **Cheap**: in `build_areas`, after resolving a DRGARE feature, bump its `feature_priority` by +1 (or place DRGARE into a sub-priority 0.5 ordering key). Emit DRGARE with priority `min(priority + 1, 9)` so it always draws after DEPARE of the same LUP group.
2. **Correct**: replicate OpenCPN's razRules 2-D ordering. Extend `all_area_ranges` to carry a secondary sort key derived from the LUP id (lower chartsymbols.xml lookup-id = earlier draw, higher id = later). Use the LUP id from `LookupEntry` (easy — it's already in the XML and the parser could save it).

The cheap option takes two lines. Recommend doing it now and deferring the correct option.

---

## 2. White rectangles at medium zoom (image B)

Fully explained by §1 — these are chart-cell-shaped DEPARE holes exposed by the `has_nodata_fill` over-skip. Fix A + Fix C close them. No further investigation needed for this symptom.

One secondary check: the user described the rectangles as "noticeably whiter than the pale-blue surrounding water". Verify:
- `src/render/colors.rs:15`: `OCEAN_BACKGROUND = [0.831, 0.918, 0.933]` = RGB(212, 234, 238).
- `assets/s52/chartsymbols.xml:33` Day palette: `CHWHT = DEPDW = (212, 234, 238)`, `DEPVS = (115, 182, 239)`, `DEPMS = (152, 197, 242)`.

So the "whiter" rectangles are exactly OCEAN_BACKGROUND (= DEPDW/CHWHT) showing through, while the "water" around them is DEPVS or DEPMS (both a saturated blue). Consistent.

---

## 3. Overview sparseness (image A)

Mostly **pre-existing behaviour, not a round-2 regression**; only the label clutter is a new artefact.

### 3.1 What survives at Denmark-scale zoom (view_scale ≈ 4,000,000)

Per `src/tiles/builder.rs:90-139` `should_render_at_scale_ex`:
- `bypass_scamin = true` for Displaybase features → LNDARE, DEPARE, COALNE etc always survive.
- Non-Displaybase features with no SCAMIN attribute fall to the `SUPER_SCAMIN` limit at `chart_native_scale * 2`. At overview zoom this culls most Standard features from small-scale charts (good).
- But: LNDARE coastline is a COALNE LINE feature, not the LNDARE area boundary. COALNE is Displaybase too, should pass. If it is missing, investigate whether the Danish overview ENC actually encodes COALNE as line geometry or whether lines are SUPER_SCAMIN-culled before reaching render.

### 3.2 Why the user sees so little geometry

Hypotheses in priority order:
- **H1 (most likely)**: At this zoom the active chart tiles are the DK overview cells. DEPARE is the dominant feature class, and every DEPARE is hitting the bug in §1. In **single-chart tiles** (no detailed overlay at overview zoom), DEPARE is NOT skipped (line 1305 needs `detailed_coverages` non-empty). So DEPARE fills do render — good. But their **boundary lines are suppressed** by `suppress_area_boundary_lines` (also §1) → no depth-contour-like visual at all. The user sees flat uniform sea.
- **H2**: TSS boundaries (TSSLPT, TSSBND) are Display category Standard by default in S-52 and have SCAMIN set by the chart. At 1:4M they may be legitimately SCAMIN-filtered. Not a bug.
- **H3**: DEPCNT (depth contours) are lines, Displaybase. They should survive. Check that they are emitted. If any DEPCNT falls to the same `has_nodata_fill` code path in the line builder, also check `build_lines` for similar over-skipping.

Action: once Fix A lands, re-test overview — most of the "sea is uniform" issue will resolve because the DEPARE boundary lines come back.

### 3.3 Label clutter at overview (big bold labels everywhere)

This is a **regression-adjacent** outcome of round-2's successful font fix. The bitmap font now looks good, so labels are legible and visible. Without SCAMIN or `ShowImportantTextOnly` filtering, every single SEAARE, LNDRGN, LNDARE, HRBARE label ends up on screen.

`src/tiles/builder.rs:1680-1701` pushes labels into `label_candidates` AFTER the feature's own SCAMIN check, but:
- SEAARE (class 122) labels are Displaybase → `bypass_scamin=true` → label pushed regardless of zoom.
- `show_important_text_only` flag exists (`src/s52/settings.rs:55`) but is **not applied anywhere**: search `grep -rn show_important_text_only src/` shows only declarations and a hash entry in `engine.rs`, no read-site filter in the builder or renderer.
- `dis` field is used in `text_layout.rs:418` only as a **tie-breaker** for decluttering, not as a **filter**.

Fix E (small): in `declutter_and_layout_labels` at `src/render/text_layout.rs:400`, before sorting, apply:

```rust
let important_only = candidates.len() > 0 /* thread through MarinerSettings */;
let candidates: Vec<&TextParams> = candidates.iter()
    .filter(|c| !important_only || c.dis < 20)
    .collect();
```

Or, simpler and more efficient, apply the filter at `build_areas` label-push time (e.g. `src/tiles/builder.rs:1690`), so declutter doesn't waste cycles on discarded labels. Default the setting to `true` at overview scales and `false` at harbour scales (LOD-aware) if the user wants automatic behaviour. Or simply default it to `true` — OpenCPN's on-screen default enables ImportantTextOnly.

Effort: ~10 lines. Expected impact: image A stops looking like it exploded in text.

---

## 4. Round-2 side-effects audit

Reading the non-lookup diffs against HEAD for correctness hazards.

### 4.1 `src/s52/cs/depare.rs:74-88` (round-2 stricter promotion)

Before: used `drval1` alone for all three thresholds.
After: requires `drval1 >= T && drval2 > T` for each threshold.

Concern raised in the task brief: does `DRVAL1=0, DRVAL2=0` (common "surface-touching dredged spot") fail to upgrade, landing as Drying/DEPIT (green) instead of DEPVS (blue)?

- Step through for DRVAL1=0, DRVAL2=0 → helper normalises to `drval2 = drval1 + 0.01 = 0.01` (lines 48-53). So `drval1=0 && drval2=0.01 > 0` → promoted to `VeryShallow` (DEPVS, blue). ✓
- For DRVAL1=missing, DRVAL2=missing → `drval1 = -1.0`, `drval2 = -0.99` → stays Drying (DEPIT, green). That maps to the degenerate "empty" DEPARE which should never happen in practice (OSENC strips such objects).
- For DRVAL1=3, DRVAL2=30 around the 2 m shallow threshold: `drval1=3 >= 2 && drval2=30 > 2` → MediumShallow. Around the 10 m safety threshold: `drval1=3 >= 10` false → stays MediumShallow. No over-promotion (which is the bug the round-2 fix was meant to prevent). ✓

**Verdict**: DEPARE02 promotion logic is correct. Not a source of the white-basin symptom.

### 4.2 `src/s52/cs/depare.rs:112-124` (DRGARE boundary)

Emits only `LS(Dashed, 1, CHGRF)`. OpenCPN's DEPARE01 for DRGARE additionally emits `AP(DRGARE01)` (a horizontal-hatch pattern). Comment in the code notes the intentional deferral. Not a regression.

### 4.3 `src/s52/engine.rs:90-93` (display-category bypass narrowed)

Bypass list shrunk from `{69, 301, 302}` (Lake, M_CSCL, M_COVR) to `{301}` (M_CSCL only). This is correct — before round 2, M_COVR was force-enabled even when the user hid Other, which put a green rectangle on screen. Now M_COVR obeys `show_other: false`. No regression.

### 4.4 `src/s52/engine.rs:153-175` (resolve_cache)

The cache keys on `(type_code, geom_type, attr_hash, settings_hash)` but **not on the chart native scale or view scale**. If a feature is resolved in a multi-chart tile under one view-scale regime and then the camera zooms, the cache returns stale instructions. In practice `resolve_feature` does not depend on scale, so this is OK. Verified: the resolve path only reads `feature.attributes` and `self.settings`. ✓

### 4.5 `src/render/label.rs:112-120` (nearest → linear filter)

Turning on linear filtering for the font atlas does mean **sub-pixel glyph outlines will anti-alias against the atlas's background cells**. If neighbour cells in the atlas are populated with glyph strokes, bilinear filtering can bleed those strokes into the current glyph. Visual evidence in image A (letters look slightly fat and fuzzy, e.g. "Krabben") is consistent with this. Not a regression in correctness — just a quality tradeoff. OpenCPN uses properly dilated glyph atlases with 1-px padding between glyphs, which is what navcore should eventually do.

### 4.6 `src/tiles/builder.rs:1508-1526` (`draw_chart_outline`)

```rust
let draw_chart_outline = feature.object_class == ObjectClass::Coverage
    && self.s52_engine.map(|e| e.settings.show_chart_boundaries).unwrap_or(false);
```

With the default `show_chart_boundaries = false` and M_COVR no longer force-enabled (§4.3), Coverage features are filtered by display-category in the engine (M_COVR is Other) and never reach this branch. No green chart-outline boxes. Correct.

### 4.7 `src/render/text_layout.rs:418` (dis tie-break)

Sort key changed to `(-disp_prio, dis)`. Correct direction — higher-priority features win, ties broken by `dis < 20` first. Does **not** filter, only orders. Means overview labels still all push to `label_candidates`; decluttering then drops some but the stampede is still huge.

---

## 5. Prioritized fix list (screenshot-impact-ordered)

1. **Fix A — `matches_attributes` interprets `?` per S-52 spec** (`src/s52/lookup.rs:208-252`, ~5 lines).
   Removes "every DEPARE resolves to NODTA" chain reaction. Restores water fills in multi-chart tiles, restores depth-area boundary lines everywhere. **Single biggest pixel-delta fix in this round.**

2. **Fix B — return false on unknown filter syntax** (same function, 1 line).
   Hardens the parser. Defence in depth after Fix A.

3. **Fix C — scope the `has_nodata_fill` skip to actual overlap** (`src/tiles/builder.rs:1305`, ~10 lines).
   Makes the no-data-quilting match OpenCPN's per-feature behaviour instead of a per-tile blanket. Strictly an improvement even with Fix A in place (genuinely unsurveyed small cells should still be suppressed only where a better chart covers them).

4. **Fix D — DRGARE draws after DEPARE at same priority** (~2 lines for the cheap version in `build_areas`).
   Restores the DEPVS blue inside marina basins in image D. Even post-Fix-A, the ordering ambiguity can re-expose a DEPDW DEPARE on top of a DEPVS DRGARE depending on OSENC feature order.

5. **Fix E — apply `show_important_text_only` / dis filter at label-push or declutter time** (`src/tiles/builder.rs:1690` or `src/render/text_layout.rs:404`, ~10 lines).
   Fixes image A overview label stampede. Default the setting to `true`.

6. **Pre-existing — SUPER_SCAMIN on TSS line classes** (lower-priority; for the overview TSSBND/TSSLPT missing).

7. **Pre-existing — COALNE / DEPCNT visibility at overview**. Verify after Fix A — many of the "lines I expected" may simply come back when DEPARE boundary suppression is lifted.

---

## 6. Verification procedure

After landing Fix A:
- Log the LUP instruction at `src/s52/engine.rs:166` with `log::debug!` already present. For a DEPARE feature with DRVAL1=5, DRVAL2=10, the log should now show `instr='CS(DEPARE01)'` instead of `instr='AC(NODTA);AP(PRTSUR01);LS(SOLD,2,CHGRD)'`.
- Inspect `has_nodata_fill` at `builder.rs:1289` under a debugger on the same DEPARE: should be `false`.
- Re-run the Ishøj screenshot: basins should be DEPVS blue, not OCEAN_BACKGROUND pale.
- Re-run the Møn image B screenshot: rectangular holes disappear.

After landing Fix E:
- Re-run overview: place-name labels drop from hundreds to tens; only dis<20 (navigationally important names) remain.

---

## 7. Appendix — concrete file:line fix sites

| Fix | File | Lines |
|---|---|---|
| A | `src/s52/lookup.rs` | insert at 228 (before `if value_str.is_empty()`) |
| B | `src/s52/lookup.rs` | 236-238 (`continue;` → `return false;`) |
| C | `src/tiles/builder.rs` | 1305 — replace `continue;` with centroid-gated skip |
| D (cheap) | `src/tiles/builder.rs` | 1373 — bump `feature_priority` for DRGARE |
| E | `src/tiles/builder.rs` | 1690 (or `src/render/text_layout.rs:404`) — apply `show_important_text_only` filter |
| (readability only) | `src/s52/cs/depare.rs` | 50-66 — document why `drval2 = drval1 + 0.01` restores promotion for DRVAL1=0,DRVAL2=0 |

Cross-references to OpenCPN source:
- LUP scoring: `doc/openCPN/libs/s52plib/src/s52plib.cpp:790-945` (function `LUPArrayIndex` / `FindBestLUP`).
- `?` attribute semantics: `s52plib.cpp:810-825`.
- DRGARE over DEPARE ordering: look-up-table load order in `chartsymbols.cpp` plus razRules 2-D indexing in `s52plib.cpp`.
- Depth-area CSP: `s52cnsy.cpp:617-688` (`_DEPARE01`) and `s52cnsy.cpp:2661-2932` (`_SNDFRM02`).
