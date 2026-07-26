#!/usr/bin/env python3
"""Compare a navcore capture with an OpenCPN reference screenshot.

    tools/parity.py <reference.png> <navcore.png> [--overlay out.png]

Reports how much of the reference's land/water boundary navcore reproduces, and
how much navcore draws that the reference does not. Coastline is the right
signal for this: it is the one feature class both renderers must agree on
exactly, it is unaffected by fonts and symbols, and it is visible at every zoom.

This is a guard rail, not a diagnosis. When it moves, ask the scene and the
S-52 oracle *why*; a number cannot tell you which feature changed.

Chrome is excluded, because it is not chart content and pretending otherwise
corrupts the comparison. The chart-outline rectangles OpenCPN draws in green
break the land mask wherever they cross land, producing "reference coastline"
along a straight line no chart contains — which quietly rewards any navcore
artefact that happens to run down the same edge.
"""

import sys
from collections import Counter

import numpy as np
from PIL import Image


# Block size the fill masks are reduced by before edges are taken.
BLOCK = 4


def water_land(a, pal):
    """Split the image into water fill, land fill and everything else.

    Two earlier masks both failed, in opposite directions. Keying on tan made
    every brown building and road read as coastline, so an urban view measured
    warehouses rather than the shore. Keying on blue made every label and buoy
    over water read as coastline instead. The fix is to recognise only *fills*:
    a pixel counts when its colour is an S-52 palette entry, and ink — text,
    symbols, line work, antialiasing — is simply not counted.

    Returns (water, known) boolean arrays.
    """
    idx, _ = classify(a, pal)
    known = idx >= 0
    r, g, b = a[:, :, 0].astype(int), a[:, :, 1].astype(int), a[:, :, 2].astype(int)
    # Every S-52 depth shade is blue or blue-grey; no land colour is.
    water = known & (b >= r + 6) & (b > 140) & (g > 120)
    return water, known


