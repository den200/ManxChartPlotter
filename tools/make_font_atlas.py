#!/usr/bin/env python3
"""Bake a signed-distance-field glyph atlas for manx's chart labels.

    tools/make_font_atlas.py            # regenerate assets/fonts/labels.{png,json}

Chart text is drawn at a wide range of sizes — a sounding at 10 px and a place
name at 40 px in the same frame — and the size follows the S-52 body size, the
display DPI and the zoom, so it is not known ahead of time. A coverage bitmap
would have to be baked per size or resampled, and resampling ink turns strokes
to mush. An SDF stores the distance to the glyph outline instead, so one bake
stays sharp at every size: the shader recovers the edge with a smoothstep whose
width it derives per pixel.

Two weights are baked into one atlas. S-52's TX instruction carries a font
weight (CHARS[1], '6' = bold), and OpenCPN honours it — place names come out
bold, light descriptions and soundings do not.
"""

import json
import os
import sys

import numpy as np
from PIL import Image, ImageDraw, ImageFont
from scipy.ndimage import distance_transform_edt

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
FONTS = os.path.join(ROOT, "assets", "fonts")

# Atlas resolution per em. The SDF is scale-independent, so this only bounds how
# fine a detail survives — 48 px/em keeps counters open in 'e' and 'a'.
PX_PER_EM = 48
# Distance range the SDF encodes, in atlas pixels. Also the padding around each
# glyph, since the field has to exist outside the ink for the edge to be found.
SPREAD = 6
# Supersampling for the coverage bitmap the distance field is measured from.
# The glyph is rendered at PX_PER_EM * SS and the field downsampled afterwards,
# so the outline is located to a fraction of an atlas pixel.
SS = 4
ATLAS_W = 1024

# ASCII, then Latin-1 for the Nordic and Western European chart names that the
# old 5x7 table simply had no glyphs for — "Vallensbæk" and "Brøndby" rendered
# as "Vallensbak" and "Brondby".
CODEPOINTS = list(range(0x20, 0x7F)) + list(range(0xA0, 0x100)) + [
    0x2013,  # en dash
    0x2019,  # right single quote
    0x2026,  # ellipsis
]


def glyph_sdf(font, ch, ss_em):
    """Render one glyph and return (sdf_u8, plane_em, advance_em).

    `plane_em` is the quad to draw the glyph into, in em units relative to the
    pen position, y up from the baseline. It includes the SDF padding, so the
    quad covers the whole field and not just the ink.
    """
    advance = font.getlength(ch) / ss_em
    try:
        box = font.getbbox(ch, anchor="ls")
    except (ValueError, KeyError):
        return None, None, advance
    if box is None:
        return None, None, advance
    bx0, by0, bx1, by1 = box
    if bx1 <= bx0 or by1 <= by0:
        return None, None, advance  # whitespace: advance only

    pad_ss = SPREAD * SS
    w = int(np.ceil(bx1 - bx0)) + 2 * pad_ss
    h = int(np.ceil(by1 - by0)) + 2 * pad_ss
    img = Image.new("L", (w, h), 0)
    # The same anchor the box was measured with: 'ls' puts the origin on the
    # baseline, which is where the box coordinates are relative to.
    ImageDraw.Draw(img).text(
        (-bx0 + pad_ss, -by0 + pad_ss), ch, font=font, fill=255, anchor="ls"
    )
    ink = np.array(img) > 127

    if not ink.any():
        return None, None, advance

    # Signed distance in supersampled pixels: positive inside, negative outside.
    inside = distance_transform_edt(ink)
    outside = distance_transform_edt(~ink)
    signed = np.where(ink, inside - 0.5, -(outside - 0.5))

    # Downsample the *field*, not the coverage — averaging distances stays
    # meaningful where averaging ink does not.
    out_w, out_h = w // SS, h // SS
    signed = signed[: out_h * SS, : out_w * SS].reshape(out_h, SS, out_w, SS).mean(axis=(1, 3))

    # Encode to 0..255 with 0.5 at the outline. Distances are in supersampled
    # pixels; SPREAD * SS is the full range.
    norm = np.clip(signed / (SPREAD * SS) * 0.5 + 0.5, 0.0, 1.0)
    sdf = (norm * 255.0 + 0.5).astype(np.uint8)

    plane = [
        (bx0 - pad_ss) / ss_em,
        -(by1 + pad_ss) / ss_em,
        (bx1 + pad_ss) / ss_em,
        -(by0 - pad_ss) / ss_em,
    ]
    return sdf, plane, advance


