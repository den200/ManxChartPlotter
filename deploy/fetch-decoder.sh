#!/usr/bin/env bash
# Put o-charts' decryption helper (oexserverd and the libraries it loads)
# into a Manx package:
#   deploy/fetch-decoder.sh <linux64|linuxarm64|osx> <package folder>
# → <package folder>/oeserverd/<platform>/, where Manx looks for it beside
#   its assets/ (resolve_oeserverd_binary in src/decrypt/mod.rs).
#
# The helper is o-charts' closed binary: Manx runs it and does not change it.
# It is not in git. The files are the ones Manx is tested with, taken from
# o-charts' own plugin repository at a fixed commit and checked against
# known digests; a copy already in oeserverd/ is used when it is there.
set -euo pipefail
cd "$(dirname "$0")/.."

PLATFORM="${1:?platform: linux64, linuxarm64 or osx}"
DEST="${2:?package folder}/oeserverd/$PLATFORM"
SOURCE=https://raw.githubusercontent.com/bdbcat/o-charts_pi/8cf01953ef2e854b17f87bb9d7eac5dd855e09a5/libs/oeserverd

case "$PLATFORM" in
linux64) FILES="
f0c0567f918e6ee89ead8a10393e624e339a79cd6503ce1cf38ec20291f4be06 oexserverd
d8d35040e02c5641da105f4df7910609c3295a2c79c060e388733454e2612b7c libsgllnx64-2.29.02.so
5c0bf043859cacfd569598980ad2bad17589fe80db92ba3bb02ccbdab5390752 libusb-0.1.so.4" ;;
linuxarm64) FILES="
114c9a1ac4b099f0cbd8762b714b03c664028b19e8849275653d9c4d68114d85 oexserverd
c1982f4c6458acd18bca60a64fc24c13e74753e4f8c22ee55bc50985a8b13f14 libsglarm64-2.31.0.0.so" ;;
osx) FILES="
6407620e394fdae652d16233c020aa8562445dceeb626a495e7bd1f8b40a0984 oexserverd
6af82539f7abf5d31f98a88309bfca370a97fd781eab8123e818eed7b0ba3aae libsglmac-2.38.0.0.dylib.x86_64" ;;
*) echo "no helper for $PLATFORM" >&2; exit 1 ;;
esac

sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -c1-64; }

mkdir -p "$DEST"
echo "$FILES" | while read -r digest name; do
    [ -n "$name" ] || continue
    if [ -f "oeserverd/$PLATFORM/$name" ]; then
        cp "oeserverd/$PLATFORM/$name" "$DEST/$name"
    else
        curl -fsSL -o "$DEST/$name" "$SOURCE/$PLATFORM/$name"
    fi
    [ "$(sha256 "$DEST/$name")" = "$digest" ] || { echo "unexpected $name for $PLATFORM" >&2; exit 1; }
    chmod 755 "$DEST/$name"
done
echo "o-charts helper for $PLATFORM in $DEST"