def coarsen_fill(water, known, block=BLOCK):
    """Majority vote per block, counting only fill pixels.

    A block with too little fill left — under a dense label, say — is dropped
    from the comparison rather than guessed at.
    """
    h = water.shape[0] // block * block
    w = water.shape[1] // block * block
    shape = (h // block, block, w // block, block)
    wsum = water[:h, :w].reshape(shape).sum(axis=(1, 3))
    ksum = known[:h, :w].reshape(shape).sum(axis=(1, 3))
    enough = ksum >= block * block // 2
    return (enough & (wsum * 2 > ksum)).astype(np.float32), enough


def chrome_mask(a):
    """Pixels of the reference that are UI, not chart.

    The green chart-outline rectangles, the toolbar strips down the left and
    along the bottom, and the zoom/scale widget bottom right.
    """
    r, g, b = a[:, :, 0].astype(int), a[:, :, 1].astype(int), a[:, :, 2].astype(int)
    h, w = r.shape
    # OpenCPN's chart outline is saturated green; nothing on a chart is.
    m = ((g > 150) & (r < g - 60) & (b < g - 60))
    # Toolbars are near-black or near-white panels, not chart colours.
    dark = (r < 70) & (g < 70) & (b < 70)
    m |= dark
    m[:, :90] = True                     # left icon strip
    m[int(h * 0.93):, :] = True          # bottom status bar and scale bar
    m[int(h * 0.88):, int(w * 0.60):] = True  # zoom + scale widget
    return dilate(m.astype(np.float32), 3) > 0


def dilate(m, n=1):
    d = m.copy()
    for _ in range(n):
        e = d.copy()
        d[1:] = np.maximum(d[1:], e[:-1])
        d[:-1] = np.maximum(d[:-1], e[1:])
        d[:, 1:] = np.maximum(d[:, 1:], e[:, :-1])
        d[:, :-1] = np.maximum(d[:, :-1], e[:, 1:])
    return d


def edges(m):
    gx = np.zeros_like(m)
    gy = np.zeros_like(m)
    gx[:, 1:] = np.abs(m[:, 1:] - m[:, :-1])
    gy[1:, :] = np.abs(m[1:, :] - m[:-1, :])
    return np.clip(gx + gy, 0, 1)


def coarsen(m, block=BLOCK):
    """Reduce the mask to blocks decided by majority.

    Text, dashed leading lines and building outlines all sit on top of land and
    break the land mask into hairlines, so a per-pixel edge count is dominated by
    where the two renderers put their *labels*, not their coastlines. A majority
    vote per block erases anything thinner than the block and leaves the
    land/water partition, which is what this is measuring.
    """
    h = m.shape[0] // block * block
    w = m.shape[1] // block * block
    blocks = m[:h, :w].reshape(h // block, block, w // block, block)
    return (blocks.mean(axis=(1, 3)) > 0.5).astype(np.float32)


def palette(path="assets/s52/chartsymbols.xml", table="DAY_BRIGHT"):
    """The S-52 colour table, so pixels can be named instead of compared."""
    import re

    src = open(path).read()
    m = re.search(rf'<color-table name="{table}">(.*?)</color-table>', src, re.S)
    if not m:
        return {}
    return {
        c.group(1): (int(c.group(2)), int(c.group(3)), int(c.group(4)))
        for c in re.finditer(
            r'<color name="(\w+)"\s+r="(\d+)"\s+g="(\d+)"\s+b="(\d+)"', m.group(1)
        )
    }


def classify(img, pal, tol=14):
    """Label each pixel with the nearest palette token, or -1.

    The tolerance absorbs OpenCPN's brightness offset (its colours sit a few
    counts off the table) without letting antialiased edges, text or symbols
    masquerade as a fill.
    """
    names = list(pal)
    table = np.array([pal[n] for n in names], dtype=np.int16)
    flat = img.reshape(-1, 1, 3).astype(np.int16)
    d = np.abs(flat - table.reshape(1, -1, 3)).sum(axis=2)
    idx = d.argmin(axis=1)
    best = d[np.arange(d.shape[0]), idx]
    idx = np.where(best <= tol * 3, idx, -1)
    return idx.reshape(img.shape[:2]), names


def color_report(ref, nav, keep):
    """Where the two renderers disagree about which S-52 colour a pixel is."""
    pal = palette()
    if not pal:
        print("no colour table found")
        return
    ri, names = classify(ref, pal)
    ni, _ = classify(nav, pal)
    named = keep & (ri >= 0) & (ni >= 0)
    total = int(named.sum())
    if not total:
        print("no classifiable fill pixels")
        return
    agree = int((ri[named] == ni[named]).sum())
    print(f"\nfill colour agreement {agree / total:.4f} over {total} classified pixels")

    conf = Counter()
    dis = named & (ri != ni)
    for a, b in zip(ri[dis], ni[dis]):
        conf[(names[a], names[b])] += 1
    if conf:
        print("\n  top disagreements (reference -> navcore, % of classified):\n")
        for (a, b), n in conf.most_common(10):
            print(f"    {100.0 * n / total:6.2f}%  {a:<7} -> {b}")


def block_agreement(ref, nav, keep, pal, block=8):
    """Fraction of blocks painted the same S-52 colour by both renderers.

    Comparing raw RGB does not work here: the reference screenshots are macOS
    captures tagged Display P3 while navcore writes raw sRGB, so identical
    renders differ by up to 23 counts on a channel — DEPVS reads (115,182,239)
    in navcore and (92,183,243) in the reference from the same table. Fitting
    that transform away is worse than useless (a linear fit in gamma space
    skews whichever colour is not the majority). Naming each pixel's palette
    token absorbs the profile shift exactly, and it is also what the question
    actually is: not "are these bytes equal" but "is this water the same shade
    of water".

    Ink — text, symbols, line work, antialiasing — is unclassified and does not
    vote; a block with too little fill left to judge is skipped.

    Returns (fraction, disagreement mask, compared mask).
    """
    idx_r, names = classify(ref, pal)
    idx_n, _ = classify(nav, pal)
    h = ref.shape[0] // block * block
    w = ref.shape[1] // block * block

    def majority(idx):
        cells = idx[:h, :w].reshape(h // block, block, w // block, block)
        cells = cells.transpose(0, 2, 1, 3).reshape(h // block, w // block, block * block)
        out = np.full(cells.shape[:2], -1, dtype=np.int32)
        count = np.zeros(cells.shape[:2], dtype=np.int32)
        for t in range(len(names)):
            c = (cells == t).sum(axis=2)
            better = c > count
            out[better] = t
            count[better] = c[better]
        return out, count

    mr, cr = majority(idx_r)
    mn, cn = majority(idx_n)

    k = np.kron(keep.astype(np.float32), np.ones((BLOCK, BLOCK)))[:h, :w]
    k = k.reshape(h // block, block, w // block, block).mean(axis=(1, 3)) > 0.5
    enough = (cr >= block * block // 2) & (cn >= block * block // 2)
    compared = k & enough
    bad = compared & (mr != mn)
    n = int(compared.sum())
    return (1.0 - bad.sum() / max(1, n)), bad, compared


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    ref_path, nav_path = sys.argv[1], sys.argv[2]

    ref = np.array(Image.open(ref_path).convert("RGB"))
    nav = np.array(Image.open(nav_path).convert("RGB"))
    h = min(ref.shape[0], nav.shape[0])
    w = min(ref.shape[1], nav.shape[1])
    ref, nav = ref[:h, :w], nav[:h, :w]

    pal = palette()
    rw, rk = water_land(ref, pal)
    nw, nk = water_land(nav, pal)
    rm, ren = coarsen_fill(rw, rk)
    nm, nen = coarsen_fill(nw, nk)
    # Compare only where both renderers left enough fill to judge, and where the
    # reference is chart rather than chrome.
    keep = (coarsen((~chrome_mask(ref)).astype(np.float32)) > 0.5) & ren & nen
    er = edges(rm) * keep
    en = edges(nm) * keep

    # A one-block tolerance: both renderers antialias and round differently, and
    # a coastline off by a few pixels is agreement, not disagreement.
    er_d, en_d = dilate(er), dilate(en)
    recall = float((er * en_d).sum()) / max(1.0, float(er.sum()))
    precision = float((en * er_d).sum()) / max(1.0, float(en.sum()))
    f1 = 2 * recall * precision / max(1e-9, recall + precision)

    agree, bad, compared = block_agreement(ref, nav, keep, pal)
    print(
        f"picture    {agree:.4f} of 8px blocks paint the same S-52 colour"
        f"   ({int(compared.sum())} blocks compared, {int(bad.sum())} differ)"
    )

    print(
        f"coastline  recall {recall:.4f}  precision {precision:.4f}  F1 {f1:.4f}"
        f"   ({int(er.sum())} reference edge px, {int(en.sum())} navcore, "
        f"{100.0 * (~keep).mean():.0f}% chrome-masked)"
    )

    if "--colors" in sys.argv or "--colours" in sys.argv:
        color_report(ref, nav, ~chrome_mask(ref))

    if "--overlay" in sys.argv:
        out = sys.argv[sys.argv.index("--overlay") + 1]
        up = lambda m: np.kron(m, np.ones((BLOCK, BLOCK)))  # noqa: E731
        h2, w2 = er.shape[0] * BLOCK, er.shape[1] * BLOCK
        base = (nav[:h2, :w2].astype(float) * 0.35 + 180 * 0.65).astype(np.uint8)
        base[up(dilate(er)) > 0] = [220, 0, 0]        # reference only
        base[up(dilate(en)) > 0] = [0, 90, 220]       # navcore only
        base[(up(dilate(er)) > 0) & (up(dilate(en)) > 0)] = [0, 0, 0]
        base[up((~keep).astype(np.float32)) > 0] = [225, 225, 225]
        Image.fromarray(base).save(out)
        print(f"wrote {out}  (red = reference only, blue = navcore only, black = both)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
