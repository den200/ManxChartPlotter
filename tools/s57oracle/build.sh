#!/usr/bin/env bash
# Build the S-57 reader oracle without CMake (clang++ only).
#   tools/s57oracle/build.sh            -> tools/s57oracle/build/s57oracle
# OCPN overrides the OpenCPN source tree (default: doc/reference projects/OpenCPN).
set -e

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
OCPN=${OCPN:-"$ROOT/doc/reference projects/OpenCPN"}
L="$OCPN/libs"
OUT="$HERE/build"
CXX=${CXX:-clang++}

[ -f "$L/s57-charts/src/s57reader.cpp" ] || { echo "OpenCPN sources not found at $OCPN" >&2; exit 1; }
mkdir -p "$OUT/obj"

# s57-charts/include comes first: libs/iso8211/include carries an older copy of s57.h.
INC=(-I"$L/s57-charts/include" -I"$L/iso8211/include" -I"$L/gdal/include" -I"$L/gdal/include/gdal")
FLAGS=(-std=c++17 -O2 -w)

objs=()
for f in "$L"/iso8211/src/*.cpp "$L"/gdal/src/*.cpp \
         "$L/s57-charts/src/ogrs57datasource.cpp" "$L/s57-charts/src/ogrs57layer.cpp" \
         "$L/s57-charts/src/s57classregistrar.cpp" "$L/s57-charts/src/s57featuredefns.cpp" \
         "$L/s57-charts/src/s57reader.cpp"; do
  o="$OUT/obj/$(basename "$f" .cpp).o"
  if [ ! -f "$o" ] || [ "$f" -nt "$o" ]; then
    echo "  CXX $(basename "$f")"
    $CXX "${FLAGS[@]}" "${INC[@]}" -c "$f" -o "$o"
  fi
  objs+=("$o")
done

echo "  CXX main.cpp"
$CXX "${FLAGS[@]}" -Wall -Wno-unused-variable "${INC[@]}" -DOCPN_S57DATA="\"$OCPN/data/s57data\"" \
  -c "$HERE/src/main.cpp" -o "$OUT/obj/main.o"

$CXX "${objs[@]}" "$OUT/obj/main.o" -o "$OUT/s57oracle"
echo "built $OUT/s57oracle"
