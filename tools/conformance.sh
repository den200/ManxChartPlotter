#!/bin/bash
# Manx against OpenCPN, as oracles, with a verdict.
#
#   tools/conformance.sh [-v]
#
# Two independent checks, either of which fails the run:
#
#  * S-52 symbology — every feature of the NOAA Massachusetts cells resolved
#    by Manx and by OpenCPN's s52plib (tools/s52conformance.sh). Fails on any
#    divergence that is not a known deviation (tools/s52oracle/README.md).
#  * S-57 reading — every NOAA Massachusetts cell, updates applied, read by
#    Manx and by OpenCPN's S-57 reader (tools/s57diff.py). Fails on any cell
#    whose features, attributes or geometry differ.
#
# Both run on free NOAA charts. Until November 2026 the S-52 check ran on
# the Danish o-charts set; it dumped decrypted features to files, which
# o-charts' conditions do not allow, and `--dump-ir` now refuses o-charts.
#
# Local only: it needs the OpenCPN sources under doc/, which are not in the
# repository. The NOAA cells are downloaded into charts/noaa-conformance/
# the first time.
set -uo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.local/rust/bin:$PATH"
VERBOSE="${1:-}"

NOAA="charts/noaa-conformance/MA"
OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT

cargo build --release --bin manx 2>&1 | grep -E "^error" && exit 2

status=0

if [ ! -d "$NOAA" ]; then
    mkdir -p charts/noaa-conformance
    curl -sS --max-time 900 -o charts/noaa-conformance/MA.zip \
        https://www.charts.noaa.gov/ENCs/MA_ENCs.zip \
        && unzip -q -o charts/noaa-conformance/MA.zip -d "$NOAA" \
        && rm charts/noaa-conformance/MA.zip
fi

echo "=== S-52 symbology: $NOAA"
if [ -d "$NOAA" ]; then
    tools/s52conformance.sh "$NOAA" "$OUT/s52" "" "$VERBOSE" | sed -n '/^compared/,$p' || status=1
else
    echo "FAIL: $NOAA is missing"
    status=1
fi

echo
echo "=== S-57 reading: $NOAA"
ORACLE=tools/s57oracle/build/s57oracle
[ -x "$ORACLE" ] || tools/s57oracle/build.sh >/dev/null
cells=0
bad_cells=0
while IFS= read -r cell; do
    cells=$((cells + 1))
    name="$(basename "$cell" .000)"
    "$ORACLE" "$cell" > "$OUT/$name.oracle" 2>/dev/null \
        && target/release/manx --s57-dump "$cell" > "$OUT/$name.manx" 2>/dev/null \
        || { echo "  $name: a reader failed"; bad_cells=$((bad_cells + 1)); continue; }
    if ! report="$(python3 tools/s57diff.py "$OUT/$name.oracle" "$OUT/$name.manx" $VERBOSE)"; then
        bad_cells=$((bad_cells + 1))
        echo "$report"
    fi
done < <(find "$NOAA" -name '*.000' | sort)
if [ "$cells" -eq 0 ]; then
    echo "FAIL: no cells found"
    status=1
elif [ "$bad_cells" -gt 0 ]; then
    echo "FAIL: $bad_cells of $cells cells differ from OpenCPN"
    status=1
else
    echo "PASS: $cells cells read exactly as OpenCPN reads them"
fi

echo
[ "$status" -eq 0 ] && echo "conformance: PASS" || echo "conformance: FAIL"
exit "$status"
