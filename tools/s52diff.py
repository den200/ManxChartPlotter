#!/usr/bin/env python3
"""Diff navcore's S-52 instruction stream against OpenCPN's (tools/s52oracle).

    navcore --dump-ir <charts> /tmp/run
    tools/s52oracle/build/s52oracle --plib assets/s52/chartsymbols.xml \
        < /tmp/run.features.ndjson > /tmp/run.oracle.ndjson
    tools/s52diff.py /tmp/run.navcore.ndjson /tmp/run.oracle.ndjson

Records are joined on `id`. Every divergence is bucketed by kind and object
class so the output ranks *what to fix next*, not just what differs.
"""

import json
import re
import sys
from collections import Counter, defaultdict

TOKEN_RE = re.compile(r"^([A-Z]{2})\((.*)\)$")


def parse_token(tok):
    """'LS(SOLD,1,CSTLN)' -> ('LS', ['SOLD','1','CSTLN']); unparsable -> (tok, [])."""
    m = TOKEN_RE.match(tok.strip().rstrip("\x1f"))
    if not m:
        return (tok.strip().rstrip("\x1f"), [])
    op, argstr = m.group(1), m.group(2)
    # chartsymbols.xml has rows with a stray extra ')' (LNDARE's TX); the greedy
    # regex leaves it inside the last argument. Drop unbalanced trailing parens
    # so this does not read as a value mismatch.
    while argstr.endswith(")") and argstr.count(")") > argstr.count("("):
        argstr = argstr[:-1]
    args, cur, depth, quoted = [], "", 0, False
    for c in argstr:
        if c == "'":
            quoted = not quoted
            continue
        if c == "," and not quoted and depth == 0:
            args.append(cur.strip())
            cur = ""
        else:
            if c == "(":
                depth += 1
            elif c == ")":
                depth -= 1
            cur += c
    args.append(cur.strip())
    return (op, args)


SOUND_GLYPH_RE = re.compile(r"^SY\(SOUND[GS][A-Z0-9]{2}\)$")


def normalise(toks, side, deviations):
    """Fold away differences navcore makes on purpose, counting each one.

    These are architectural choices, not bugs, and they are numerous enough
    that leaving them in the ranking would bury everything else. Each fold is
    reported separately so it stays visible.
    """
    out = []
    i = 0
    while i < len(toks):
        t = toks[i]

        # OpenCPN emits MP() for a multipoint sounding and expands it per point
        # inside the renderer; navcore has a dedicated multipoint path and
        # returns nothing from the CS procedure.
        if side == "opencpn" and t == "MP()":
            deviations["multipoint sounding expanded by the renderer, not CS"] += 1
            i += 1
            continue

        # navcore draws soundings as text wherever they appear; OpenCPN
        # composes them from per-digit symbols (SY(SOUNDG21);SY(SOUNDG12)).
        if side == "opencpn" and SOUND_GLYPH_RE.match(t):
            while i < len(toks) and SOUND_GLYPH_RE.match(toks[i]):
                i += 1
            out.append("SOUNDING()")
            deviations["sounding drawn as text, not digit symbols"] += 1
            continue
        if side == "navcore" and t.replace("'", "").startswith("TE(%4.1lf,VALSOU"):
            out.append("SOUNDING()")
            i += 1
            continue

        # navcore builds light descriptions and sector arcs in the tile builder
        # (litdsn01 / light_sector_info) instead of returning them from the CS
        # procedure, so its IR carries only the bare symbol.
        # LITDSN01's output is a quoted *literal* in display group 23; every
        # other TX names an attribute. That tells the two apart without
        # enumerating light-character abbreviations.
        if side == "opencpn" and t.startswith("TX('") and t.rstrip(")").endswith(",23"):
            deviations["light description emitted by the renderer, not CS"] += 1
            i += 1
            continue
        if side == "opencpn" and t.startswith("CA("):
            deviations["light sector arc emitted by the renderer, not CS"] += 1
            i += 1
            continue

        out.append(t)
        i += 1
    return out


def norm_num(s):
    """Compare 2 and 2.0 and 2.00 as equal; leave non-numerics alone."""
    try:
        f = float(s)
        return f"{f:g}"
    except (ValueError, TypeError):
        return s


