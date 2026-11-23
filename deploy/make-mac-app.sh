#!/bin/bash
# Build Manx.app from a release build: the binary in Contents/MacOS, the
# assets and the icon in Contents/Resources.
#
#   deploy/make-mac-app.sh [destination]     (default: target/Manx.app)
#
# The app is ad-hoc signed, not notarised: fine on the Mac that built it.
# o-charts still need oexserverd in ~/.manx/decoder (see deploy/INSTALL.md).
set -euo pipefail
cd "$(dirname "$0")/.."

DEST="${1:-target/Manx.app}"
VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
export PATH="$HOME/.local/rust/bin:$PATH"

cargo build --release --bin manx

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
APP="$TMP/Manx.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/manx "$APP/Contents/MacOS/"
cp -R assets "$APP/Contents/Resources/"

# The icon, from the 512 px mark at every size macOS asks for.
ICONSET="$TMP/Manx.iconset"
mkdir "$ICONSET"
for s in 16 32 128 256 512; do
    sips -z $s $s doc/logo/mark-512.png --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
    d=$((s * 2))
    [ $d -le 512 ] && sips -z $d $d doc/logo/mark-512.png --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/Manx.icns"

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Manx</string>
    <key>CFBundleDisplayName</key><string>Manx</string>
    <key>CFBundleIdentifier</key><string>org.manx.plotter</string>
    <key>CFBundleExecutable</key><string>manx</string>
    <key>CFBundleIconFile</key><string>Manx</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF

codesign --force --sign - "$APP"
rm -rf "$DEST"
mv "$APP" "$DEST"
echo "built $DEST ($VERSION)"
