# Installing Manx

**Beta. Not for navigation.** Always carry official charts.

## Run it

Unpack the download anywhere and run `manx` (`manx.exe` on Windows). Keep the `assets` folder next to it.

- **Raspberry Pi 5 / Odroid:** Raspberry Pi OS (Bookworm or Trixie) or any 64-bit ARM Linux with a Vulkan driver. The Pi's standard desktop has one.
- **macOS:** the app is not signed yet. The first time, right-click `manx` and choose Open, or run `xattr -dr com.apple.quarantine .` in the unpacked folder.
- **Windows:** free charts work; o-charts are not supported on Windows yet.

## Charts

- **Free charts:** Charts → Free charts. NOAA charts download and open in the app.
- **o-charts:** buy and install them from Charts → Chart shop. To read them, Manx needs o-charts' own decryption helper, which we may not redistribute:
  1. Install the o-charts plugin in OpenCPN, or download it from o-charts.
  2. Copy `oexserverd` and the libraries beside it into `~/.manx/decoder/`.
  3. On a Mac, the helper is Intel-only: install Rosetta with `softwareupdate --install-rosetta`.

Each machine uses one of your o-charts licence slots.

## Boat data

Manx reads everything from [Signal K](https://signalk.org). Open Instruments and enter your server's address, for example `openplotter.local:3000`.

## Android

Download the `.apk` from the release page and install it. 64-bit ARM (for example an ODROID-C5).
