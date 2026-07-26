# NavCore2/OpenCPN Rendering Parity Execution Plan

Date: 2026-04-24

This plan tracks the non-text rendering parity work: symbols, areas, colors,
and lines. It reflects the current tree, where several older audit findings
are already fixed: Standard display defaults, M_COVR category handling, LUP
specificity lookup, palette-indexed areas/lines, simplified symbol table
selection, and priority-aware drawing.

## Current Findings

### Symbols

- Current point rendering resolves LUP and CS rules through `S52Engine`.
- The builder renders only the first resolved `SY()` instruction for a point
  feature, which drops multi-symbol portrayals and supplementary marks.
- Symbols are sampled from a precolored RGBA atlas, so palette changes do not
  recolor them the way OpenCPN's Day/Dusk/Night symbol assets do.
- Symbol sizing uses a fixed global scale. This is useful for visibility, but
  it is not a verified match for OpenCPN presentation-library metrics.

### Areas

- Area fill colors are palette-indexed and display priority is carried to the
  shader.
- `PRTSUR01` is intentionally excluded from the generic pattern renderer
  because that path currently creates false translucent blocks.
- Pattern metadata such as origin, pivot, min distance, and max distance is
  parsed but not used by the renderer.
- DRGARE is priority-bumped over DEPARE as a pragmatic draw-order patch.
- Background patterns currently do not use the same stencil split as background
  areas and lines.

### Colors

- Area and line shaders use the palette buffer.
- Symbols, text, and soundings are not yet fully palette-aware.
- Unknown color-token fallback behavior can hide missing S-52 coverage.

### Lines

- Simple `LS()` rendering is palette-indexed and supports screen-space widths,
  dashes, and miter/bevel joins.
- Complex `LC()` rendering uses vector stamps, but per-primitive HPGL stroke
  width is ignored when stamp segments are converted to polylines.
- LC placement is still approximate compared with OpenCPN when clipping,
  simplification, caps, joins, and special arcs are involved.

## Execution Order

1. Use the existing tile parity report and add focused debug output only where
   it reveals emitted draw operations that the report cannot show.
2. Fix point-symbol composition by emitting every resolved `SY()` instruction,
   not only the first.
3. Make symbol coloring palette-aware by generating or loading palette-specific
   atlases, or by preserving tokenized symbol color data if available.
4. Fix area patterns: implement `PRTSUR01` specially, honor pattern origin and
   pivot metadata, and add background-pattern stencil handling.
5. Fix line parity: preserve LC phase against original feature geometry,
   respect HPGL stroke widths, and compare LS/LC metrics against OpenCPN.
6. Move text and soundings onto palette-aware rendering.
7. Replace geometry shortcuts with robust ring/tessellation handling where
   visual parity still differs after the earlier fixes.

## Execution Log

- 2026-04-24: Plan saved. First implementation target is symbol composition.
- 2026-04-24: Point builder now emits all resolved `SY()` instructions per
  feature and logs/counts multi-symbol point portrayals.
- 2026-04-24: Symbol renderer now switches between OpenCPN day, dusk, and
  dark raster symbol atlases when the S-52 palette changes.
- 2026-04-24: Area pattern renderer now uses atlas cell size plus origin/pivot
  metadata instead of repeating only the occupied bitmap rect.
- 2026-04-24: LC vector stamps are grouped by HPGL `SW` primitive width so
  complex line symbols are no longer all forced through width 1.
- 2026-04-24: `PRTSUR01` now bypasses the generic zero-height AP texture path
  and emits sparse horizontal hatch strokes clipped to area rings.
- 2026-04-24: Background pattern draws now use a stencil-tested pattern
  pipeline, matching the foreground/background split used by areas and lines.
- 2026-04-24: Label and sounding instances now carry S-52 palette indices and
  their shaders sample the palette buffer, so Day/Dusk/Night changes retint
  text-like chart content without rebuilding tiles.
- 2026-04-24: Added ring-based `earcutr` fallback tessellation for filled area
  features that have usable rings but emit no pre-triangulated vertices.