def bake(path, ss_em):
    """Rasterise every codepoint of one weight."""
    font = ImageFont.truetype(path, ss_em)
    out = {}
    for cp in CODEPOINTS:
        ch = chr(cp)
        sdf, plane, advance = glyph_sdf(font, ch, ss_em)
        out[cp] = {"sdf": sdf, "plane": plane, "adv": advance}
    return out, font


def pack(weights):
    """Shelf-pack every glyph of every weight into one atlas."""
    boxes = []
    for name, glyphs in weights.items():
        for cp, g in glyphs.items():
            if g["sdf"] is not None:
                boxes.append((name, cp, g["sdf"].shape[1], g["sdf"].shape[0]))
    # Tallest first so shelves stay tight.
    boxes.sort(key=lambda b: -b[3])

    placed = {}
    x = y = shelf_h = 0
    for name, cp, w, h in boxes:
        if x + w > ATLAS_W:
            x = 0
            y += shelf_h
            shelf_h = 0
        placed[(name, cp)] = (x, y)
        x += w
        shelf_h = max(shelf_h, h)
    height = y + shelf_h

    # Round up to a multiple of 4 so the row pitch is friendly to wgpu's
    # 256-byte copy alignment on the width and avoids odd texture heights.
    height = (height + 3) & ~3
    atlas = np.zeros((height, ATLAS_W), np.uint8)
    for (name, cp), (px, py) in placed.items():
        sdf = weights[name][cp]["sdf"]
        atlas[py : py + sdf.shape[0], px : px + sdf.shape[1]] = sdf
    return atlas, placed


def main():
    ss_em = PX_PER_EM * SS
    weights = {}
    fonts = {}
    for name, filename in (("regular", "DejaVuSans.ttf"), ("bold", "DejaVuSans-Bold.ttf")):
        weights[name], fonts[name] = bake(os.path.join(FONTS, filename), ss_em)

    atlas, placed = pack(weights)
    png = os.path.join(FONTS, "labels.png")
    Image.fromarray(atlas, "L").save(png, optimize=True)

    meta = {
        "font": "DejaVu Sans",
        "px_per_em": PX_PER_EM,
        "spread_px": SPREAD,
        "atlas": [ATLAS_W, int(atlas.shape[0])],
        "glyphs": {},
    }
    # Cap height drives the pixel size the layout asks for, so it has to come
    # from the font rather than being guessed.
    ascent, descent = fonts["regular"].getmetrics()
    cap = fonts["regular"].getbbox("H", anchor="ls")
    meta["ascent"] = ascent / ss_em
    meta["descent"] = descent / ss_em
    meta["cap_height"] = -cap[1] / ss_em

    for name, glyphs in weights.items():
        entries = {}
        for cp, g in glyphs.items():
            e = {"adv": round(g["adv"], 5)}
            if g["sdf"] is not None:
                px, py = placed[(name, cp)]
                h, w = g["sdf"].shape
                e["uv"] = [px, py, px + w, py + h]
                e["plane"] = [round(v, 5) for v in g["plane"]]
            entries[str(cp)] = e
        meta["glyphs"][name] = entries

    with open(os.path.join(FONTS, "labels.json"), "w") as f:
        json.dump(meta, f, separators=(",", ":"), sort_keys=True)

    n = sum(1 for g in weights["regular"].values() if g["sdf"] is not None)
    print(
        f"{png}: {ATLAS_W}x{atlas.shape[0]} SDF atlas, {n} glyphs x 2 weights, "
        f"{PX_PER_EM} px/em, spread {SPREAD} px, cap height {meta['cap_height']:.3f} em"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
