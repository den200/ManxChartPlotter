#!/bin/bash
# Run the unit tests over and over in conditions that shake out tests which
# pass by luck: one thread and many, four time zones (two with half- and
# three-quarter-hour offsets, which is where local-midnight arithmetic breaks),
# and an empty HOME, so a test that quietly leans on this machine's settings,
# cache or charts fails here instead of passing everywhere it is run by Den.
#
#   tools/test-stress.sh [runs]        (default 20)
#
# The test binary is built once and then run directly: cargo itself needs the
# real HOME to find its registry, the tests must not have it.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.local/rust/bin:$PATH"

RUNS="${1:-20}"
ZONES=(UTC Europe/Copenhagen Pacific/Chatham America/St_Johns)

BIN="$(cargo test --lib --no-run --message-format=json 2>/dev/null \
    | python3 -c 'import json,sys
for line in sys.stdin:
    m = json.loads(line)
    if m.get("reason") == "compiler-artifact" and m.get("executable") and m["target"]["kind"] == ["lib"]:
        print(m["executable"])' | tail -1)"
[ -x "$BIN" ] || { echo "could not find the lib test binary" >&2; exit 2; }

# The tests read their fixtures relative to the crate, as cargo would run them.
export CARGO_MANIFEST_DIR="$PWD"

failed=0
for i in $(seq 1 "$RUNS"); do
    tz="${ZONES[$(( (i - 1) % ${#ZONES[@]} ))]}"
    threads=$(( i % 2 == 0 ? 1 : 0 ))   # 0 lets the harness choose
    home="$(mktemp -d)"
    args=(--quiet)
    [ "$threads" -eq 1 ] && args+=(--test-threads=1)
    if out="$(HOME="$home" TZ="$tz" "$BIN" "${args[@]}" 2>&1)"; then
        printf 'run %2d  ok    TZ=%-18s threads=%s\n' "$i" "$tz" "${threads/0/many}"
    else
        failed=$((failed + 1))
        printf 'run %2d  FAIL  TZ=%-18s threads=%s\n' "$i" "$tz" "${threads/0/many}"
        echo "$out" | grep -E "^test .* FAILED|panicked|^failures:" | head -20
    fi
    rm -rf "$home"
done

echo "$((RUNS - failed))/$RUNS runs passed"
[ "$failed" -eq 0 ]
