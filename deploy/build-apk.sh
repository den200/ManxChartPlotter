#!/usr/bin/env bash
# Build navcore's Android APK (arm64, e.g. the ODROID-C5) on the Mac:
#   deploy/build-apk.sh              → target/navcore.apk
#   INSTALL=1 deploy/build-apk.sh    → and adb install it
# Needs the Android SDK/NDK (ANDROID_HOME, default the Homebrew
# android-commandlinetools), a JDK for apksigner, and the Rust target
# aarch64-linux-android. No Gradle: the app is a NativeActivity, so the APK
# is the manifest, libnavcore.so and assets.zip (the project's assets/).
set -euo pipefail
cd "$(dirname "$0")/.."

ANDROID_HOME="${ANDROID_HOME:-/opt/homebrew/share/android-commandlinetools}"
JAVA_HOME="${JAVA_HOME:-/opt/homebrew/opt/openjdk@17}"
NDK="${NDK:-$(ls -d "$ANDROID_HOME"/ndk/* | sort -V | tail -1)}"
BT="$(ls -d "$ANDROID_HOME"/build-tools/* | sort -V | tail -1)"
PLATFORM="$ANDROID_HOME/platforms/android-34/android.jar"
TC="$NDK/toolchains/llvm/prebuilt/darwin-x86_64/bin"
API=26
export PATH="$JAVA_HOME/bin:$HOME/.local/rust/bin:$HOME/.cargo/bin:$PATH"
export CC_aarch64_linux_android="$TC/aarch64-linux-android$API-clang"
export AR_aarch64_linux_android="$TC/llvm-ar"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$TC/aarch64-linux-android$API-clang"

cargo build --release -p navcore-android --target aarch64-linux-android

OUT=target/apk
rm -rf "$OUT" && mkdir -p "$OUT/lib/arm64-v8a" "$OUT/assets"
"$TC/llvm-strip" -o "$OUT/lib/arm64-v8a/libnavcore.so" target/aarch64-linux-android/release/libnavcore.so
# Entries keep their assets/ prefix: navcore runs from the directory that
# holds assets/, and unpacks this zip into that directory.
zip -qr -X "$OUT/assets/assets.zip" assets -x '*/.*'


"$BT/aapt2" link -o "$OUT/base.apk" --manifest android/AndroidManifest.xml -I "$PLATFORM" \
    --min-sdk-version $API --target-sdk-version 34 -A "$OUT/assets" -0 zip
(cd "$OUT" && zip -q base.apk lib/arm64-v8a/libnavcore.so)
"$BT/zipalign" -p -f 4 "$OUT/base.apk" "$OUT/aligned.apk"

# A debug key: enough to install for testing. Made once, kept in ~/.android.
KEY="$HOME/.android/debug.keystore"
if [ ! -f "$KEY" ]; then
    mkdir -p "$HOME/.android"
    keytool -genkeypair -keystore "$KEY" -storepass android -keypass android -alias androiddebugkey \
        -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=Android Debug,O=Android,C=US" >/dev/null
fi
"$BT/apksigner" sign --ks "$KEY" --ks-pass pass:android --out target/navcore.apk "$OUT/aligned.apk"
echo "built target/navcore.apk ($(du -h target/navcore.apk | cut -f1))"

if [ "${INSTALL:-}" = 1 ]; then
    "$ANDROID_HOME/platform-tools/adb" install -r target/navcore.apk
fi
