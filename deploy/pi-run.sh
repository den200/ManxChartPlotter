#!/usr/bin/env bash
# (Re)start navcore on the Pi's own screen. Runs on the Pi, usually over ssh
# from deploy-to-pi.sh, which is why it points at the desktop's Wayland
# session instead of inheriting one. Log: ~/navcore.log
set -euo pipefail
pkill -x navcore 2>/dev/null && sleep 1 || true
export WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR="/run/user/$(id -u)"
export RUST_LOG="${RUST_LOG:-info}"
cd "$HOME/navcore"
setsid nohup target/release/navcore >"$HOME/navcore.log" 2>&1 </dev/null &
sleep 3
if pgrep -x navcore >/dev/null; then
    echo "navcore running on the Pi's screen (log: ~/navcore.log)"
else
    echo "navcore exited — last lines of ~/navcore.log:"; tail -20 "$HOME/navcore.log"; exit 1
fi
