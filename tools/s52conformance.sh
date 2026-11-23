#!/bin/sh
# S-52 conformance run: diff manx's symbology resolution against OpenCPN's.
#
#   tools/s52conformance.sh <chart_dir_or_file> [out_dir] [max_charts] [-v]
#
# Free S-57 charts only (NOAA ENCs): `--dump-ir` writes chart features to
# files, which o-charts' conditions do not allow for their charts.
#
# Exits non-zero when any feature diverges from OpenCPN for a reason that is
# not one of the known deviations listed in tools/s52oracle/README.md — and
# when the oracle itself fails, which once went unnoticed because its
# errors were thrown away.
#
# Pipeline:
#   manx --dump-ir   -> <out>.features.ndjson  (class/primitive/attributes)
#                       -> <out>.manx.ndjson   (manx's LUP + expanded rules)
#   s52oracle           -> <out>.oracle.ndjson    (OpenCPN s52plib, same features)
#   s52diff.py                                     ranked divergence report
#
# Both engines are given the SAME mariner settings (manx's defaults), so a
# divergence is always an engine difference, never a configuration difference.

set -e
CHARTS="${1:-charts/noaa-conformance/MA}"
OUT="${2:-/tmp/s52conf}"
MAX="${3:-}"

ROOT=$(cd "$(dirname "$0")/.." && pwd)
ORACLE="$ROOT/tools/s52oracle/build/s52oracle"
PLIB="$ROOT/assets/s52/chartsymbols.xml"

if [ ! -x "$ORACLE" ]; then
    echo "building s52oracle..."
    mkdir -p "$ROOT/tools/s52oracle/build"
    (cd "$ROOT/tools/s52oracle/build" && cmake .. -DCMAKE_BUILD_TYPE=Release >/dev/null && make -j8 >/dev/null)
fi

mkdir -p "$(dirname "$OUT")"

# MANX_VIEW_SCALE turns on the third comparison layer: visibility. Both
# engines are asked whether each feature would be drawn at this view scale,
# which is a decision neither the instruction diff nor the scene dump covers.
VIEW_SCALE="${MANX_VIEW_SCALE:-}"

# The ENC display category has to reach both engines, or the OTHER-category
# classes manx now draws by default read as thousands of divergences that
# are really a configuration difference. manx reads this env var directly
# (MarinerSettings::from_env).
DISPLAY_CAT="${MANX_DISPLAY_CAT:-all}"

# Depth settings, same story: manx reads these from the environment, so the
# oracle has to be given whatever the renderer would use. Defaults mirror
# MarinerSettings::default(), which mirrors s52plib's _MARparamVal.
SAFETY_DEPTH="${MANX_SAFETY_DEPTH:-3}"
SAFETY_CONTOUR="${MANX_SAFETY_CONTOUR:-3}"
SHALLOW_CONTOUR="${MANX_SHALLOW_CONTOUR:-2}"
DEEP_CONTOUR="${MANX_DEEP_CONTOUR:-6}"
TWO_SHADES=""
[ "${MANX_DEPTH_SHADES:-4}" = "2" ] && TWO_SHADES="--two-shades"

echo "== manx --dump-ir"
"$ROOT/target/release/manx" --dump-ir "$CHARTS" "$OUT" $MAX 2>&1 | tail -1

echo "== s52oracle (OpenCPN s52plib)"
# Settings mirror MarinerSettings::default() in src/s52/settings.rs, which in
# turn mirrors s52plib's own _MARparamVal defaults (s52utils.cpp). Keep the two
# in step: a mismatch here reports configuration differences as engine bugs.
"$ORACLE" --plib "$PLIB" \
    --points simplified \
    --boundaries plain \
    --safety-contour "$SAFETY_CONTOUR" \
    --safety-depth "$SAFETY_DEPTH" \
    --shallow-contour "$SHALLOW_CONTOUR" \
    --deep-contour "$DEEP_CONTOUR" \
    $TWO_SHADES \
    --depth-unit 1 \
    --display-cat "$DISPLAY_CAT" \
    ${VIEW_SCALE:+--view-scale $VIEW_SCALE} \
    < "$OUT.features.ndjson" > "$OUT.oracle.ndjson" 2> "$OUT.oracle.err" || {
        echo "FAIL: s52oracle exited with an error:" >&2
        tail -5 "$OUT.oracle.err" >&2
        exit 2
    }
nfeat=$(wc -l < "$OUT.features.ndjson")
nora=$(wc -l < "$OUT.oracle.ndjson")
if [ "$nora" -ne "$nfeat" ]; then
    echo "FAIL: the oracle resolved $nora of $nfeat features" >&2
    exit 2
fi

echo "== diff"
python3 "$ROOT/tools/s52diff.py" "$OUT.manx.ndjson" "$OUT.oracle.ndjson" \
    --features "$OUT.features.ndjson" \
    --safety-contour "$SAFETY_CONTOUR" --deep-contour "$DEEP_CONTOUR" ${4:+"$4"}
