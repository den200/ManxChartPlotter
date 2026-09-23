#!/usr/bin/env bash
# (Re)start navcore on the Pi's own screen, traced (see pi-trace.sh). Runs on
# the Pi — over ssh from deploy-to-pi.sh, or from the desktop shortcut.
# Log: ~/navcore.log → latest trace session.
set -euo pipefail
pkill -x navcore 2>/dev/null && sleep 2 || true
setsid nohup bash "$HOME/navcore/deploy/pi-trace.sh" >/dev/null 2>&1 </dev/null &
sleep 3
if pgrep -x navcore >/dev/null; then
    echo "navcore running on the Pi's screen (trace: ~/navcore-traces/latest)"
else
    echo "navcore exited — last lines of ~/navcore.log:"; tail -20 "$HOME/navcore.log"; exit 1
fi
