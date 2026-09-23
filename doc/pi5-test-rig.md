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

## Deploy / build / run

```sh
deploy/deploy-to-pi.sh rpi5      # rsync source → ~/navcore on the Pi, cargo build --release there
```

- Builds **on the Pi** (aarch64). Never cross-copy a Mac binary.
- `pi-setup.sh` has already run: apt deps, rustup in `~/.cargo`, and XDG
  autostart in `~/.config/autostart/navcore.desktop`, so navcore starts with
  the desktop.
- The Wayland session belongs to user `den`. To start or restart the app on the
  Pi's screen from SSH:

  ```sh
  ssh rpi5 'pkill -x navcore; cd ~/navcore && WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/1000 nohup target/release/navcore >/tmp/navcore.log 2>&1 &'
  ssh rpi5 'tail -f /tmp/navcore.log'
  ```
- Screenshot of the Pi's screen: `ssh rpi5 'WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/1000 grim /tmp/s.png' && scp rpi5:/tmp/s.png .`
- Check the outputs: `ssh rpi5 'WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/1000 wlr-randr'`
- GPU: V3D through Mesa Vulkan (`vulkaninfo --summary`).

## Charts / licence

The Pi is its own o-charts machine. `license/` and `charts/` are deliberately
not deployed (see `deploy/README.md`). Register it as its own system from
navcore's Charts window before downloading. The decryption helper comes from
`oeserverd/linuxarm64/`, which the deploy rsync does copy.
