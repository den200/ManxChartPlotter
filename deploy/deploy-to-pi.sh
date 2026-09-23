#!/usr/bin/env bash
# Copy the source to the Pi, build it there and restart navcore on its screen.
# Run on the Mac:
#   deploy/deploy-to-pi.sh              # the rpi5 test rig (ssh alias)
#   deploy/deploy-to-pi.sh pi@navcore.local
#   NORUN=1 deploy/deploy-to-pi.sh      # build only, don't restart the app
# The Pi keeps its own licence fingerprint (license/) and its own charts —
# neither is copied: an o-charts licence belongs to one machine.
set -euo pipefail
HOST="${1:-rpi5}"
cd "$(dirname "$0")/.."

rsync -az --delete \
    --exclude target/ --exclude charts/ --exclude license/ \
    --exclude .git/ --exclude 'doc/reference projects/' --exclude doc/openCPN/ \
    --exclude .claude/ --exclude '*.png' \
    ./ "$HOST:navcore/"
# The PNG exclusion above is for scratch captures; the assets need theirs.
rsync -az assets/ "$HOST:navcore/assets/"

# First time: the Pi has no Rust yet, so set it up (packages, Rust, autostart).
if ! ssh "$HOST" 'test -x ~/.cargo/bin/cargo'; then
    echo "First deployment: setting the Pi up (this may ask for the Pi's sudo password)…"
    ssh -t "$HOST" 'bash ~/navcore/deploy/pi-setup.sh'
fi
ssh "$HOST" 'source ~/.cargo/env && cd ~/navcore &&
    if out=$(cargo build --release 2>&1); then echo "$out" | tail -1; else echo "$out" | tail -40; exit 1; fi'
if [ -z "${NORUN:-}" ]; then
    ssh "$HOST" 'bash ~/navcore/deploy/pi-run.sh'
fi
