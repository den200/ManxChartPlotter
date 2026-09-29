#!/usr/bin/env python3
"""Pack Natural Earth's land polygons into manx's world basemap.

    curl -LO https://raw.githubusercontent.com/nvkelso/natural-earth-vector/master/geojson/ne_50m_land.geojson
    python3 tools/basemap/make_basemap.py ne_50m_land.geojson assets/basemap/land.bin

Natural Earth is public domain (naturalearthdata.com). The 1:50m set is
drawn for maps at about 1:50 000 000, which is what the basemap stands in
for: somewhere to pan across between detailed charts.

Format, little-endian:
    b"NCLAND1\\0"
    u32 polygon count
    per polygon: u32 ring count (the first is the outline, the rest holes)
    per ring:    u32 point count, then that many (i32 lon, i32 lat) in 1e-7°
Rings are not closed (the last point is not the first again). Latitudes are
held inside ±85.05°, the edge of the Mercator map.
"""
import json
import struct
import sys

MAX_LAT = 85.05


def main(src, dst):
    data = json.load(open(src))
    polygons = []
    for feature in data["features"]:
        g = feature["geometry"]
        polys = g["coordinates"] if g["type"] == "MultiPolygon" else [g["coordinates"]]
        polygons.extend(polys)
    out = bytearray(b"NCLAND1\0")
    out += struct.pack("<I", len(polygons))
    for poly in polygons:
        out += struct.pack("<I", len(poly))
        for ring in poly:
            pts = []
            for lon, lat in ring:
                lat = max(-MAX_LAT, min(MAX_LAT, lat))
                p = (round(lon * 1e7), round(lat * 1e7))
                if not pts or pts[-1] != p:
                    pts.append(p)
            if len(pts) > 1 and pts[0] == pts[-1]:
                pts.pop()
            out += struct.pack("<I", len(pts))
            for p in pts:
                out += struct.pack("<ii", *p)
    open(dst, "wb").write(out)
    print(f"{len(polygons)} polygons, {len(out)} bytes -> {dst}")


if __name__ == "__main__":
    main(*sys.argv[1:3])
