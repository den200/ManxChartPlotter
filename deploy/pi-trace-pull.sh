#!/usr/bin/env bash
# Copy the Pi's trace sessions to traces/pi/ and summarise one. Run on the Mac:
#   deploy/pi-trace-pull.sh                  # latest session
#   deploy/pi-trace-pull.sh 20260923-130501  # a given one
#   HOST=pi@manx.local deploy/pi-trace-pull.sh
set -euo pipefail
HOST="${HOST:-rpi5}"
cd "$(dirname "$0")/.."
mkdir -p traces/pi
rsync -az --exclude latest "$HOST:manx-traces/" traces/pi/
SESSION="${1:-$(ls -1 traces/pi | grep -E '^[0-9]{8}-' | tail -1)}"
ln -sfn "$SESSION" traces/pi/latest
python3 tools/pi_trace_report.py "traces/pi/$SESSION"
