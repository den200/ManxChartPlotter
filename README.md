# Manx

A fast, light chart plotter for the Raspberry Pi, Linux and Android.

![Manx on the approach to Copenhagen](doc/img/hero.png)

I'm Denis. I own a boat, and I couldn't stop myself geeking around with AI to see if I could build a brand-new chart plotter that runs smoothly on computers like the Raspberry Pi and Odroid. This is the result of about ten months of trial and error. It wouldn't have been possible without projects like [OpenCPN](https://opencpn.org).

## Download

[Latest release](https://github.com/den200/manx/releases/latest): Raspberry Pi and other 64-bit ARM Linux, Linux PCs, macOS (Apple silicon), Android. Windows is experimental.

Unpack and run `manx`. Setup, o-charts and Android: [deploy/INSTALL.md](deploy/INSTALL.md).

## Why

OpenCPN was too slow for me. Fixing it looked harder than starting over.

Raspberry Pis are showing up on more and more boats. They deserve a plotter built for them.

So Manx is Rust and Vulkan. Fast, light, modern, built to last.

A chart plotter can be blazing fast. Manx proves it.

## The name

A Manx shearwater was flown from Wales to Boston in the 1950s and released. Twelve days later it was back in its burrow, 5,000 km across unfamiliar ocean.

## Features

- **Charts.** S-52 presentation, as the standard draws it. o-charts (oeSENC) and free NOAA ENCs.
- **Chart shop.** Buy and install o-charts, or download NOAA charts, from inside the app.
- **Day, Dusk, Night.** IHO colour tables for the chart. The interface dims with it.
- **Signal K.** The only data source, so there is one thing to set up. Instruments, wind rose, own ship.
- **AIS.** Targets, CPA/TCPA alarms.
- **Routes.** Plan on the chart. GPX in and out. Shared with Signal K.
- **Weather.** GFS wind and gusts, waves, currents, rain, tides. Barbs and a colour layer on the chart.
- **Weather routing.** Isochrones, your boat's polar, waves and currents included.
- **Safety.** MOB, anchor watch, alarms that sound, track, logbook.
- **Touch first.** Big targets. North-up, head-up, two-finger twist.

## Numbers

Raspberry Pi 5 (4 GB), 1920 × 1080, NOAA charts of San Diego.

| | Panning and zooming every frame | View held still |
|---|---|---|
| Frame rate | 60 fps (display limit) | — |
| Frame time, 95th percentile | 16.8 ms | — |
| CPU | 28 % of one core | 0 % |
| GPU | 19 % busy | 0 % |
| Memory | 242 MB (263 MB peak) | 229 MB |

Manx draws only when something changes. Still, it costs nothing.

Measured with `MANX_PROFILE=1 MANX_STRESS=coast` and the rig's sampler ([doc/pi5-test-rig.md](doc/pi5-test-rig.md)).

## Screenshots

| Dusk | Night |
|---|---|
| ![Dusk](doc/img/dusk.png) | ![Night](doc/img/night.png) |

| Wind and gusts | Forecast |
|---|---|
| ![Wind barbs with gusts over the Kattegat](doc/img/wind.png) | ![The weather sheet](doc/img/sheet.png) |

## Runs on

- Raspberry Pi 5 (Raspberry Pi OS, Wayland)
- Linux (x86_64, arm64)
- Android (arm64, e.g. ODROID-C5)
- macOS (Apple silicon)
- Windows: experimental, free charts only for now

## Build

```sh
cargo run --release -- /path/to/charts
```

Raspberry Pi: [deploy/README.md](deploy/README.md). Android: [deploy/ANDROID.md](deploy/ANDROID.md).

## Status

Beta. **Not for navigation.** Always carry official charts.

## Thanks

Manx stands on the shoulders of others.

- **[OpenCPN](https://opencpn.org)**. Its S-52 portrayal code taught us how charts are drawn, and parts of Manx are translated from it. Without OpenCPN, no Manx.
- **[o-charts](https://o-charts.org)**. Affordable official charts, and `oexserverd` to read them.
- **[Signal K](https://signalk.org)**. One open language for everything on board.
- **[AvNav](https://www.wellenvogel.net/software/avnav/docs/beschreibung.html)**. Proof that a light plotter on a Pi works.
- **[QuteNav](https://github.com/jusirkka/qutenav)**. Showed o-charts working outside OpenCPN.
- **[IHO S-52](https://iho.int)**, **[NOAA](https://www.noaa.gov)** (ENCs, GFS), **[Open-Meteo](https://open-meteo.com)**, **[Natural Earth](https://www.naturalearthdata.com)**.
- **[wgpu](https://wgpu.rs)**, **[egui](https://www.egui.rs)**, **[winit](https://github.com/rust-windowing/winit)** and the Rust community.

## License

[GPL-3.0-or-later](LICENSE). Parts are derived from OpenCPN (GPLv2 or later), © David S. Register and the OpenCPN contributors.
