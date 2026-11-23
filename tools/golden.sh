#!/bin/bash
# Golden images: what the chart actually looks like, held still.
#
#   tools/golden.sh            capture every view and compare with its reference
#   tools/golden.sh --update   capture and make the captures the new references
#   tools/golden.sh NAME…      only the named views
#
# Unit tests and the OpenCPN oracles check what Manx decides to draw; this
# checks what reaches the screen — draw order, line widths, pattern phase,
# label placement, palettes — which nothing else does.
#
# Each capture runs with its own HOME (target/golden-home), so it sees Manx's
# default settings and never yours, and never writes to yours: the app saves
# its settings as it runs, and those must not leak from one capture into the
# next. The converted-chart cache stays in that HOME between runs, which is
# what keeps a run to a few seconds a view.
#
# References: every view is drawn from a public-domain NOAA cell of Boston
# harbour in tests/fixtures/s57-harbour and is committed (tests/golden/).
# There are no views of o-charts: their conditions forbid keeping a
# rendering of the charts, and MANX_SHOT refuses to capture one.
# Local only: the capture needs a GPU.
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
export PATH="$HOME/.local/rust/bin:$PATH"

UPDATE=0
ONLY=()
for a in "$@"; do
    case "$a" in
        --update) UPDATE=1 ;;
        *) ONLY+=("$a") ;;
    esac
done

NOAA="$ROOT/tests/fixtures/s57-harbour"
FIXTURE="42.262,-70.912,5"    # Boston harbour, the NOAA fixture cell US5BOSBE
WIDE="42.262,-70.912,9"       # the same, wider: depth shades and the safety contour

# name | reference dir | chart folder | view | extra environment
VIEWS=(
    "noaa-wide|.|$NOAA|$WIDE|MANX_PALETTE=day"
    "noaa-wide-safety-10m|.|$NOAA|$WIDE|MANX_PALETTE=day MANX_SAFETY_DEPTH=10 MANX_SAFETY_CONTOUR=10"
    "noaa-wide-standard|.|$NOAA|$WIDE|MANX_PALETTE=day MANX_DISPLAY_CAT=standard"
    "noaa-harbour-tilted|.|$NOAA|$FIXTURE|MANX_PALETTE=day MANX_TILT=40"
    "noaa-harbour-route|.|$NOAA|$FIXTURE|MANX_PALETTE=day MANX_ROUTE_NEW=42.255,-70.925;42.268,-70.900"
    "noaa-harbour-day|.|$NOAA|$FIXTURE|MANX_PALETTE=day"
    "noaa-harbour-dusk|.|$NOAA|$FIXTURE|MANX_PALETTE=dusk"
    "noaa-harbour-night|.|$NOAA|$FIXTURE|MANX_PALETTE=night"
)

cargo build --release --bin manx 2>&1 | grep -E "^error" && exit 2

GHOME="$ROOT/target/golden-home"
SETTINGS="$GHOME/Library/Application Support/manx/settings.json"
OUT="$ROOT/target/golden-out"
mkdir -p "$GHOME" "$OUT"

pass=0
fail=0
missing=0
for v in "${VIEWS[@]}"; do
    IFS='|' read -r name where charts view extra <<< "$v"
    if [ ${#ONLY[@]} -gt 0 ] && [[ ! " ${ONLY[*]} " == *" $name "* ]]; then
        continue
    fi
    ref_dir="$ROOT/tests/golden/$where"
    ref="$ref_dir/$name.png"
    cap="$OUT/$name.png"
    if [ ! -d "$charts" ]; then
        echo "skip  $name (no charts at $charts)"
        continue
    fi
    # Fresh defaults for every view, and a route store nothing else reads.
    rm -f "$SETTINGS" "$cap"
    routes="$(mktemp -d)"
    # shellcheck disable=SC2086
    env HOME="$GHOME" MANX_ROUTES="$routes" MANX_VIEW="$view" MANX_SIZE=1400x1100 \
        MANX_SHOT="$cap" $extra "$ROOT/target/release/manx" "$charts" \
        > "$OUT/$name.log" 2>&1
    rm -rf "$routes"
    if [ ! -s "$cap" ]; then
        echo "FAIL  $name: no capture (see $OUT/$name.log)"
        fail=$((fail + 1))
        continue
    fi
    if [ "$UPDATE" -eq 1 ]; then
        mkdir -p "$ref_dir"
        cp "$cap" "$ref"
        echo "bless $name"
        continue
    fi
    if [ ! -f "$ref" ]; then
        echo "MISS  $name: no reference — run with --update after checking $cap"
        missing=$((missing + 1))
        continue
    fi
    if result="$(python3 tools/golden_compare.py "$ref" "$cap" "$OUT/$name.diff.png")"; then
        echo "ok    $name: $result"
        pass=$((pass + 1))
    else
        echo "FAIL  $name: $result — see $OUT/$name.diff.png"
        fail=$((fail + 1))
    fi
done

[ "$UPDATE" -eq 1 ] && exit 0
echo "$pass passed, $fail failed, $missing without a reference"
[ "$fail" -eq 0 ] && [ "$missing" -eq 0 ]
