#!/usr/bin/env bash
# One-time setup of a Raspberry Pi 5 (Raspberry Pi OS 64-bit, desktop) to
# build and run navcore. Run on the Pi:  bash ~/navcore/deploy/pi-setup.sh
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

echo "== Autostart"
mkdir -p "$HOME/.config/autostart"
cp "$HOME/navcore/deploy/navcore.desktop" "$HOME/.config/autostart/navcore.desktop"
sed -i "s|@HOME@|$HOME|g" "$HOME/.config/autostart/navcore.desktop"

echo "Done. Build with:  cd ~/navcore && cargo build --release"
