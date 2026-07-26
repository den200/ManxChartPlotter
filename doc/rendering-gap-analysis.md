# NavCore2 vs OpenCPN: Rendering Gap Analysis

**Date**: 2026-04-16
**Method**: Source-level comparison of navcore2 (`src/`) against OpenCPN (`doc/openCPN/libs/s52plib/src/`)

---

## TL;DR

NavCore2 has a solid tiled GPU rendering architecture with working S-52 lookup,
priority-ordered drawing, cross-tile text decluttering, palette switching for
areas and lines, LC complex line patterns, stencil-based multi-chart masking,
and 14 CS procedures. However, it falls short of OpenCPN in several specific
areas. The gaps are ranked below from most to least impactful.

---

## CRITICAL: Gaps Affecting Navigation Safety Display

### 1. Incomplete S-57 Attribute Code Mapping

**The single biggest correctness issue.** The SENC parser maps S-57 numeric
attribute codes to string names via `s57_attribute_name()`. Only **27 codes** are
mapped. Every unmapped code becomes `"ATTR_NNN"`, making it invisible to CS
procedures that look up attributes by name.

**File**: `src/senc/features.rs:17-58`

**Missing codes used by CS procedures**:

| Attribute | S-57 Code | Used By | Effect When Missing |
|-----------|-----------|---------|---------------------|
| CATWRK | 71 | `wrecks.rs:85` | All wrecks render as generic (category unknown) |
| CATOBS | 39 | `obstrn.rs:94` | Obstructions don't distinguish foul ground vs rock |
| TOPSHP | 171 | `topmar.rs:31` | All topmarks get default symbol regardless of shape |
| QUASOU | 126 | `wrecks.rs:87`, `sndfrm.rs:232` | Unsafe wrecks may not be flagged; sounding uncertainty lost |
| TECSOU | 166 | `sndfrm.rs:210` | Swept depth indicator never shown on soundings |
| STATUS | 151 | `sndfrm.rs:245` | Doubtful soundings not marked |
| RESTRN | 131 | `restrn.rs:29`, `resare.rs:81` | All restricted areas render as generic |
| EXPSOU | 90 | `wrecks.rs:86`, `obstrn.rs:93` | Exposure status ignored for obstructions/wrecks |
| CONRAD | 82 | `qualin.rs:30`, `quapos.rs:26` | Radar conspicuity not checked for quality assessment |
| CATREA | 56 | (used in LUP attr matching) | Restricted area sub-types not distinguished |
| CATCAM | 14 | (used in LUP attr matching) | Cardinal buoy types not distinguished |
| BOYSHP | 4 | (used in LUP attr matching) | Buoy shape ignored in LUP selection |
| BCNSHP | 3 | (used in LUP attr matching) | Beacon shape ignored in LUP selection |
| VERCLR | 181 | (TX/TE text formatting) | Vertical clearance values not shown on bridges/cables |
| FUNCTN | 94 | (used in LUP attr matching) | Building function not distinguished |

**Impact**: Virtually every CS procedure silently falls to defaults. Wrecks,
obstructions, topmarks, restricted areas, and sounding quality indicators all
render incorrectly.

**Fix**: Add ~20 missing S-57 code mappings to the match statement. Trivial fix,
massive impact.

---

### 2. No Category-Mutable Promotion (DISPLAYBASE Promotion)

OpenCPN's CS procedures can **promote** features to DISPLAYBASE when they are
safety-critical. For example, an OBSTRN in shallow water gets promoted so it
remains visible even when the user hides "Standard" category features. OpenCPN
tracks this via `m_bcategory_mutable` on S57Obj.

**NavCore2**: `resolve_feature()` in `src/s52/engine.rs:170-174` returns the
LUP's static display category. CS procedures cannot modify it. There is no
mechanism for runtime category promotion.

**Affected features**:
- OBSTRN, WRECKS: Dangerous obstructions should be DISPLAYBASE
- DEPCNT: Safety contour should be DISPLAYBASE
- UWTROC: Underwater rocks in shallow water should be DISPLAYBASE

**Impact**: If a user sets display category to "DISPLAYBASE only", dangerous
obstructions and the safety contour vanish. This is a safety issue per IHO S-52.

---

### 3. UDWHAZ03 Spatial Query Missing (Isolated Danger Detection)

OpenCPN's `_UDWHAZ03()` (`s52cnsy.cpp:521-608`) performs **spatial intersection
queries** -- it finds all DEPARE/DRGARE features that contain the danger object
and checks whether the surrounding depth area is already shallow.

