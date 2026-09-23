#!/usr/bin/env bash
# Copy the source to the Pi and build it there. Run on the Mac:
#   deploy/deploy-to-pi.sh pi@navcore.local
# The Pi keeps its own licence fingerprint (license/) and its own charts —
# neither is copied: an o-charts licence belongs to one machine.
set -euo pipefail
HOST="${1:?usage: deploy/deploy-to-pi.sh user@host}"
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
    echo "First deployment: setting the Pi up (this asks for the Pi's sudo password)…"
    ssh -t "$HOST" 'bash ~/navcore/deploy/pi-setup.sh'
fi
ssh "$HOST" 'source ~/.cargo/env && cd ~/navcore && cargo build --release 2>&1 | tail -3'
echo "Built. Start it on the Pi's screen, or reboot to autostart."
