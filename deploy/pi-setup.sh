#!/usr/bin/env bash
# One-time setup of a Raspberry Pi 5 (Raspberry Pi OS 64-bit, desktop) to
# build and run manx. Run on the Pi:  bash ~/manx/deploy/pi-setup.sh
set -euo pipefail

echo "== System packages"
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
    build-essential pkg-config curl rsync \
    libxkbcommon-dev libwayland-dev libx11-dev libxcursor-dev libxrandr-dev \
    libxi-dev libudev-dev mesa-vulkan-drivers vulkan-tools

echo "== Rust"
if ! command -v cargo >/dev/null 2>&1 && [ ! -x "$HOME/.cargo/bin/cargo" ]; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
# shellcheck disable=SC1091
source "$HOME/.cargo/env"
rustc --version

echo "== GPU"
# wgpu draws the chart through Vulkan; the Pi 5's V3D has a Mesa driver.
vulkaninfo --summary 2>/dev/null | grep -E "deviceName|apiVersion" | head -4 || \
    echo "vulkaninfo not available yet — it works after a reboot into the desktop"

echo "== Screen blanking off"
# A plotter is watched, not touched: Raspberry Pi OS blanks an idle screen,
# chart and depth with it. The compositor does the blanking, and winit has
# no call to hold it off, so it is switched off here (1 is raspi-config's
# "no"). Takes effect after a reboot.
sudo raspi-config nonint do_blanking 1

echo "== Autostart"
mkdir -p "$HOME/.config/autostart"
cp "$HOME/manx/deploy/manx.desktop" "$HOME/.config/autostart/manx.desktop"
sed -i "s|@HOME@|$HOME|g" "$HOME/.config/autostart/manx.desktop"

echo "Done. Build with:  cd ~/manx && cargo build --release"
