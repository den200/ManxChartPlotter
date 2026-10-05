# Running Manx on Android

Manx runs on 64-bit ARM Android 8 and later. It's tested on an ODROID-C5
running Android 14. This page covers getting it onto a device, charts, and
the one thing that must not be lost: the device's o-charts identity.

## Build and install

On the Mac, from the project folder:

```sh
deploy/build-apk.sh              # → target/manx.apk
INSTALL=1 deploy/build-apk.sh    # and adb install it
```

The script needs the Android SDK/NDK, a JDK and the Rust target
`aarch64-linux-android`. It fetches o-charts' Android helper itself (see
below). To install over the network, switch on network ADB on the device and
run `adb connect <device-ip>:5555` first.

## ODROID-C5: one-time setup

Found on the boat's C5 (Android 14, build 151). None of it is Manx's doing,
but each makes it a poor plotter until fixed:

- **Screen goes black for a few seconds when switching apps.** The C5
  defaults to 1080p at 59.94 Hz, while apps such as Chrome or Brave ask for
  60 Hz. Each switch between the two is a full HDMI reset. Make 60 Hz the
  default, once (it survives reboots):
  `adb shell cmd display set-user-preferred-display-mode 1920 1080 60.000004`
- **Wrong date after power-up, so nothing online works.** The C5 has no clock
  battery. It boots at its firmware's build date, and Android gives up on
  internet time after a few failed tries. Point it at a time server on the
  boat, such as a GPS-fed one, with Google's as a fallback:
  `adb shell settings put global ntp_server "ntp://<boat-ntp-host>|ntp://time.android.com"`
- **Google sign-in keeps resetting.** Google's security-key step opens the USB
  touch panel as if it were a key, and the touchscreen drops each time.
  Sign in from a computer with `scrcpy`, choose *Try another way → Enter your
  password*, and avoid the passkey and security-key options.

## Test hooks (`MANX_*`)

An Android app starts with no environment, so the `MANX_*` hooks a desktop
run takes from its shell (`MANX_PROFILE`, `MANX_STRESS`, `MANX_VIEW`, ...)
come from `manx.env` in the app's storage, one `KEY=VALUE` per line:

```sh
printf 'MANX_PROFILE=1\nMANX_STRESS=coast\n' > manx.env
adb push manx.env /data/local/tmp/
adb shell run-as org.navcore.plotter cp /data/local/tmp/manx.env files/
adb shell am force-stop org.navcore.plotter      # read at the next start
adb shell run-as org.navcore.plotter grep profile.fps files/manx.log
```

Delete the file (`run-as org.navcore.plotter rm files/manx.env`) to go back
to a normal start.

## The signing key: never change it

Every APK is signed with Manx's own key,
`~/.navcore/signing/navcore-release.jks`, with its password in `password`
beside it. `MANX_KEYSTORE` points elsewhere. If the key is missing, the
script falls back to a throwaway debug key and warns you.

Android installs an update only if it is signed with the same key. That
matters for o-charts too (next section): on devices without Widevine, a
device's o-charts identity is tied to this key. **Back the key and its
password up off the machine, and never replace the key.** An APK signed with
another key can't use the charts licensed to a device, and the slot can't be
recovered.

## Charts

**Free NOAA charts:** Charts → Free charts → pick a state → Download.

**o-charts:** follow the same steps as on any new machine:

1. Charts → o-charts shop → **1. Sign in**.
2. **2. This system**: register the device under a new name, such as
   `odroid`. This is free and uses no slot. Don't reuse another machine's
   name.
3. **3. Charts**: Download. The first download assigns one of the licence's
   slots to this device **permanently**, and Manx asks you to confirm.
   For an expired licence, it offers the editions your account's machines
   received while it was paid for.

Charts licensed to another machine don't decrypt here, so there's no point
copying them over.

## Will the licence survive…?

o-charts licenses a chart to a device's identity. On Android, Manx gives
o-charts' helper the device's **Widevine ID**, or, where the device has no
Widevine (the ODROID-C5 has only ClearKey), its **`ANDROID_ID`**.

| Event | Charts still work? |
|---|---|
| System update that keeps your apps and data | **Yes** |
| Manx update | **Yes**, if signed with the same key |
| Uninstall and reinstall Manx | **Yes**, if signed with the same key; charts need downloading again |
| Manx signed with a **different key** | **No** (`ANDROID_ID` changes) |
| **Factory reset**, or flashing a fresh image that erases user data | **No**: it's a new device to o-charts and needs a new slot |

The Widevine ID survives even a factory reset. `ANDROID_ID` doesn't.
**Before re-flashing a device that holds a slot**, check that the update
keeps user data. After an update, Manx's fingerprint in `files/license/`
should be unchanged.

## One app, one slot

o-charts' terms: on Android each app uses its own licence slot. Charts
installed in Manx can't be shared with OpenCPN or any other app, and
OpenCPN's can't be shared with Manx. The chart shop says so before it
spends the slot. See [o-charts terms](../README.md#o-charts-terms).

## How o-charts works on Android (for developers)

- **Helper:** o-charts' closed `oexserverd` 1.23 for arm64 Android, the file
  AvNav's o-charts provider ships. `build-apk.sh` fetches it and checks its
  SHA-256. It's packed as `lib/arm64-v8a/liboexserverd.so`, because since
  Android 10 an app may only run programs from its native library
  directory.
- **Identity:** the helper reads none of its own; with no ID the
  fingerprint's identity is empty. Manx passes `-y <Widevine ID>`
  (fingerprint `oc04R_<ts>.fpr`) or `-z <ANDROID_ID>` (`oc03R_<ts>.fpr`),
  and refuses to make a fingerprint without one
  (`src/decrypt/android_id.rs`).
- **Fingerprint:** the helper prints `timestamp;HEX` instead of writing a
  file. The hex is the same bytes a desktop `.fpr` holds.
- **Decryption:** the daemon gets the same ID flag and listens on the
  abstract Unix socket `com.opencpn.ocharts_pi`, instead of the desktop
  helper's `/tmp` FIFOs. It uses the same 1025-byte request and answers on
  the same connection.
- **Storage:** the licence directory, settings and charts live in the app's
  own storage, so they survive app updates.
