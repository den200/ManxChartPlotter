#!/usr/bin/env python3
"""Check manx's emitted scene against invariants, and trace pixels to features.

    manx --dump-scene <charts> <lat,lon,mpp> <WxH> /tmp/scene.ndjson
    tools/scenecheck.py /tmp/scene.ndjson              # invariant report
    tools/scenecheck.py /tmp/scene.ndjson --at 1180,420   # who drew this pixel
    tools/scenecheck.py /tmp/scene.ndjson --missing     # what the view is missing

Where the S-52 oracle answers "which instruction did this feature get", this
answers "which polygon, from which chart, by which code path, put ink here".

There is no OpenCPN counterpart and none is needed: area geometry arrives from
the SENC already tessellated, so manx is reproducing given triangles rather
than computing them. A triangle that lies outside its own feature's declared
extent is wrong on its face — no reference render required to say so.
"""

import json
import sys
from collections import Counter, defaultdict


def load(path):
    viewport, areas, coverage, skips, lines = None, [], [], [], []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            rec = json.loads(line)
            if rec.get("record") == "viewport":
                viewport = rec
            elif rec.get("record") == "area":
                areas.append(rec)
            elif rec.get("record") == "coverage":
                coverage.append(rec)
            elif rec.get("record") == "skip":
                skips.append(rec)
            elif rec.get("record") == "line":
                lines.append(rec)
    return viewport, areas, coverage, skips, lines


def tri_area(t):
    (x0, y0), (x1, y1), (x2, y2) = t
    return abs((x1 - x0) * (y2 - y0) - (x2 - x0) * (y1 - y0)) / 2.0


def point_in_tri(p, t):
    (x0, y0), (x1, y1), (x2, y2) = t
    # A zero-area triangle has all three edge signs zero, which the test below
    # reads as "inside" — so a clipping sliver would claim every pixel in the
    # scene. Reject them first.
    if tri_area(t) < 1e-6:
        return False
    px, py = p
    d1 = (px - x2) * (y0 - y2) - (x0 - x2) * (py - y2)
    d2 = (px - x0) * (y1 - y0) - (x1 - x0) * (py - y0)
    d3 = (px - x1) * (y2 - y1) - (x2 - x1) * (py - y1)
    neg = d1 < 0 or d2 < 0 or d3 < 0
    pos = d1 > 0 or d2 > 0 or d3 > 0
    return not (neg and pos)


def check(viewport, areas):
    """Invariants every emitted area triangle must satisfy."""
    findings = defaultdict(list)
    by_source = Counter()
    total_tris = 0

    for a in areas:
        by_source[a["source"]] += 1
        # extent_px is [left, top, right, bottom] with y growing downward
        ex0, ey0, ex1, ey1 = a["extent_px"]
        left, right = min(ex0, ex1), max(ex0, ex1)
        top, bottom = min(ey0, ey1), max(ey0, ey1)
        # One pixel of slack for the f32 round-trip through the vertex buffer.
        slack = 1.0
        w = max(right - left, 1.0)
        h = max(bottom - top, 1.0)

        for i, t in enumerate(a["tris_px"]):
            total_tris += 1
            ident = f"{a['chart']}#{a['feature']} {a['class']} ({a['source']}) tri {i}"

            if any(not all(map(lambda v: v == v, p)) for p in t):  # NaN
                findings["non-finite vertex"].append(ident)
                continue

            if tri_area(t) < 1e-9:
                findings["degenerate triangle"].append(ident)

            # A triangle may be clipped to a tile, so it can be *smaller* than
            # the feature — but never outside it.
            for (px, py) in t:
                if px < left - slack or px > right + slack or py < top - slack or py > bottom + slack:
                    over = max(left - px, px - right, top - py, py - bottom)
                    findings["vertex outside the feature's own extent"].append(
                        f"{ident}: {over:.0f}px beyond a {w:.0f}x{h:.0f}px extent"
                    )
                    break

    return findings, by_source, total_tris


def dist_to_segment(p, a, b):
    px, py = p
    ax, ay = a
    bx, by = b
    dx, dy = bx - ax, by - ay
    d2 = dx * dx + dy * dy
    if d2 < 1e-12:
        return ((px - ax) ** 2 + (py - ay) ** 2) ** 0.5
    t = max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / d2))
    return ((px - ax - t * dx) ** 2 + (py - ay - t * dy) ** 2) ** 0.5


def strokes_near(lines, px, py, radius):
    """Stroked features passing within `radius` pixels, nearest first.

    Most ink on a chart that is not a fill is an area's LS() boundary rather
    than a line feature, and a fill-less area leaves no trace in the area log at
    all — so "nothing covers this pixel" is a common and useless answer without
    this.
    """
    hits = {}
    for rec in lines:
        best = None
        for poly in rec["polylines_px"]:
            for i in range(len(poly) - 1):
                d = dist_to_segment((px, py), poly[i], poly[i + 1])
                if best is None or d < best:
                    best = d
        if best is not None and best <= radius:
            key = (rec["chart"], rec["feature"], rec["class"], rec["source"], rec["style"],
                   rec["priority"], rec["is_background"])
            hits[key] = min(best, hits.get(key, 1e9))
    return sorted(hits.items(), key=lambda kv: kv[1])