**NavCore2** (`obstrn.rs:74-81`, `wrecks.rs:66-72`): Uses a simplified heuristic
-- just compares VALSOU against safety_contour. No spatial query.

```rust
// obstrn.rs:74 - simplified, no spatial context
fn is_isolated_danger(valsou: Option<f64>, settings: &MarinerSettings) -> bool {
    if let Some(depth) = valsou {
        depth < settings.safety_contour as f64 && depth >= 0.0
    } else { false }
}
```

**Impact**: An obstruction at 5m in a DEPARE with DRVAL1=3m (already shallow
water) gets incorrectly flagged as an isolated danger with ISODGR51 symbol.
OpenCPN would correctly suppress it.

---

## HIGH: Gaps Visibly Affecting Rendering Fidelity

### 4. No Score-Based LUP Ranking (Attribute Matching)

OpenCPN's `FindBestLUP()` (`s52plib.cpp:306`) does **scored attribute matching**:
for each candidate LUP, it counts matched vs total attribute filters and picks
the highest-scoring match. First 100% match wins; fallback to default (no-attr)
LUP.

**NavCore2** (`src/s52/lookup.rs:393-396`): Uses **first-match** -- iterates
candidates and returns the first one whose attributes don't conflict, without
scoring partial matches.

**Impact**: For features with multiple applicable LUPs of varying specificity
(e.g., a BOYLAT with CATLAM=1 and COLOUR=3,1), a less-specific LUP may be
selected over a better-matching one.

---

### 5. Line Feature Text Labels Dropped

`build_lines()` in `src/tiles/builder.rs:1833` skips all non-line instructions
with `_ => {}`. Any TX/TE instructions on line features are silently dropped.

**Missing text**: depth contour labels (VALDCO), traffic separation zone names,
cable overhead clearances, pipeline labels.

**Impact**: Depth contours are rendered but never labeled with their depth value.
This is a significant navigation information loss.

---

### 6. Symbol Atlas: Day Palette Only

NavCore2's symbol atlas is built from `rastersymbols-day.png` only
(`tools/extract_symbols.py:22`). OpenCPN ships 5 raster symbol sets (Day, Dusk,
Night, plus simplified/paper variants).

When `switch_palette()` is called (`state.rs:1283`), it updates the area/line
color palette buffer but **not the symbol atlas texture**. Symbols show Day
colors even in Dusk/Night mode.

**Impact**: Buoys, beacons, lights, and all point symbols appear too bright in
Night mode, defeating the purpose of night palette.

---

### 7. Render Pass Architecture Difference

**OpenCPN**: 5 separate render passes, each iterating priority 0-9:
```
Pass 1: ALL area fills at all priorities
Pass 2: ALL area boundaries at all priorities
Pass 3: ALL line features at all priorities
Pass 4: ALL point features at all priorities
Pass 5: ALL text (areas->lines->points per priority)
```

**NavCore2**: Single pass with priority loop 0-9, geometry types interleaved
within each priority level:
```
For priority 0..9:
    Areas -> Patterns -> Lines -> Symbols
Then: Soundings -> Labels
```

**Difference**: In OpenCPN, a Line feature at priority 2 draws AFTER all area
fills (including fills at priority 8), because all fills are in Pass 1. In
navcore2, a line at priority 2 draws before an area fill at priority 3.

NavCore2's approach is arguably more correct per S-52 spec (priority dominates),
but produces visibly different layering than OpenCPN for cross-priority
interactions.

---

### 8. QUAPNT01 Missing (Point Quality Symbols)

OpenCPN's QUAPNT01 (`s52cnsy.cpp:2177-2220`) handles quality-of-position for
point objects. Returns `SY(QUAPOS01/02/03)` depending on accuracy. Called as a
sub-procedure by OBSTRN04 and WRECKS02.

**NavCore2**: Has QUALIN01 (line) and QUAPOS01 (general) but NOT QUAPNT01. The
dispatch in `cs/mod.rs:64-89` has no "QUAPNT01" entry.

**Impact**: Point obstructions and wrecks with uncertain positions (QUAPOS 2-9)
get no quality indicator symbol.

---

### 9. TOPMAR01 Incomplete (Platform Detection + TOPSHP 14-33)

