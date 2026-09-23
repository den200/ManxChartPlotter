# s57oracle — OpenCPN's S-57 reader as an oracle

NavCore is getting its own S-57 (ENC) reader. This tool dumps what **OpenCPN's**
reader makes of the same cell, one feature per line, so the two can be diffed
feature by feature: same classes, same attributes, same geometry, same result
after the update files are applied.

```
cell.000 (+ .001 .002 …) ──► s57oracle ──► oracle.ndjson ─┐
                                                          ├─► diff by foid
cell.000 (+ .001 .002 …) ──► navcore S-57 reader ─────────┘
```

**Dev tool only.** It links GPL code from OpenCPN (the ISO 8211 module, the
S-57 reader, and the GDAL/OGR subset OpenCPN vendors) and must never ship with
navcore or be linked into it. Read behaviour from it; implement from the spec.

## Build

Needs the OpenCPN source tree at `doc/reference projects/OpenCPN` (kept locally
and not tracked, like `doc/openCPN` for s52oracle), and clang++. No wxWidgets,
no GL: nothing in this slice of OpenCPN needs them, so no stubs are needed
either. The one wx-based file, `s57registrar_mgr.cpp`, is left out. It maps
acronyms to SENC ids, which the oracle doesn't need.

```sh
tools/s57oracle/build.sh                 # plain clang++, no CMake needed
# or
cmake -S tools/s57oracle -B tools/s57oracle/build && cmake --build tools/s57oracle/build
```

Either way the binary is `tools/s57oracle/build/s57oracle`. The path to
OpenCPN's `data/s57data` (`s57objectclasses.csv`, `s57attributes.csv`) is
compiled in. Override it with `--s57data DIR`, and override the source tree with
`OCPN=… build.sh` or `-DOCPN_ROOT=…`.

## Run

```sh
s57oracle ENC_ROOT/US4MA1DC/US4MA1DC.000 > oracle.ndjson
s57oracle --no-updates ENC_ROOT/US4MA1DC/US4MA1DC.000 > base.ndjson   # base cell only
```

The 233-cell NOAA Massachusetts set (137,528 features, 365 update files) runs
in about 12 s in total.

Line 1:

```json
{"dsid":{"dsnm":"US4MA1BD.000","edtn":"1","updn":3,"base_updn":0,"isdt":"20240411",
 "last_update_isdt":"20250707","comf":10000000,"somf":10,"cscl":45000,"nall":1,"aall":1,
 "updates_applied":3,"update_files":["US4MA1BD.001","US4MA1BD.002","US4MA1BD.003"],"features":22}}
```

`updn` is the last update number OpenCPN would record (`m_last_applied_update`).
`updates_applied` counts the real files applied; missing numbers don't count
(see below).

Then one line per feature:

```json
{"rcid":5,"objl":42,"acronym":"DEPARE","prim":3,"grup":1,"rver":1,"foid":"550-833497592-5553",
 "attrs":{"DRVAL1":"36.5","DRVAL2":"54.8"},
 "geom":{"type":"Polygon","rings":[[[-70.6021345,40.8],[-70.602145,40.7998329],…]]}}
{"rcid":419,"objl":129,"acronym":"SOUNDG","prim":1,…,"geom":{"type":"MultiPoint","c":[[-70.777642,40.6278143,62.1],…]}}
{"rcid":1835,"objl":400,"acronym":"C_AGGR","prim":255,…,"attrs":{…},"geom":null}
```

- `attrs` holds the ATVL strings **as stored in the (updated) record**, in
  ATTF-then-NATF order, including list values like `"COLOUR":"1,3"`, NATF
  national attributes, and empty strings (`"CATLIT":""`). The set of keys is
  exactly what OpenCPN's reader keeps. The tool checks this against the OGR
  feature on every line and warns on stderr if they differ.
- Coordinates are printed with log10(COMF) decimals (7 for NOAA) and depths
  with log10(SOMF) decimals, trailing zeros trimmed. Both are exact, because the
  stored values are integers divided by COMF or SOMF.
- `foid` is `AGEN-FIDN-FIDS` with FIDN as the unsigned 32-bit value from the
  spec. See the caveats.
- Diagnostics and a per-cell summary go to stderr.

## How OpenCPN opens a cell, and so how the oracle does

