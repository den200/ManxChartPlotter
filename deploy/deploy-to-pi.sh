#!/usr/bin/env bash
# Copy the source to the Pi, build it there and restart manx on its screen.
# Run on the Mac:
#   deploy/deploy-to-pi.sh              # the rpi5 test rig (ssh alias)
#   deploy/deploy-to-pi.sh pi@manx.local
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
    ./ "$HOST:manx/"
# The PNG exclusion above is for scratch captures; the assets need theirs.
rsync -az assets/ "$HOST:manx/assets/"
# Which source this is — the trace sessions record it (pi-trace.sh).
echo "$(git describe --always --dirty) $(git branch --show-current) deployed $(date -u -Iseconds)" |
    ssh "$HOST" 'cat > ~/manx/BUILD'
# Double-click launcher on the Pi's desktop. quick_exec stops the file
# manager asking "execute or open?" on every double-click.
ssh "$HOST" 'mkdir -p ~/Desktop ~/.config/libfm &&
    install -m 755 ~/manx/deploy/manx-test.desktop ~/Desktop/manx-test.desktop &&
    { [ -f ~/.config/libfm/libfm.conf ] || cp /etc/xdg/libfm/libfm.conf ~/.config/libfm/; } &&
    { grep -q "^quick_exec=" ~/.config/libfm/libfm.conf || sed -i "/^\[config\]/a quick_exec=1" ~/.config/libfm/libfm.conf; }'

# First time: the Pi has no Rust yet, so set it up (packages, Rust, autostart).
if ! ssh "$HOST" 'test -x ~/.cargo/bin/cargo'; then
    echo "First deployment: setting the Pi up (this may ask for the Pi's sudo password)…"
    ssh -t "$HOST" 'bash ~/manx/deploy/pi-setup.sh'
fi
ssh "$HOST" 'source ~/.cargo/env && cd ~/manx &&
    if out=$(cargo build --release 2>&1); then echo "$out" | tail -1; else echo "$out" | tail -40; exit 1; fi'
if [ -z "${NORUN:-}" ]; then
    ssh "$HOST" 'bash ~/manx/deploy/pi-run.sh'
fi
