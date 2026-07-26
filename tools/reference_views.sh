#!/bin/sh
# Capture the four OpenCPN reference views with navcore, pixel-for-pixel.
#
# The view parameters below were solved from the reference screenshots themselves
# (chart-outline rectangles + coastline cross-correlation), not from the lat/lon in
# the OpenCPN status bar — that field shows the *cursor* position, not the view centre.
#
# NAVCORE_VIEW = "centre_lat,centre_lon,mpp"  (mpp = Mercator metres per pixel)
# NAVCORE_SIZE = "WxH" physical pixels, matching the reference screenshot canvas
#                (canvas = screenshot minus the OpenCPN status bar)
#
# Usage: tools/reference_views.sh [chart_dir] [out_dir]

set -e
CHARTS="${1:-charts/oeuSENC-DK-2025-1-20-base-macbook/}"
OUT="${2:-doc/reference-views}"
mkdir -p "$OUT"

shot() { # name view size
    echo "=> $1"
    NAVCORE_UI=0 NAVCORE_VIEW="$2" NAVCORE_SIZE="$3" NAVCORE_SHOT="$OUT/$1.png" \
        cargo run --release -- "$CHARTS" >/dev/null 2>&1
}

# 1: Kalvebodbroen / Kalvebod S leading lights, OpenCPN "OverZoom"
shot view1-kalvebod  55.614011,12.510879,0.5423  2000x1334
# 2: Brondby Lystbadehavn, OpenCPN 1:7800
shot view2-brondby   55.609799,12.434845,1.8577  2000x1281
# 3: Mosede Havn, OpenCPN 1:2600
shot view3-mosede    55.566164,12.286074,0.6014  2000x1281
# 4: Nordre Rose / Scanport Havn, OpenCPN 1:11600
shot view4-nordrerose 55.632918,12.674552,2.7608 2000x1281

echo "wrote captures to $OUT"
