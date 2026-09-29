# Raspberry Pi 5 test rig

The Pi 5 on Denis's desk is where Manx gets tested on real target hardware.
Set up 2026-09-23.

## Reaching it

```sh
ssh rpi5            # alias in ~/.ssh/config → den@rpi5.local, key ~/.ssh/id_ed25519_rpi5
```

- **Key-only.** Password SSH is off and user `den` has *no* password at all.
  Always go through the `rpi5` alias. `ssh den@rpi5.local` without
  `-i ~/.ssh/id_ed25519_rpi5` offers the wrong key and fails with
  `Permission denied (publickey)`.
- `sudo` needs no password, so setup scripts run over plain `ssh rpi5 '...'`
  and don't need `ssh -t`.
- The Pi is on Wi-Fi with a DHCP address (was `192.168.1.249`). If `rpi5.local`
  stops resolving, use the IP: `ssh -i ~/.ssh/id_ed25519_rpi5 den@<ip>`.
- From Claude Code's Bash tool, SSH/rsync to the Pi needs
  `dangerouslyDisableSandbox: true` (network egress).

## The machine

- Pi 5, 4 GB RAM, 128 GB SD. Raspberry Pi OS trixie arm64 (image 2026-09-15),
  kernel 6.18. Desktop is **labwc (Wayland)**, auto-login as `den`.
- Set up by cloud-init from the SD card's `user-data`. Hostname `rpi5`,
  timezone Europe/Copenhagen, Wi-Fi regdom DK.
- **Display:** the Dell doesn't announce itself over HDMI (no hotplug, no EDID),
  so HDMI-0 (the port next to USB-C power) is forced on with
  `video=HDMI-A-1:1920x1080@60D` in `/boot/firmware/cmdline.txt`. The original
  is saved as `cmdline.txt.orig`. Without that line the desktop comes up with
  no outputs at all.
- **Input:** Logitech MX Anywhere 3S and MX Keys S over Bluetooth, paired and
  trusted, so they reconnect on boot. Keyboard layout is Danish (`dk`) in
  `/etc/default/keyboard` and `~/.config/labwc/environment`. To pair from SSH,
  use `bluetoothctl --agent NoInputNoOutput`. The default agent asks for a PIN
  that nobody can type in time.

## Deploy / build / run

```sh
deploy/deploy-to-pi.sh           # rsync → ~/manx on the Pi, cargo build --release there, restart Manx on its screen
NORUN=1 deploy/deploy-to-pi.sh   # build only
```

This is the everyday loop: edit on the Mac, run it, look at the Dell.

- Builds **on the Pi** (aarch64). Never cross-copy a Mac binary. The first
  build took 10 min; incremental ones take a minute or two.
- Don't run a deploy while a build is still going on the Pi. The rsync would
  change sources under the running cargo.
- `pi-setup.sh` has already run: apt deps and rustup in `~/.cargo`. Its XDG
  autostart is **disabled** on this rig (moved to
  `~/manx-autostart.desktop.disabled`), because a crash at startup would
  loop.
- **If the desktop dies** (e.g. a GPU hang: `dmesg` shows
  `v3d_reset: Resetting GPU for hang`), labwc exits and the Pi drops to the
  LightDM login screen. `den` has no password, so it can't be typed in there.
  Recover over SSH with `ssh rpi5 'sudo systemctl restart lightdm'` (autologin
  runs again). First seen 2026-09-23: zooming into NOAA California cells
  (US5SAN*, US6CA77M) hung V3D.
- `deploy/pi-run.sh` (on the Pi) kills any running Manx and starts it,
  traced, on the desktop's Wayland session (`wayland-0`, `/run/user/1000`),
  with `RUST_LOG=info` and `RUST_BACKTRACE=1`. Restart without rebuilding:
  `ssh rpi5 'bash ~/manx/deploy/pi-run.sh'`. Follow the log:
  `ssh rpi5 'tail -f ~/manx.log'`.
- **Desktop shortcut** "Manx (test)" (`deploy/manx-test.desktop`,
  reinstalled by every deploy) runs the same `pi-run.sh`. Denis double-clicks it
  to test the latest deployed build. On a failed start it shows the log tail
  in a zenity dialog. `quick_exec=1` in `~/.config/libfm/libfm.conf` stops the
  "execute?" prompt.

