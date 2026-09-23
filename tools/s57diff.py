#!/usr/bin/env python3
"""Compare navcore's S-57 path with OpenCPN's reader, feature by feature.

    tools/s57diff.py <oracle.ndjson> <navcore.ndjson> [-v]

oracle: tools/s57oracle/build/s57oracle cell.000
navcore: navcore --s57-dump cell.000

Features are matched by FRID RCID. Checked: the class, every attribute
value (numerically where both parse as numbers), point positions, sounding
counts, positions and depths, line length and area size (in the Mercator
plane, where navcore draws them). Prints a count of each kind of
divergence per object class; -v adds examples.
"""
import json, math, sys
from collections import Counter, defaultdict

R = 6378137.0 * 0.9996


def merc(lon, lat):
    lat = max(min(lat, 85.0511), -85.0511)
    return (math.radians(lon) * R, math.log(math.tan(math.pi / 4 + math.radians(lat) / 2)) * R)


def ring_area(ring):
    pts = [merc(*p[:2]) for p in ring]
    return abs(sum(a[0] * b[1] - b[0] * a[1] for a, b in zip(pts, pts[1:]))) / 2


def length(line):
    pts = [merc(*p[:2]) for p in line]
    return sum(math.hypot(b[0] - a[0], b[1] - a[1]) for a, b in zip(pts, pts[1:]))


def load(path):
    head, feats = None, {}
    for line in open(path):
        o = json.loads(line)
        if "dsid" in o:
            head = o["dsid"]
        else:
            feats[o["rcid"]] = o
    return head, feats


def same_value(a, b):
    if a == b:
        return True
    try:
        return abs(float(a) - float(b)) < 1e-6
    except ValueError:
        return a.strip() == b.strip()


def compare(oracle, ours, verbose=False, name=""):
    oh, of = load(oracle)
    nh, nf = load(ours)
    issues = Counter()
    examples = defaultdict(list)

    def flag(kind, cls, msg):
        issues[(kind, cls)] += 1
        if len(examples[(kind, cls)]) < 3:
            examples[(kind, cls)].append(msg)

    for rcid, o in of.items():
        cls = o["acronym"]
        if o.get("geom") is None:
            continue  # OpenCPN writes no SENC record for these either
        n = nf.get(rcid)
        if n is None:
            flag("missing", cls, f"rcid {rcid}")
            continue
        if n["acronym"] != cls:
            flag("class", cls, f"rcid {rcid}: {n['acronym']}")
        for k, v in o["attrs"].items():
            if v in ("", "\u007f"):
                continue
            nv = n["attrs"].get(k)
            if nv is None:
                flag("attr-missing", cls, f"rcid {rcid} {k}={v!r}")
            elif not same_value(v, nv):
                flag("attr-value", cls, f"rcid {rcid} {k}: oracle {v!r} navcore {nv!r}")
        g, h = o["geom"], n.get("geom") or {}
        t = g["type"]
        if t == "Point":
            if h.get("type") != "Point":
                flag("geom-type", cls, f"rcid {rcid}: {h.get('type')}")
            elif max(abs(a - b) for a, b in zip(g["c"], h["c"])) > 1e-6:
                flag("point", cls, f"rcid {rcid}: {g['c']} vs {h['c']}")
        elif t == "MultiPoint":
            if h.get("type") != "MultiPoint":
                flag("geom-type", cls, f"rcid {rcid}: {h.get('type')}")
            elif len(g["c"]) != len(h["c"]):
                flag("soundings", cls, f"rcid {rcid}: {len(g['c'])} vs {len(h['c'])}")
            else:
                for a, b in zip(sorted(g["c"]), sorted(h["c"])):
                    if max(abs(a[0] - b[0]), abs(a[1] - b[1])) > 2e-6 or abs(a[2] - b[2]) > 0.01:
                        flag("sounding", cls, f"rcid {rcid}: {a} vs {b}")
                        break
        elif t in ("LineString", "MultiLineString"):
            parts = [g["c"]] if t == "LineString" else g["c"]
            lo = sum(length(p) for p in parts)
            if h.get("type") != "Lines":
                flag("geom-type", cls, f"rcid {rcid}: {h.get('type')}")
            elif lo > 1.0 and abs(h["len"] - lo) / lo > 0.01:  # slivers under a metre: noise
                flag("line-length", cls, f"rcid {rcid}: oracle {lo:.0} navcore {h['len']:.0}")
        elif t == "Polygon":
            rings = g["rings"]
            ao = ring_area(rings[0]) - sum(ring_area(r) for r in rings[1:]) if rings else 0
            if h.get("type") != "Area":
                flag("geom-type", cls, f"rcid {rcid}: {h.get('type')}")
            elif ao > 100.0 and abs(h["area"] - ao) / ao > 0.005:  # slivers under 100 m²: noise
                flag("area", cls, f"rcid {rcid}: oracle {ao:.0f} navcore {h['area']:.0f} ({len(rings)} rings)")
    for rcid, n in nf.items():
        o = of.get(rcid)
        if o is None or o.get("geom") is None:
            flag("extra", n["acronym"], f"rcid {rcid}")

    total = sum(1 for o in of.values() if o.get("geom") is not None)
    bad = sum(issues.values())
    print(f"{name}: {total} features, {bad} divergences")
    for (kind, cls), c in sorted(issues.items(), key=lambda x: -x[1]):
        print(f"  {kind:14} {cls:8} {c}")
        if verbose:
            for e in examples[(kind, cls)]:
                print(f"      {e}")
    return bad


if __name__ == "__main__":
    args = [a for a in sys.argv[1:] if a != "-v"]
    sys.exit(1 if compare(args[0], args[1], "-v" in sys.argv, args[1]) else 0)