def query(viewport, areas, skips, lines, px, py):
    """Everything that covers a pixel, in draw order (last wins).

    Then everything that *would* cover it and was dropped. A pixel showing the
    wrong colour is usually not a bug in what was drawn — it is a bug in what
    was not, and the second list is the one that names it.
    """
    hits = []
    for a in areas:
        for i, t in enumerate(a["tris_px"]):
            if point_in_tri((px, py), t):
                hits.append((a, i))
                break
    hits.sort(key=lambda h: h[0]["priority"])
    print(f"pixel ({px:.0f}, {py:.0f}) — {len(hits)} area primitive(s), lowest priority first:\n")
    for a, i in hits:
        print(
            f"  prio {a['priority']}  colour#{a['color_index']:<4} {a['class']:<8}"
            f" {a['chart']} 1:{a['chart_scale']:<6} #{a['feature']:<6}"
            f" {'background' if a['is_background'] else 'FOREGROUND'}"
            f"  via {a['source']}  tile z{a['tile'][0]}/{a['tile'][1]}/{a['tile'][2]}"
        )
    if not hits:
        print("  nothing — the pixel is background, or drawn by a stroke/symbol/text")

    near = strokes_near(lines, px, py, 6.0)
    print(f"\n{len(near)} stroked feature(s) within 6px:\n")
    for (chart, feat, cls, source, style, prio, bg), d in near[:12]:
        print(
            f"  {d:4.1f}px  prio {prio}  {cls:<8} {chart} #{feat:<6}"
            f" {style:<22} {source}{'  background' if bg else ''}"
        )
    if not near:
        print("  none")

    # The extent is a bounding box, not the polygon, so this over-reports: a
    # feature whose box contains the pixel need not cover it. It still narrows
    # thousands of skips to a handful worth reading.
    near = {}
    for s in skips:
        ext = s.get("extent_px")
        if not ext:
            continue
        x0, y0, x1, y1 = min(ext[0], ext[2]), min(ext[1], ext[3]), max(ext[0], ext[2]), max(ext[1], ext[3])
        if x0 <= px <= x1 and y0 <= py <= y1:
            key = (s["chart"], s["feature"], s["class"], s["reason"])
            near[key] = (x1 - x0, y1 - y0)

    dropped = [(k, v) for k, v in near.items() if k[3] != "outside-tile"]
    print(f"\n{len(dropped)} feature(s) whose extent contains this pixel were NOT drawn:\n")
    # Smallest extent first: the tightest box around the pixel is the most
    # likely to be the feature actually under it.
    for (chart, feat, cls, reason), (w, h) in sorted(dropped, key=lambda kv: kv[1][0] * kv[1][1]):
        print(f"  {cls:<8} {chart} #{feat:<6} {reason:<28} extent {w:.0f}x{h:.0f}px")
    if not dropped:
        print("  none — everything covering this pixel was drawn")


def point_in_poly(p, poly):
    x, y = p
    inside = False
    j = len(poly) - 1
    for i in range(len(poly)):
        xi, yi = poly[i]
        xj, yj = poly[j]
        if (yi > y) != (yj > y) and x < (xj - xi) * (y - yi) / (yj - yi) + xi:
            inside = not inside
        j = i
    return inside


def coverage_report(viewport, areas, coverage):
    """How much of what each chart draws lies outside its own M_COVR coverage.

    A cell is selected for a tile by the rectangular extent in its SENC header,
    but the ground it actually describes is its M_COVR polygon set. Where those
    differ, the cell paints ground it has no data for — and the finer or coarser
    cell that should show there never gets the chance.
    """
    rings = defaultdict(list)
    for c in coverage:
        for poly in c["polygons_px"]:
            if len(poly) >= 4:
                rings[c["chart"]].append(poly)

    print("chart coverage (M_COVR) versus what was drawn:\n")
    for chart in sorted({a["chart"] for a in areas}):
        polys = rings.get(chart, [])
        drawn = [a for a in areas if a["chart"] == chart]
        if not polys:
            print(f"  {chart}: no M_COVR rings — cannot check ({len(drawn)} primitives)")
            continue

        # A coverage ring with no area masks nothing. These rings drive the
        # quilt stencil, so a degenerate one silently disables masking for the
        # chart rather than failing.
        #
        # Degenerate means zero width or height — not "few distinct
        # coordinates". An M_COVR ring is usually an axis-aligned rectangle,
        # which has exactly two distinct x and two distinct y values; counting
        # those condemned every valid coverage ring in the corpus.
        def flat_ring(p):
            xs = [q[0] for q in p]
            ys = [q[1] for q in p]
            return max(xs) - min(xs) < 1e-6 or max(ys) - min(ys) < 1e-6

        flat = [p for p in polys if flat_ring(p)]
        if flat:
            print(
                f"  {chart}: {len(flat)} of {len(polys)} coverage ring(s) are DEGENERATE"
                f" (zero width or height) — quilt masking for this chart is unreliable"
            )
            if len(flat) == len(polys):
                continue
        outside = 0
        for a in drawn:
            cx = sum(p[0] for t in a["tris_px"] for p in t) / max(1, 3 * len(a["tris_px"]))
            cy = sum(p[1] for t in a["tris_px"] for p in t) / max(1, 3 * len(a["tris_px"]))
            if not any(point_in_poly((cx, cy), poly) for poly in polys):
                outside += 1
        pct = 100.0 * outside / max(1, len(drawn))
        flag = "  <-- drawing outside its own coverage" if pct > 5 else ""
        print(
            f"  {chart}: {len(polys)} ring(s), {outside}/{len(drawn)} primitives"
            f" centred outside coverage ({pct:.0f}%){flag}"
        )


