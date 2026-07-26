#!/bin/sh
# S-52 conformance run: diff navcore's symbology resolution against OpenCPN's.
#
#   tools/s52conformance.sh <chart_dir_or_file> [out_dir] [max_charts]
#
# Pipeline:
#   navcore --dump-ir   -> <out>.features.ndjson  (class/primitive/attributes)
#                       -> <out>.navcore.ndjson   (navcore's LUP + expanded rules)
#   s52oracle           -> <out>.oracle.ndjson    (OpenCPN s52plib, same features)
#   s52diff.py                                     ranked divergence report
#
# Both engines are given the SAME mariner settings (navcore's defaults), so a
# divergence is always an engine difference, never a configuration difference.

set -e
CHARTS="${1:-charts/oeuSENC-DK-2025-1-20-base-macbook/}"
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

# NAVCORE_VIEW_SCALE turns on the third comparison layer: visibility. Both
# engines are asked whether each feature would be drawn at this view scale,
# which is a decision neither the instruction diff nor the scene dump covers.
VIEW_SCALE="${NAVCORE_VIEW_SCALE:-}"

# The ENC display category has to reach both engines, or the OTHER-category
# classes navcore now draws by default read as thousands of divergences that
# are really a configuration difference. navcore reads this env var directly
# (MarinerSettings::from_env).
DISPLAY_CAT="${NAVCORE_DISPLAY_CAT:-all}"

# Depth settings, same story: navcore reads these from the environment, so the
# oracle has to be given whatever the renderer would use. Defaults mirror
# MarinerSettings::default(), which mirrors s52plib's _MARparamVal.
SAFETY_DEPTH="${NAVCORE_SAFETY_DEPTH:-3}"
SAFETY_CONTOUR="${NAVCORE_SAFETY_CONTOUR:-3}"
SHALLOW_CONTOUR="${NAVCORE_SHALLOW_CONTOUR:-2}"
DEEP_CONTOUR="${NAVCORE_DEEP_CONTOUR:-6}"
TWO_SHADES=""
[ "${NAVCORE_DEPTH_SHADES:-4}" = "2" ] && TWO_SHADES="--two-shades"

echo "== navcore --dump-ir"
"$ROOT/target/release/navcore" --dump-ir "$CHARTS" "$OUT" $MAX 2>&1 | tail -1

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
    < "$OUT.features.ndjson" > "$OUT.oracle.ndjson" 2>/dev/null

echo "== diff"
python3 "$ROOT/tools/s52diff.py" "$OUT.navcore.ndjson" "$OUT.oracle.ndjson" \
    --features "$OUT.features.ndjson" "$4"
