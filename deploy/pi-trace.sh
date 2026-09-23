#!/usr/bin/env bash
# Run navcore on the Pi with a trace session around it. Started detached by
# pi-run.sh. Everything lands in ~/navcore-traces/<UTC time>/ on the SD card
# (not /tmp, which is RAM here), so it survives the desktop crashing:
#   navcore.log  the app's own log (RUST_LOG, default info) + panics/backtraces
#   system.csv   1 s samples: CPU, memory, navcore CPU/RSS, GPU busy, temp,
#                clocks, voltages, throttle flags (deploy/pi_sampler.py)
#   kernel.log   kernel messages while it runs — GPU resets show up here
#   session.txt  build, start, end and exit status
# ~/navcore.log always points at the latest navcore.log.
set -uo pipefail
cd "$HOME/navcore"
KEEP=30
DIR="$HOME/navcore-traces/$(date -u +%Y%m%d-%H%M%S)"
mkdir -p "$DIR"
ln -sfn "$DIR/navcore.log" "$HOME/navcore.log"
ln -sfn "$DIR" "$HOME/navcore-traces/latest"

{
    echo "build:   $(cat BUILD 2>/dev/null || echo unknown)"
    echo "binary:  $(stat -c '%y' target/release/navcore)"
    echo "kernel:  $(uname -r)"
    echo "mesa:    $(dpkg-query -W -f='${Version}' mesa-vulkan-drivers 2>/dev/null)"
    echo "started: $(date -u -Iseconds)"
    echo "throttled at start: $(vcgencmd get_throttled | cut -d= -f2)"
} >"$DIR/session.txt"

export WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR="/run/user/$(id -u)"
export RUST_LOG="${RUST_LOG:-info}" RUST_BACKTRACE=1

journalctl -k -f -n 0 -o short-iso-precise >"$DIR/kernel.log" 2>&1 &
JPID=$!
target/release/navcore >"$DIR/navcore.log" 2>&1 </dev/null &
NPID=$!
python3 deploy/pi_sampler.py "$NPID" "$DIR/system.csv" 2>>"$DIR/sampler.err"
wait "$NPID"; CODE=$?
sleep 2; kill "$JPID" 2>/dev/null

{
    echo "ended:   $(date -u -Iseconds)"
    echo "exit:    $CODE$([ $CODE -gt 128 ] && echo " (signal $((CODE - 128)))")"
    echo "throttled at end: $(vcgencmd get_throttled | cut -d= -f2)"
} >>"$DIR/session.txt"

# Keep the newest $KEEP sessions.
ls -1d "$HOME"/navcore-traces/2* 2>/dev/null | head -n -"$KEEP" | xargs -r rm -rf