def quilt(viewport, areas):
    """Which charts contributed to each tile, and which overlap.

    manx quilts several charts into one view; where two of them cover the
    same tile, the finer one should win and the coarser one should only fill
    what the finer does not cover. A tile fed by two charts at different scales
    is where that goes wrong.
    """
    by_tile = defaultdict(Counter)
    for a in areas:
        by_tile[tuple(a["tile"])][a["chart"]] += 1

    multi = {t: c for t, c in by_tile.items() if len(c) > 1}
    print(f"{len(by_tile)} tiles carried area geometry; {len(multi)} drew from more than one chart\n")
    for tile, charts in sorted(multi.items())[:20]:
        z, x, y = tile
        parts = ", ".join(f"{name} ({n})" for name, n in charts.most_common())
        print(f"  tile z{z}/{x}/{y}: {parts}")
    if len(multi) > 20:
        print(f"  ... and {len(multi) - 20} more")

    print("\ncharts contributing to the view:")
    totals = Counter()
    for c in by_tile.values():
        totals.update(c)
    for name, n in totals.most_common():
        print(f"  {n:6d}  {name}")


def missing(viewport, areas, skips):
    """What this view is missing, ranked by how much of the screen it would cover.

    Sorting by feature count answers the wrong question — 23,000 dropped
    building outlines matter less than five dropped lagoons. Rank by on-screen
    area instead, and the classes that change the picture come first.
    """
    W, H = viewport["width_px"], viewport["height_px"]
    drawn = {(a["chart"], a["feature"]) for a in areas}
    per_class = defaultdict(lambda: [0, 0.0, set()])  # count, px^2, reasons

    seen = set()
    for s in skips:
        ext = s.get("extent_px")
        if not ext or s["reason"] == "outside-tile":
            continue
        key = (s["chart"], s["feature"])
        if key in drawn or key in seen:
            continue  # drawn in some other tile, or already counted
        seen.add(key)
        x0, y0 = max(0.0, min(ext[0], ext[2])), max(0.0, min(ext[1], ext[3]))
        x1, y1 = min(W, max(ext[0], ext[2])), min(H, max(ext[1], ext[3]))
        if x1 <= x0 or y1 <= y0:
            continue  # extent lies off-screen
        entry = per_class[s["class"]]
        entry[0] += 1
        entry[1] += (x1 - x0) * (y1 - y0)
        entry[2].add(s["reason"])

    total = W * H
    print(f"features overlapping the {W:.0f}x{H:.0f} view that were never drawn:\n")
    print(f"  {'class':<10}{'count':>7}{'screen area':>14}   reasons")
    for cls, (n, area, reasons) in sorted(per_class.items(), key=lambda kv: -kv[1][1]):
        print(f"  {cls:<10}{n:>7}{100.0 * area / total:>13.1f}%   {', '.join(sorted(reasons))}")
    if not per_class:
        print("  nothing — every feature overlapping the view was drawn")


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    viewport, areas, coverage, skips, lines = load(sys.argv[1])
    if viewport is None:
        print("no viewport record — is this a --dump-scene file?")
        return 2

    if "--quilt" in sys.argv:
        quilt(viewport, areas)
        print()
        coverage_report(viewport, areas, coverage)
        return 0

    if "--missing" in sys.argv:
        missing(viewport, areas, skips)
        return 0

    if "--at" in sys.argv:
        px, py = (float(v) for v in sys.argv[sys.argv.index("--at") + 1].split(","))
        query(viewport, areas, skips, lines, px, py)
        return 0

    findings, by_source, total_tris = check(viewport, areas)
    print(
        f"{len(areas)} area primitives, {total_tris} triangles, "
        f"viewport {viewport['width_px']:.0f}x{viewport['height_px']:.0f} "
        f"at {viewport['mpp']:.4f} m/px\n"
    )
    print("geometry source:")
    for src, n in by_source.most_common():
        print(f"  {n:6d}  {src}")

    print()
    if not findings:
        print("invariants: all triangles inside their feature's extent, none degenerate")
        return 0

    print("INVARIANT VIOLATIONS:")
    for kind, items in sorted(findings.items(), key=lambda kv: -len(kv[1])):
        print(f"  {len(items):6d}  {kind}")
        for it in items[:5]:
            print(f"            {it}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
