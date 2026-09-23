# Raspberry Pi 5 test rig

The Pi 5 on Denis's desk is where navcore gets tested on real target hardware.
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
deploy/deploy-to-pi.sh           # rsync → ~/navcore on the Pi, cargo build --release there, restart navcore on its screen
NORUN=1 deploy/deploy-to-pi.sh   # build only
```

This is the everyday loop: edit on the Mac, run it, look at the Dell.

- Builds **on the Pi** (aarch64). Never cross-copy a Mac binary. The first
  build took 10 min; incremental ones take a minute or two.
- Don't run a deploy while a build is still going on the Pi. The rsync would
  change sources under the running cargo.
- `pi-setup.sh` has already run: apt deps and rustup in `~/.cargo`. Its XDG
  autostart is **disabled** on this rig (moved to
  `~/navcore-autostart.desktop.disabled`), because a crash at startup would
  loop.
- **If the desktop dies** (e.g. a GPU hang: `dmesg` shows
  `v3d_reset: Resetting GPU for hang`), labwc exits and the Pi drops to the
  LightDM login screen. `den` has no password, so it can't be typed in there.
  Recover over SSH with `ssh rpi5 'sudo systemctl restart lightdm'` (autologin
  runs again). First seen 2026-09-23: zooming into NOAA California cells
  (US5SAN*, US6CA77M) hung V3D.
- `deploy/pi-run.sh` (on the Pi) kills any running navcore and starts it on the
  desktop's Wayland session (`wayland-0`, `/run/user/1000`) with
  `RUST_LOG=info`, logging to `~/navcore.log`. Restart the app without
  rebuilding: `ssh rpi5 'bash ~/navcore/deploy/pi-run.sh'`. Follow the log:
  `ssh rpi5 'tail -f ~/navcore.log'`.
- Screenshot of the Pi's screen: `ssh rpi5 'WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/1000 grim /tmp/s.png' && scp rpi5:/tmp/s.png .`
- Check the outputs: `ssh rpi5 'WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/1000 wlr-randr'`
- GPU: V3D through Mesa Vulkan (`vulkaninfo --summary`).

## Charts / licence

The Pi is its own o-charts machine. `license/` and `charts/` are deliberately
not deployed (see `deploy/README.md`). Register it as its own system from
navcore's Charts window before downloading. The decryption helper comes from
`oeserverd/linuxarm64/`, which the deploy rsync does copy.
