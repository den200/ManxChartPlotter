#!/usr/bin/env python3
"""Attribute a picture-level parity gap to the features that caused it.

    manx --dump-scene <charts> <lat,lon,mpp> <WxH> /tmp/scene.ndjson
    tools/parity_blame.py <reference.png> <manx.png> /tmp/scene.ndjson

`tools/parity.py` says *how much* manx and OpenCPN disagree. This says *who*:
it takes every pixel where manx draws a land/water boundary the reference
does not, looks it up in the scene log, and ranks the classes responsible.

That is the step that makes a screenshot actionable. A number tells you the
picture got worse; this tells you it was CBLARE's boundary, from the 1:22000
cell, drawn as DASH,2,CHMGD — which is a claim you can check against the S-52
oracle and then fix.

The reverse direction (reference-only ink) cannot be blamed on manx records —
by definition manx drew nothing there — so it is reported as locations and
as the classes manx *skipped* nearby, which is usually the answer.
"""

import json
import sys
from collections import Counter, defaultdict

import numpy as np
from PIL import Image

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from parity import (  # noqa: E402
    BLOCK,
    block_agreement,
    chrome_mask,
    coarsen,
    coarsen_fill,
    palette,
    water_land,
)

# The blame runs at the picture metric's block size, not the mask's.
PBLOCK = 8


def load_scene(path):
    areas, lines, skips = [], [], []
    with open(path) as f:
        for ln in f:
            rec = json.loads(ln)
            r = rec.get("record")
            if r == "area":
                areas.append(rec)
            elif r == "line":
                lines.append(rec)
            elif r == "skip":
                skips.append(rec)
    return areas, lines, skips


def build_line_index(lines, w, h, radius=3, block=PBLOCK):
    """Stamp every stroked feature into a pixel -> feature-key grid.

    Per-pixel nearest-segment search over thousands of polylines is too slow to
    run for every disputed pixel; rasterising once is not.
    """
    grid = defaultdict(list)
    for rec in lines:
        key = (rec["class"], rec["source"], rec["style"], rec["chart"])
        for poly in rec["polylines_px"]:
            for i in range(len(poly) - 1):
                (x0, y0), (x1, y1) = poly[i], poly[i + 1]
                n = int(max(abs(x1 - x0), abs(y1 - y0))) + 1
                if n > 20000:
                    continue
                for t in range(n + 1):
                    x = x0 + (x1 - x0) * t / n
                    y = y0 + (y1 - y0) * t / n
                    bx, by = int(x) // block, int(y) // block
                    for dy in range(-1, 2):
                        for dx in range(-1, 2):
                            grid[(bx + dx, by + dy)].append(key)
    return grid


def area_blame(areas, bx, by, block=PBLOCK):
    """The highest-priority area whose extent covers this block."""
    best = None
    px, py = (bx + 0.5) * block, (by + 0.5) * block
    for a in areas:
        x0, y0, x1, y1 = a["extent_px"]
        if not (min(x0, x1) - block <= px <= max(x0, x1) + block):
            continue
        if not (min(y0, y1) - block <= py <= max(y0, y1) + block):
            continue
        if best is None or a["priority"] > best["priority"]:
            best = a
    return best


def main():
    if len(sys.argv) < 4:
        print(__doc__)
        return 2
    ref = np.array(Image.open(sys.argv[1]).convert("RGB"))
    nav = np.array(Image.open(sys.argv[2]).convert("RGB"))
    h = min(ref.shape[0], nav.shape[0])
    w = min(ref.shape[1], nav.shape[1])
    ref, nav = ref[:h, :w], nav[:h, :w]
    areas, lines, skips = load_scene(sys.argv[3])

    pal = palette()
    rw, rk = water_land(ref, pal)
    nw, nk = water_land(nav, pal)
    _, ren = coarsen_fill(rw, rk)
    _, nen = coarsen_fill(nw, nk)
    keep = (coarsen((~chrome_mask(ref)).astype(np.float32)) > 0.5) & ren & nen

    # Blame the blocks the picture metric counts as different — that is the
    # number being driven down, so it is the one worth attributing.
    _, bad, kblocks = block_agreement(ref, nav, keep, pal, block=PBLOCK)

    grid = build_line_index(lines, w, h)

    print(f"{int(bad.sum())} of {int(kblocks.sum())} blocks differ "
          f"({100.0 * bad.sum() / max(1, kblocks.sum()):.1f}%)\n")

    blame = Counter()
    unattributed = 0
    for by, bx in zip(*np.nonzero(bad)):
        keys = grid.get((int(bx), int(by)))
        if keys:
            # One vote per block, to the nearest distinct feature class.
            for k in set(keys):
                blame[k] += 1.0 / len(set(keys))
        else:
            a = area_blame(areas, int(bx), int(by))
            if a:
                blame[(a["class"], "area-fill", f"colour#{a['color_index']}", a["chart"])] += 1
            else:
                unattributed += 1

    print("differing blocks, by the feature nearest them:\n")
    print(f"  {'blocks':>8}  {'class':<9} {'via':<14} {'style':<22} chart")
    for (cls, src, style, chart), n in blame.most_common(18):
        print(f"  {n:8.1f}  {cls:<9} {src:<14} {style:<22} {chart}")
    if unattributed:
        print(f"  {unattributed:8d}  (no area or stroke recorded — symbol, text or pattern)")

    return 0


if __name__ == "__main__":
    sys.exit(main())