This follows the SENC build path: `Osenc::createSenc200` → `GetBaseFileAttr`
→ `ingestCell` → `ValidateAndCountUpdates` → `s57chart::GetUpdateFileArray`
(`gui/src/Osenc.cpp`, `gui/src/s57chart.cpp`).

**Reader options.** `ingestCell` sets `RETURN_LINKAGES=ON` and
`RETURN_PRIMITIVES=ON` on the `OGRS57DataSource`. `OGRS57DataSource::Open`
always adds `LNAM_REFS=ON`. After the updates are applied, `ingestCell` calls
`SetOptions` again with `RETURN_LINKAGES=ON`, `RETURN_PRIMITIVES=OFF`, so
features rather than vector primitives come back. That call also drops
`LNAM_REFS`, because it was never in that list. Everything else is off:

| option | value | effect |
|---|---|---|
| `UPDATES` | off | the reader's own `FindAndApplyUpdates` is not used; OpenCPN applies updates itself (below) |
| `SPLIT_MULTIPOINT` | off | a SOUNDG stays one MultiPoint feature |
| `ADD_SOUNDG_DEPTH` | off | depth is only the Z of each point |
| `PRESERVE_EMPTY_NUMBERS` | off | an empty E/I/F attribute value is dropped, not kept as a marker |
| `RETURN_PRIMITIVES` | on for ingest, then off | only affects which layers exist; features are read with it off |
| `RETURN_LINKAGES` | on | FSPT linkage fields on the OGR feature (not emitted) |
| `LNAM_REFS` | on at open, off when reading | no effect on the output |

The registrar (the class and attribute catalogue) is `S57ClassRegistrar`,
loaded from OpenCPN's `data/s57data`, as `LoadInfo(g_csv_locn)` does.

**Which update files are used** (`GetUpdateFileArray`):

- A file qualifies if it sits in the same directory as the `.000`, has the same
  base name, and has an extension that parses as an integer (`CATALOG.031`
  excluded). Its DSID `EDTN` must equal the base's, and its `ISDT` must be on or
  after the base's `ISDT`. If the parent directory's name is numeric, OpenCPN
  searches recursively from there instead.
- They are applied in extension order, 1…N, where N is the highest qualifying
  number. A missing number, or a file of 25 bytes or fewer, becomes an empty
  dummy module, so it is a no-op that still advances `updn`.
- Each file is applied with `S57Reader::ApplyUpdates`. The first one that
  returns an error **stops** the chain. That update's records up to and past the
  failing one have already been applied.

**Which features come out.** `S57Reader::ReadNextFeature` order is the
`oFE_Index` order, which is **ascending FRID RCID**, not file order, because
`DDFRecordIndex` qsorts by key. A feature whose OBJL is not in the catalogue
is dropped (reported on stderr).

OpenCPN's SENC writer then also skips every feature without geometry. That
means C_AGGR and C_ASSO, and any feature whose geometry an update broke. The
oracle still emits those with `"geom":null`, so the Rust reader can be checked
on them too. Filter `geom != null` to get OpenCPN's visible set.

## Reader behaviours the Rust reader must mimic (or deliberately not)

Attributes (`ApplyObjectClassAttributes`):

1. **Attributes outside the class schema are dropped.** Only attributes listed
   for the class in `s57objectclasses.csv` survive. The same goes for NATF
   attributes.
2. **Deleted attributes.** An update sets ATVL to `0x7F`. For ATTF the field
   is unset. **NATF has no such check**, so a deleted national attribute comes
   through as the string `"\u007f"`. That is an OpenCPN defect.
3. **Empty numbers.** Empty E/I/F values are dropped. Empty A/S/L values are
   kept as `""` by the reader (the SENC writer later drops them).
4. **Duplicate ATTL.** The last occurrence wins, at the first one's position.
5. **Typing.** OpenCPN stores E and I values as `atoi()`, F as `atof()`, and A,
   S and L as strings. `"03"` becomes 3, and `"3.60"` becomes 3.6. The oracle
   prints the raw string. Compare numerically where the class type is E, I or
   F.