OpenCPN (`s52cnsy.cpp:2934-3193`) has **two** complete symbol tables:
- **Floating platform** (buoys): TOPSHP 1-33
- **Fixed platform** (beacons): TOPSHP 1-33 with different symbols

It uses `_atPtPos()` to detect whether the topmark is on a buoy or beacon.

**NavCore2** (`topmar.rs:31-48`): One table, TOPSHP 1-13 only. No platform
detection. TOPSHP 14-33 (T-shape, diamond, rectangle, rhombus, flag, etc.) all
default to "TOPMAR01".

**Impact**: ~20 topmark shapes render as wrong symbol. All topmarks use
floating-platform symbols regardless of buoy vs beacon.

---

### 10. No Priority Sorting for Label Declutter

OpenCPN's `CheckTextRectList` is priority-sorted: higher-priority text always
wins collision resolution.

**NavCore2** (`src/render/text_layout.rs:389-431`): `declutter_and_layout_labels`
uses insertion order, not display priority. A low-priority label inserted first
can block a higher-priority one.

---

## MEDIUM: Gaps Affecting Visual Completeness

### 11. No DASD (Dash-Dot) Line Pattern

OpenCPN supports 4 pen styles: SOLD, DASH, DOTT, DASD. NavCore2's
`LinePattern` enum (`src/s52/instruction.rs:8-15`) only has Solid, Dashed,
Dotted. The fragment shader (`assets/shaders/line.wgsl:117-131`) uses a simple
two-phase pattern that cannot express dash-dot-dash-dot sequences.

**Impact**: Features using DASD (e.g., electronic bearing lines, some boundaries)
render with the wrong dash pattern.

---

### 12. No Shared Edge Priority Promotion (SetLineFeaturePriority)

OpenCPN's `SetLineFeaturePriority()` promotes shared edges to the maximum
priority of all referencing features. If a DEPARE (priority 2) and DEPCNT
(priority 5) share boundary segments, the edge gets priority 5.

**NavCore2**: Each feature's lines get the feature's own `disp_prio` from S-52
lookup. Shared edges can z-fight or be incorrectly covered.

---

### 13. General Symbol Rotation Not Applied

The `SymbolInstance` struct (`src/render/symbols.rs:84`) has a `rotation` field
and the shader supports it, but rotation is only populated for LIGHTS features
(`src/tiles/builder.rs:2064-2073`). All non-LIGHTS features get `rotation=0`.

The S-52 SY() instruction's rotation parameter is not parsed
(`src/s52/instruction.rs:75` -- only stores `name`). Features like CURRENTS,
TSSLPT with ORIENT attributes will not rotate.

---

### 14. LIGHTS: Faint Sector Rendering Unclear

`lights.rs:245` computes a `faint` flag for obscured/faint sectors (LITVIS
3/7/8). OpenCPN renders these with CHBLK outline + CHBRN fill. It's unclear if
navcore2's rendering pipeline uses this flag to alter arc appearance.

---

### 15. LIGHTS: Sequential-Only Colocation Detection

NavCore2 (`builder.rs:2003-2004`) tracks `last_light_pos` and deduplicates
against only the previous light position. If co-located lights are non-adjacent
in the feature list, duplicates appear. OpenCPN maintains a full list of
processed positions.

---

### 16. DEPARE02: drval2 Not Checked in 4-Shade Mode

NavCore2 (`depare.rs:76-87`) uses drval1-only for progressive depth zone
classification. OpenCPN's `_SEABED01` checks BOTH drval1 AND drval2 for each
threshold. Minor practical difference since drval2 > drval1 in valid data, but
it's a spec divergence.

---

### 17. 7-Segment Sounding Digits vs S-52 Font Symbols

OpenCPN renders soundings using chartsymbols atlas glyphs (SOUNDS11 for shallow,
SOUNDG25 for deep). NavCore2 uses procedural 7-segment rendering in
`assets/shaders/sounding.wgsl:81-141`.

Functionally equivalent (same depth/color logic, same drying/swept/uncertainty
indicators) but **visually distinctly different** from any reference implementation.

---

### 18. Bitmap Font Only for Labels (5x7 Hardcoded)

`src/render/label.rs:301-398` uses a procedurally-generated 5x7 bitmap font
atlas (96 ASCII chars). No support for S-52 text size variations, font weight,
or proportional spacing. All text renders at the same monospace bitmap font, just
scaled.

---

## LOW: Minor Gaps and Missing Non-Chart Features

### 19. No Line Endpoint Caps

