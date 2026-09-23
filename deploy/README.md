# Running navcore on the Raspberry Pi 5

This is what gets navcore from the Mac onto the plotter, and what to expect
the first time.

## What you need on the Pi

- Raspberry Pi OS (64-bit) **with desktop**, updated.
- SSH switched on (Raspberry Pi Imager → settings → enable SSH), and the Pi
  on the same network as the Mac.

## First time

On the Mac, from the project folder:

```sh
deploy/deploy-to-pi.sh pi@<pi-name>.local
```

The first time, this also sets the Pi up (system packages, Rust, and
starting navcore by itself when the desktop starts) and asks for the Pi's
password once. The first build takes 10–20 minutes; later ones a minute or
two.

## Charts on the Pi — read this before downloading

An o-charts licence belongs to **one machine**. The Pi is a new machine, so:

1. Start navcore on the Pi, open **Charts**, sign in.
2. **Register** the Pi under a new name (e.g. `boat-pi`).
3. **Download** your chart set. This assigns one of your licence's slots to
   the Pi, permanently — navcore asks you to confirm first.

The Mac's charts and licence files are deliberately *not* copied: they would
not decrypt on the Pi.

## Updating

After changes on the Mac: `deploy/deploy-to-pi.sh pi@<pi-name>.local`, then
restart navcore on the Pi (or reboot it).