6. **Text encoding.** ATTF text is Latin-1 when AALL=1. NATF is Latin-1 when
   NALL=1 and UCS-2LE when NALL=2, with the 0x1F terminator stripped. The oracle
   converts to UTF-8 on the same rules, and also treats lexical level 0 as
   Latin-1. The NOAA MA set is AALL=NALL=1.
7. **Bounds check.** NATF ATTL uses `>= maxAttr` where ATTF uses `> maxAttr`,
   so the highest attribute code is rejected in NATF only.
8. **FIDN** is unsigned 32-bit in the spec, but OGR holds it as a signed int. In
   OpenCPN any FIDN above 2^31 is negative; this happens in the MA set, e.g.
   C_AGGR `550-3541081736-41680`.

Geometry:

9. **Points.** A point is (X, Y) from the first FSPT pointer's VI or VC node,
   using SG2D, or SG3D where Z = VE3D/SOMF. OpenCPN reads position quality from
   the node's first ATTV and adds it as QUAPOS in the SENC when it is not 10.
   That is not part of the reader's attributes and isn't emitted.
10. **Soundings** are one MultiPoint. SG3D is read as YCOO, XCOO, VE3D.
    Depth = VE3D / SOMF, not rounded.
11. **Lines** are always one `LineString`, never Multi. Only the **first FSPT
    field** is used. The start node is added for the first edge only, then each
    edge's SG2D vertices (reversed when ORNT=2), then its end node. Edges are
    concatenated **without checking continuity**. MASK and USAG are ignored.
    Fewer than 2 points means no geometry. A one-vertex edge is read through
    repeated SG2D fields instead of the repeat count; areas do the same.
12. **Areas.** Each edge (start node, SG2D, end node, reversed for ORNT=2)
    goes into `OGRBuildPolygonFromEdges(bestEffort, autoClose=FALSE, tol=0)`.
    Rings are chained greedily by **exact** endpoint equality, taking each
    unused edge in either direction, and a new ring starts at the first
    unconsumed edge. Rings are **not classified or reoriented**. The "exterior"
    is simply the ring built from the first FSPT edge, which in practice is the
    USAG=1 boundary because producers encode it first. Rings with fewer than 3
    points are dropped. MASK and USAG are ignored. Areas read ORNT from
    `FSPT[0]` with the edge index even when there are several FSPT fields.
    Across all 71,146 rings in the MA set, every ring closes.

Updates (`ApplyUpdates` / `ApplyRecordUpdate`):

13. RUIN=1 inserts without checking RVER. Delete and modify require target
    RVER = update RVER − 1. A modify must also keep the same PRIM.
14. FSPC, VRPC and SGCC insert/delete/modify are applied. **FFPC
    (feature-to-feature pointer updates) is ignored.** SG3D coordinates are
    updated through the same path as SG2D.
15. Attribute updates replace the last ATTF/NATF entry with the same ATTL, or
    append one. The value is kept as `0x7F` for a deletion; see 2.
16. An SGCC delete on a record with no coordinates is silently accepted. That
    is a workaround for Hong Kong data.
17. Any record in an update file other than DSID, VRID or FRID makes
    `ApplyUpdates` return an error. That stops the update chain after the whole
    file has been applied.

## Verified on (NOAA MA, `ENC_ROOT`)

| cell | scale | features base → updated | update files | classes (top) | notes |
|---|---|---|---|---|---|
| US4MA1BD | 1:45k | 24 → 22 | 3 | SBDARE 4, DEPARE 3, DEPCNT 2 | smallest cell; 1 added, 3 deleted |
| US4MA1CD | 1:45k | 87 → 105 | 19 | LIGHTS 20, LNDMRK 19, LNDARE 19 | 52 added, 34 removed, 23 changed (e.g. LIGHTS SIGPER 1→6) |
| US4MA1DC | 1:45k | 780 → 816 | 26 | UWTROC 230, DEPCNT 115, DEPARE 101 | 75 added, 39 removed, 70 changed; 70 holes |
| US4MA1EC | 1:45k | 3014 → 3029 | 15 | UWTROC 797, DEPCNT 390, DEPARE 372 | 554 holes; SOUNDG 0.1–44.8 m |

All 233 cells: no reader warnings, no dropped features, 41 C_AGGR/C_ASSO with
no geometry, and every line is valid JSON. Building with CMake and with
build.sh gives byte-identical output.