`src/render/line_vertices.rs:30-32` produces butt caps at polyline endpoints. No
round or projecting caps. Visible on thick lines (TSELNE width=6, SLCONS_WHARF
width=4).

### 20. No Simplified/Paper Point Symbol Toggle

`src/s52/lookup.rs:130` is hardcoded to Paper Chart table. Simplified point
symbols (table 0) are parsed from chartsymbols.xml but never selectable.

### 21. Missing Mariner Setting Toggles

`src/s52/settings.rs` lacks: `simplified_points`, `full_sector_lights`,
`light_descriptions`, `show_aton_text`. These are all present in OpenCPN's
`s52utils.h` mariner parameter definitions.

### 22. Pattern Offset Alignment Never Applied

`src/render/patterns.rs:494-504` has `calculate_pattern_offset()` but it is
never called. Patterns may not align seamlessly across adjacent features sharing
the same pattern type.

### 23. LC Circle Approximation

`src/s52/lc.rs:20`: HPGL CI (circle) commands in LC complex line patterns are
approximated as polygon outlines, not filled shapes.

### 24. PRTSUR01 Pattern Excluded

`src/tiles/builder.rs:1376-1377`: The port survey pattern PRTSUR01 is explicitly
skipped with a comment about "large false translucent blocks."

### 25. Missing CS Procedures (Non-Chart Features)

These OpenCPN procedures are absent from navcore2 but are mostly AIS/navigation
overlay features, not chart rendering:
- SYMINS01 (virtual AIS ATONs)
- OWNSHP02 (own ship symbol)
- VESSEL01/02 (AIS vessel targets)
- LEGLIN02 (route leg lines)
- CLRLIN01 (clearing lines)
- PASTRK01 (past track)

SEABED01 is absent as a standalone procedure but its logic is inlined in
`depare02()`. OpenCPN's standalone SEABED01 is also a stub.

---

## Previously Documented Gaps Now FIXED

The earlier audit (`doc/rendering-parity-audit.md`) listed some issues that have
since been resolved:

| Claimed Gap | Current Status |
|-------------|---------------|
| "Lines remain Day-colored in Night mode" | **FIXED**: Lines use palette-indexed `color_index` in `line.wgsl:130`, read from `palette[]` storage buffer |
| "Tile-Bound Decluttering" | **FIXED**: `build_global_text_buffers()` in `state.rs:1894` collects from ALL visible tiles before decluttering |
| "Custom ear-clipping triangulator fails" | **MISLEADING**: OSENC files contain pre-triangulated geometry (`senc/geometry.rs:4`). No runtime tessellation. Fan triangulation is only for clipping convex tile-boundary fragments |
| "CS result caching missing" | **FIXED**: HashMap cache in S52Engine keyed by (object_class, procedure, attr_hash) |
| "No SCAMIN bypass for safety" | **FIXED**: DISPLAYBASE features bypass SCAMIN in builder |
| "Synchronous tile building" | **FIXED**: Async worker with try_recv, deferred upload queue |

---

## Summary: Top 10 Fixes by Impact-to-Effort Ratio

| # | Gap | Effort | Impact | Ref | Status |
|---|-----|--------|--------|-----|--------|
| 1 | Add ~20 missing S-57 attribute codes | Trivial | **Massive** -- fixes all CS procedures | Gap 1 | **DONE** (2026-04-16) |
| 2 | Add line feature TX/TE text emission | Small | High -- depth contour labels appear | Gap 5 | **DONE** (2026-04-16) |
| 3 | Priority-sort label declutter candidates | Small | Medium -- correct text priority | Gap 10 | **DONE** (2026-04-16) |
| 4 | Build Dusk/Night symbol atlases | Small | High -- proper night mode | Gap 6 | |
| 5 | Add DASD line pattern (4-phase shader) | Small | Medium -- correct dash-dot lines | Gap 11 | **DONE** (2026-04-16) |
| 6 | Score-based LUP matching | Medium | High -- correct symbol selection | Gap 4 | |
| 7 | Category-mutable promotion in CS | Medium | High -- safety compliance | Gap 2 | |
| 8 | Complete TOPMAR01 (TOPSHP 14-33 + platform) | Small | Medium -- correct topmarks | Gap 9 | |
| 9 | General symbol rotation from SY()/ORIENT | Small | Medium -- correct orientations | Gap 13 | |
| 10 | Spatial UDWHAZ03 for isolated dangers | Large | High -- correct danger symbols | Gap 3 | |
