#!/usr/bin/env bash
# (Re)start manx on the Pi's own screen, traced (see pi-trace.sh). Runs on
# the Pi — over ssh from deploy-to-pi.sh, or from the desktop shortcut.
# Log: ~/manx.log → latest trace session.
set -euo pipefail
pkill -x manx 2>/dev/null && sleep 2 || true
setsid nohup bash "$HOME/manx/deploy/pi-trace.sh" >/dev/null 2>&1 </dev/null &
sleep 3
if pgrep -x manx >/dev/null; then
    echo "manx running on the Pi's screen (trace: ~/manx-traces/latest)"
else
    echo "manx exited — last lines of ~/manx.log:"; tail -20 "$HOME/manx.log"; exit 1
fi