## Tracing

Every start through `pi-run.sh` is a trace session in
`~/manx-traces/<UTC time>/` on the SD card (`latest` → newest; 30 kept).
It's written by `deploy/pi-trace.sh` and deliberately not in `/tmp`, which
is RAM here, so it survives a desktop crash:

| file | what |
|---|---|
| `manx.log` | app log, panics, backtraces |
| `system.csv` | 1 s samples (`deploy/pi_sampler.py`, fsync'd each row): CPU %, load, mem available, zram, Manx CPU/RSS/threads, V3D render/bin busy % and jobs/s (from `/sys/class/drm/card0/device/gpu_stats`), temp, ARM/V3D MHz, core V, EXT5V V, `get_throttled` |
| `kernel.log` | kernel messages during the run (`v3d_reset … hang` lands here) |
| `session.txt` | build (`~/manx/BUILD`, written by deploy: git describe + branch), binary time, kernel, Mesa, start/end, exit code/signal, throttle flags |

On the Mac: `deploy/pi-trace-pull.sh [session]` rsyncs everything to
`traces/pi/` (gitignored) and runs `tools/pi_trace_report.py`. That prints
min/avg/max per metric, throttle flags decoded over time, each suspicious
kernel line with the Manx log and samples of the 5 s before it, and
WARN/ERROR grouped by message.

Known from the first traces (2026-09-23): no fan on this Pi, so throttle flags
`0xe0000` (soft temp limit and ARM cap *occurred*) show up; no under-voltage
despite the "can't supply 5 A" popup (that's only PD negotiation with the
Dell dock).

### Reproducing a GPU hang

Manx draws only when something changes, so a still view never hangs the GPU.
Two env hooks make a hang reproducible without hands on the mouse:

- `MANX_STRESS=1` pans in a circle and zooms out ~64× and back, every frame.
  `MANX_STRESS=coast` follows the coast from San Diego Bay to LA harbour and
  back at 0.3–8 m/px. Both redraw at 60 fps (~180 V3D jobs/s in `system.csv`).
- `MANX_WIND=fill` fetches the wind for the starting view and turns on its
  colour wash. With `MANX_STRESS=1` the view then runs off the forecast,
  which is how the empty-mesh UI crash (below) was reproduced.
- `MANX_SKIP=bg,area,pattern,line,sector,symbol,text,label,mariner,stroke,lc`
  leaves those layers out, to bisect a hang by layer (`stroke` and `lc` split
  `line` into plain strokes and LC() symbol lines).

```sh
ssh rpi5 'MANX_STRESS=coast MANX_VIEW=32.68,-117.235,1 bash ~/manx/deploy/pi-run.sh'
```

The first V3D hang (2026-09-23) was the LC() fragment shader (`fs_lc`). An inner
loop bounded by a uniform (`i < u.lc_count`) hung V3D within seconds under
`MANX_STRESS`. The same loop with a constant bound and a `break` doesn't.
Keep shader loops constant-bounded on this GPU. The `Corrupt prim … huge=true`
warnings seen at the time were a false alarm from a too-tight check in
`senc/geometry.rs`.

The second crash (2026-09-23, panning DK with the wind wash on) was not the
GPU: `Buffer slices can not be empty` in egui-wgpu. Where the view had no
forecast data, the wash was a mesh of corners with no triangles. egui passes
that on, and wgpu panics on its empty index range. `ui_batch.rs` now never
hands egui a mesh without triangles.
- Screenshot of the Pi's screen: `ssh rpi5 'WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/1000 grim /tmp/s.png' && scp rpi5:/tmp/s.png .`
- Check the outputs: `ssh rpi5 'WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/1000 wlr-randr'`
- GPU: V3D through Mesa Vulkan (`vulkaninfo --summary`).

## Charts / licence

The Pi is its own o-charts machine. `license/` and `charts/` are deliberately
not deployed (see `deploy/README.md`). Register it as its own system from
Manx's Charts window before downloading. The decryption helper comes from
`oeserverd/linuxarm64/`, which the deploy rsync does copy.
