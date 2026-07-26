# s52oracle — OpenCPN as a symbology oracle

Screenshot diffing conflates four independent failure modes: wrong LUP picked,
wrong CS logic, wrong symbol geometry, wrong rasterisation. This harness cuts
the first two out of the picture by comparing the **instruction stream** instead
of pixels.

```
                 ┌──────────────────────────────┐
 chart (.oesu) ──┤ navcore --dump-ir            ├── features.ndjson ──┐
                 │  (parses the SENC once)      ├── navcore.ndjson ─┐ │
                 └──────────────────────────────┘                   │ │
                                                                    │ │
                 ┌──────────────────────────────┐                   │ │
                 │ s52oracle                    │◄──────────────────┼─┘
                 │  links OpenCPN's s52plib     ├── oracle.ndjson ──┤
                 └──────────────────────────────┘                   │
                                                                    ▼
                                                             tools/s52diff.py
                                                        ranked divergence report
```

Both engines resolve **the same feature**: navcore parses the chart, the oracle
never touches one. That removes the biggest confound — you are never comparing
two different feature sets — and it means the oracle needs no o-charts
decryption, no OpenGL and no wx frame.

## Run it

```sh
tools/s52conformance.sh charts/oeuSENC-DK-2025-1-20-base-macbook/ /tmp/s52conf
```

That builds the oracle if needed, dumps navcore's stream, runs OpenCPN's engine
over the same features with **navcore's mariner settings**, and prints the diff
ranked by divergence kind and object class. `-v` as the 4th argument adds
examples. 153k features take about 6 seconds end to end.

To reproduce one finding, re-run against the single chart named in the id
(`OC-45-DESOM5#42` → chart `OC-45-DESOM5`, feature index 42).

## What it is

`src/main.cpp` links four translation units from OpenCPN's standalone
presentation library (`doc/openCPN/libs/s52plib`) — `s52plib.cpp`,
`s52cnsy.cpp`, `chartsymbols.cpp`, `s52utils.cpp` — plus geoprim and pugixml,
and calls exactly two things per feature:

- `s52plib::S52_LUPLookup()` — which lookup row wins,
- the CS procedure via `condTable` — what that row's `CS(...)` expands to.

`S57Obj` is reimplemented in main.cpp (the real one drags in the chart/GDAL/GL
stack); only the attribute store matters here and its semantics are pinned by
how `FindBestLUP` and the CS procedures read it. `stubs.cpp` satisfies the
linker for the rasterisation half — every stub that must never run aborts
loudly rather than returning a plausible lie.

**Dev tool only.** It links GPLv2 code and must never ship with navcore. The
relationship is oracle-only: read behaviour from it, implement from the spec.

## The three layers

The harness answers three separate questions about every feature, and a bug can
live in any one of them while the other two are perfect:

| layer | question | how it is checked |
|---|---|---|
| symbology | which lookup row, which instructions? | diff against `S52_LUPLookup` + the CS jump table |
| visibility | would it be drawn at all, at this scale? | diff against `ObjectRenderCheckCat` |
| geometry | where did the ink actually go? | `navcore --dump-scene` + `tools/scenecheck.py` |

The visibility layer needs a view scale, so it is off unless you give one:

```sh
NAVCORE_VIEW_SCALE=11600 tools/s52conformance.sh charts/<dir>/ /tmp/run
```

It covers display category, the meta-object filter, SCAMIN and SUPER_SCAMIN —
the decisions that make a correctly-resolved feature never reach the screen.
Configure the oracle to match navcore, not to OpenCPN's compiled defaults:
`--super-scamin` and `--show-meta` are off because OpenCPN's *config* defaults
are off (`navutil.cpp` reads both with a default of 0), and `--zoom-modifier`
is pinned to 0 because SCAMIN is scaled by `pow(8, mod/5)`.

Both engines must run the **same mariner settings** or the report measures
configuration rather than code. The one that matters most is the ENC display
category: the reference captures are taken with "All", so navcore defaults to
it and `s52conformance.sh` passes `--display-cat all` to the oracle. Override
both at once with `NAVCORE_DISPLAY_CAT=standard`.

`tools/parity.py <reference.png> <capture.png>` closes the loop at the picture
level — coastline recall and precision with OpenCPN's UI chrome masked out. Use
it as a guard rail after any change the other layers call clean; the three
layers can all be zero while the picture is wrong for a reason none of them
model (draw order, line width, pattern phase).

## Reading the report

- `lup_selection` / `lup_priority` / `lup_category` — the two engines picked
  different lookup rows. Always a navcore bug.
- `expand_*` — same row, different expansion. A CS procedure differs.
- `expand_* [ctx]` — **suspect, not proof.** The procedure consults chart
  context the oracle was not given for that feature. navcore now ships the
  surrounding depth areas with every danger object (`"assoc"` in the feature
  record, `DepthAreaIndex` on the navcore side, a local `s57chart` definition
  hooked to `chart_context::pt2GetAssociatedObjects` on the oracle side), so
  UDWHAZ03 is decidable and this tag only appears where a feature genuinely
  sits in no depth area. TOPMAR01's floating/rigid ATON lists are still empty.
- `SETTINGS_MISMATCH_table` — the two sides disagree on which of the five LUP
  tables to read, i.e. they were configured differently. Fix that before
  reading anything else in the report.
- "fields OpenCPN emits that navcore does not model" — navcore's
  `RenderInstruction` has no slot for them (symbol rotation, TX/TE spacing).

## Known deviations

`tools/s52diff.py` folds these out of the ranking and counts them separately,
so they stay visible without burying real bugs.

- **Light description and sector arc** — navcore builds both in
  `tiles/builder.rs` (`litdsn01`, `light_sector_info`) rather than returning
  `TX(...)`/`CA(...)` from the CS procedure, so its IR carries the bare symbol.
- **Soundings drawn as text** — OpenCPN composes a depth from per-digit symbols
  (`SY(SOUNDG21);SY(SOUNDG12)`); navcore draws it as one text instruction,
  consistently, everywhere soundings appear.
- **DEPARE01 depth shade where DRVAL2 is absent** — *this one is an OpenCPN
  defect, not a navcore choice.* `s52cnsy.cpp:621-631` declares `double drval1,
  drval2;` and only assigns `drval2` from the attribute, with the intended
  `drval2 = drval1 + 0.01` commented out, so a feature with no DRVAL2 gets its
  depth shade from an uninitialised stack slot. In this corpus all 303
  disagreeing AC() colours are features with no DRVAL2 and none with one.
  navcore uses the documented fallback.
- **Directional lights** — LIGHTS06 declares `orientstr` and leaves the
  assignment commented out, so OpenCPN renders *every* directional light as
  `SY(QUESMRK1)` whatever its ORIENT. navcore draws the oriented flare the
  procedure describes.
- **Multipoint soundings** — OpenCPN emits `MP()` and expands per point in the
  renderer; navcore has a dedicated multipoint path.

## Where the numbers stand

Full Danish set, 391 charts, 153,276 features: **zero unexplained divergences**,
down from 22 % when the harness was first run end to end. Every remaining
difference is one of the deviations above, each counted separately — so any new
divergence appearing in the ranked section is a regression.

## Version note

The vendored `doc/openCPN/libs` is OpenCPN 5.12.4; the reference screenshots
came from 5.14.0. s52plib changed little between them, but a divergence that
looks unexplainable is worth checking against 5.14 sources before assuming
navcore is wrong. `doc/openCPN/libs/s52plib/src/s52plib.cpp` also carries a
4-line portability patch (`TextObjList::Node` → `compatibility_iterator`) so it
builds against wxWidgets 3.3.