def args_differ(a, b):
    """Return the index of the first differing arg, or None. '?' is navcore's
    marker for a field it does not model — reported separately, not as a value
    mismatch."""
    n = max(len(a), len(b))
    for i in range(n):
        x = a[i] if i < len(a) else "<missing>"
        y = b[i] if i < len(b) else "<missing>"
        if x == "?" or y == "?":
            continue
        if norm_num(x) != norm_num(y):
            return i
    return None


def load(path):
    out = {}
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            rec = json.loads(line)
            out[rec["id"]] = rec
    return out


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    nav = load(sys.argv[1])
    ora = load(sys.argv[2])
    verbose = "-v" in sys.argv
    feats = {}
    if "--features" in sys.argv:
        feats = load(sys.argv[sys.argv.index("--features") + 1])

    kinds = Counter()
    by_class = defaultdict(Counter)
    examples = defaultdict(list)
    unmodeled = Counter()
    deviations = Counter()
    n = 0

    for fid, nrec in sorted(nav.items()):
        orec = ora.get(fid)
        if orec is None:
            continue
        n += 1
        cls = nrec["obj"]
        nlup, olup = nrec.get("lup"), orec.get("lup")

        # CS procedures whose OpenCPN result depends on chart context the oracle
        # is not given (associated depth areas, floating/rigid ATON arrays).
        # Divergences involving these are suffixed [ctx] — they are suspect, not
        # proof of a navcore bug.
        # Once navcore ships the surrounding depth areas with a feature
        # ("assoc_used"), the oracle runs UDWHAZ03 over the same neighbourhood
        # and the result is decidable — no caveat needed.
        ctx_sensitive = not orec.get("assoc_used") and bool(
            set(orec.get("cs", [])) & {"OBSTRN04", "WRECKS02", "TOPMAR01", "DEPVAL01", "UDWHAZ03"}
        )

        def note(kind, detail):
            if ctx_sensitive and kind.startswith("expand"):
                kind += " [ctx]"
            kinds[kind] += 1
            by_class[kind][cls] += 1
            if len(examples[(kind, cls)]) < 3:
                examples[(kind, cls)].append((fid, detail))

        # A table mismatch means the two engines were configured differently
        # (simplified vs paper points, plain vs symbolized boundaries). Every
        # downstream divergence would then be meaningless, so call it out first.
        if nlup and olup:
            ntab = nlup.get("tnam", "").upper().replace("_BOUNDARIES", "").replace("_CHART", "")
            otab = olup.get("tnam", "").upper().replace("_BOUNDARIES", "").replace("_CHART", "")
            if ntab != otab:
                note("SETTINGS_MISMATCH_table", f"navcore={nlup.get('tnam')} opencpn={olup.get('tnam')}")

        if (nlup is None) != (olup is None):
            note("lup_missing", f"navcore={'None' if nlup is None else 'ok'} "
                               f"opencpn={'None' if olup is None else 'ok'}")
            continue
        if nlup is None:
            continue

        # --- LUP selection: same instruction string means the same LUP row ---
        ninst = nlup["inst"].replace("\x1f", "").strip()
        oinst = olup["inst"].replace("\x1f", "").strip()
        if ninst != oinst:
            note("lup_selection", f"navcore={ninst!r} opencpn={oinst!r}")
        if nlup["dpri"] != olup["dpri"]:
            note("lup_priority", f"navcore={nlup['dpri']} opencpn={olup['dpri']}")
        ndisc = nlup["disc"].upper()
        odisc = olup["disc"].upper()
        if ndisc[:4] != odisc[:4]:
            note("lup_category", f"navcore={ndisc} opencpn={odisc}")

        # --- visibility: would the feature be drawn at all? ---
        # A third decision layer, independent of symbology: display category,
        # SCAMIN and SUPER_SCAMIN. A feature can resolve to exactly the right
        # instructions and still never reach the screen.
        nvis = (nrec.get("visibility") or {}).get("visible")
        ovis = orec.get("visible")
        if nvis is not None and ovis is not None and nvis != ovis:
            v = nrec["visibility"]
            why = "scale" if not v.get("scale_ok") else "category"
            note(
                "visibility",
                f"navcore={'shown' if nvis else 'hidden (' + why + ')'} "
                f"opencpn={'shown' if ovis else 'hidden'}",
            )

        # --- expanded instruction stream ---
        # OP(...) is a priority override that some CS procedures emit, but
        # OpenCPN's StringToRules has no "OP" instruction: it scans straight
        # past the token and the override never reaches its renderer. Dropping
        # it here compares what each engine actually draws.
        # OpenCPN's DEPARE01 leaves `drval2` uninitialised when the feature has
        # no DRVAL2 (s52cnsy.cpp:621-631 — the intended `drval2 = drval1 + 0.01`
        # is commented out), so its depth shade comes from whatever was on the
        # stack. Every AC divergence in the corpus is a feature with no DRVAL2
        # and none with one, so navcore's reading is the correct one and the
        # oracle's answer here is not evidence.
        if cls in ("DEPARE", "DRGARE") and fid in feats:
            if "DRVAL2" not in feats[fid].get("attrs", {}):
                deviations["DEPARE01 depth shade (OpenCPN reads uninitialised DRVAL2)"] += 1
                orec = dict(orec)
                orec["expanded"] = [
                    t for t in orec["expanded"] if not t.startswith("AC(")
                ]
                nrec = dict(nrec)
                nrec["expanded"] = [
                    t for t in nrec["expanded"] if not t.startswith("AC(")
                ]

        nraw = [t.strip() for t in nrec["expanded"] if not t.startswith("OP(")]
        oraw = [t.strip() for t in orec["expanded"] if not t.startswith("OP(")]

        # OpenCPN's LIGHTS06 declares `orientstr` and leaves the assignment
        # commented out, so every directional light comes out as a question
        # mark whatever its ORIENT. navcore draws the oriented flare the S-52
        # procedure describes; drop both sides of that substitution.
        if "SY(QUESMRK1)" in oraw:
            deviations["directional light (OpenCPN's ORIENT handling is disabled)"] += 1
            oraw = [t for t in oraw if t != "SY(QUESMRK1)"]
            for k, t in enumerate(nraw):
                if t.startswith("SY("):
                    nraw.pop(k)
                    break
        ntok = [parse_token(t) for t in normalise(nraw, "navcore", deviations)]
        otok = [parse_token(t) for t in normalise(oraw, "opencpn", deviations)]
        if len(ntok) != len(otok):
            def show(toks):
                return [op + "(" + ",".join(args) + ")" for op, args in toks]
            note("expand_count", f"navcore={show(ntok)} opencpn={show(otok)}")
        else:
            for i, ((nop, nargs), (oop, oargs)) in enumerate(zip(ntok, otok)):
                if nop != oop:
                    note("expand_opcode",
                         f"#{i} navcore={nrec['expanded'][i].strip()!r} "
                         f"opencpn={orec['expanded'][i].strip()!r}")
                    break
                j = args_differ(nargs, oargs)
                if j is not None:
                    note(f"expand_args_{nop}",
                         f"#{i} arg{j} navcore={nrec['expanded'][i].strip()!r} "
                         f"opencpn={orec['expanded'][i].strip()!r}")
                    break
                # navcore modelled fewer args than OpenCPN emitted
                if len(nargs) < len(oargs):
                    unmodeled[f"{nop}:arg{len(nargs)}..{len(oargs)-1}"] += 1
                for k, a in enumerate(nargs):
                    if a == "?":
                        unmodeled[f"{nop}:arg{k}"] += 1

    print(f"compared {n} features\n")
    total_bad = sum(kinds.values())
    print(f"{total_bad} features with at least one divergence "
          f"({100.0*total_bad/max(n,1):.1f}%)\n")

    print("divergence by kind:")
    for kind, cnt in kinds.most_common():
        print(f"  {cnt:7d}  {kind}")
        for cls, c in by_class[kind].most_common(8):
            print(f"           {c:6d}  {cls}")
            if verbose:
                for fid, detail in examples[(kind, cls)]:
                    print(f"                    id={fid} {detail}")
    if deviations:
        print("\nknown deviations folded out (navcore renders these elsewhere):")
        for k, c in deviations.most_common():
            print(f"  {c:7d}  {k}")
    if unmodeled:
        print("\nfields OpenCPN emits that navcore does not model:")
        for k, c in unmodeled.most_common(15):
            print(f"  {c:7d}  {k}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
