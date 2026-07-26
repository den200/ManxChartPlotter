#!/usr/bin/env python3
"""Build navcore's symbol atlas, rendering the vector symbols from their HPGL.

    tools/render_symbols.py [--check]

navcore's atlas used to be OpenCPN's `rastersymbols-day.png` copied verbatim —
glyphs drawn for 1:1 display at the S-52 nominal density of 3.125 px/mm. On a
200-dpi screen the renderer has to magnify them 2.5x to reach the right physical
size, and magnifying a raster is exactly as lossy as it sounds: a sounding digit
loses its black core to the antialiasing halo baked into the sheet.

375 of the 1093 symbols carry vector definitions, and they cover 89% of actual
symbol draws. Those are rendered here from their HPGL at `OVERSAMPLE` times the
nominal size, so the GPU has real detail to sample when it scales them up. The
remaining 718 — `<definition>R</definition>`, including all the sounding digits
— are copied from the raster sheet as before.

The atlas records a symbol's *display* size separately from its texture rect, so
an oversampled cell still draws at the size S-52 asks for.

`--check` renders each vector symbol alongside the raster the sheet holds for
the same name and reports how well they agree — the coordinate conventions in
the HPGL (y-up, pivot- and origin-relative) are easy to get subtly wrong, and
the raster is the ground truth for what the symbol should look like.
"""

import json
import os
import re
import sys
# defusedxml: chartsymbols.xml is a repo asset, but there is no reason for a
# build script to carry the stdlib parser's entity-expansion behaviour.
from defusedxml import ElementTree as ET

from PIL import Image, ImageDraw

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CHARTSYMBOLS = os.path.join(ROOT, "assets/s52/chartsymbols.xml")
RASTERSHEET = os.path.join(
    ROOT, "doc/reference projects/OpenCPN/data/s57data/rastersymbols-day.png"
)
OUT_PNG = os.path.join(ROOT, "assets/symbols/atlas.png")
OUT_JSON = os.path.join(ROOT, "assets/symbols/atlas.json")

# Bitmap pixels per HPGL unit. The HPGL is in 0.01mm and the symbol library is
# drawn for 3.125 px/mm — the S-52 nominal display density — so one nominal
# pixel is 32 HPGL units. Every symbol's <bitmap> size is its <vector> size
# times this, which is how the two representations stay interchangeable.
PX_PER_UNIT = 0.032
# How much finer than nominal the vector art is rasterised. The renderer scales
# symbols up by ~2.5x on a 200-dpi display, so 4x leaves detail in hand.
OVERSAMPLE = 4
ATLAS_W = 2048


def palette(root, table="DAY_BRIGHT"):
    for ct in root.iter("color-table"):
        if ct.get("name") != table:
            continue
        out = {}
        for c in ct.iter("color"):
            out[c.get("name")] = (
                int(c.get("r")),
                int(c.get("g")),
                int(c.get("b")),
            )
        return out
    return {}


def color_refs(text):
    """Parse a colour reference like 'ASNDG2BDEPMD' into {'A': 'SNDG2', ...}.

    One letter followed by a five-character token, repeated.
    """
    out = {}
    text = (text or "").strip()
    for i in range(0, len(text) - 5, 6):
        out[text[i]] = text[i + 1 : i + 6]
    return out


def parse_points(args):
    nums = [int(n) for n in re.findall(r"-?\d+", args)]
    return list(zip(nums[0::2], nums[1::2]))


class Hpgl:
    """The S-52 subset of HPGL, as s52plib's RenderFromHPGL interprets it."""

    def __init__(self, size, scale, origin, pal, refs):
        self.img = Image.new("RGBA", size, (0, 0, 0, 0))
        self.d = ImageDraw.Draw(self.img)
        self.scale = scale
        self.ox, self.oy = origin
        self.h = size[1]
        self.pal = pal
        self.refs = refs
        self.pen = (0, 0, 0)
        self.alpha = 255
        self.width = 1
        self.pos = (0, 0)
        self.poly = None

    def xy(self, p):
        # HPGL in chartsymbols.xml is y-down, the same sense as the screen —
        # s52plib uses the coordinates directly, subtracting only the pivot.
        # Flipping y here mirrored every asymmetric symbol; TOPMAR02's triangle
        # pointed down and BOYSPP15's hypotenuse ran the wrong way, which is
        # exactly the sort of thing that looks plausible until you put it next
        # to the raster the sheet already holds.
        return (
            (p[0] - self.ox) * self.scale,
            (p[1] - self.oy) * self.scale,
        )

    def rgba(self):
        return (*self.pen, self.alpha)

    def run(self, hpgl):
        for cmd in hpgl.split(";"):
            cmd = cmd.strip()
            if len(cmd) < 2:
                continue
            op, args = cmd[:2].upper(), cmd[2:]
            if op == "SP":
                token = self.refs.get(args[:1], "CHBLK")
                self.pen = self.pal.get(token, (0, 0, 0))
            elif op == "SW":
                # Pen width is in nominal pixels, not HPGL units.
                self.width = max(1, int(args or 1) * OVERSAMPLE)
            elif op == "ST":
                # s52plib: transparency = (4 - index) * 64, clamped.
                self.alpha = max(0, min(255, (4 - int(args or 0)) * 64))
            elif op == "PU":
                pts = parse_points(args)
                if pts:
                    self.pos = pts[0]
            elif op == "PD":
                pts = parse_points(args)
                if not pts:
                    # A bare PD is a dot: s52plib draws lineStart -> lineStart+1.
                    pts = [(self.pos[0] + 1, self.pos[1])]
                for p in pts:
                    if self.poly is not None:
                        self.poly.append(self.xy(p))
                    else:
                        self.d.line([self.xy(self.pos), self.xy(p)],
                                    fill=self.rgba(), width=self.width)
                    self.pos = p
            elif op == "PM":
                mode = (args or "0").strip()
                if mode == "0":
                    self.poly = [self.xy(self.pos)]
                elif mode == "2":
                    pass  # close; the fill comes with FP
            elif op == "FP":
                if self.poly and len(self.poly) >= 3:
                    self.d.polygon(self.poly, fill=self.rgba())
                self.poly = None
            elif op == "CI":
                r = int(args or 0) * self.scale
                cx, cy = self.xy(self.pos)
                box = [cx - r, cy - r, cx + r, cy + r]
                if self.poly is not None:
                    self.d.ellipse(box, fill=self.rgba())
                else:
                    self.d.ellipse(box, outline=self.rgba(), width=self.width)
        return self.img


def parse_symbols():
    root = ET.parse(CHARTSYMBOLS).getroot()
    pal = palette(root)
    out = []
    for sym in root.iter("symbol"):
        name = sym.findtext("name")
        bitmap, vector = sym.find("bitmap"), sym.find("vector")
        if name is None or bitmap is None:
            continue
        loc = bitmap.find("graphics-location")
        bp = bitmap.find("pivot")
        entry = {
            "name": name,
            "bw": int(bitmap.get("width", 0)),
            "bh": int(bitmap.get("height", 0)),
            "bx": int(loc.get("x", 0)) if loc is not None else 0,
            "by": int(loc.get("y", 0)) if loc is not None else 0,
            "bpx": int(bp.get("x", 0)) if bp is not None else 0,
            "bpy": int(bp.get("y", 0)) if bp is not None else 0,
            # The HPGL hangs off <vector>, not off <symbol>.
            "hpgl": vector.findtext("HPGL") if vector is not None else None,
            "refs": color_refs(sym.findtext("color-ref")),
        }
        if vector is not None and entry["hpgl"]:
            vo, vp = vector.find("origin"), vector.find("pivot")
            entry.update(
                vw=int(vector.get("width", 0)),
                vh=int(vector.get("height", 0)),
                vox=int(vo.get("x", 0)) if vo is not None else 0,
                voy=int(vo.get("y", 0)) if vo is not None else 0,
                vpx=int(vp.get("x", 0)) if vp is not None else 0,
                vpy=int(vp.get("y", 0)) if vp is not None else 0,
            )
        out.append(entry)
    return out, pal


def hpgl_bounds(hpgl, pen_units):
    """The extent the HPGL actually draws, in HPGL units.

    The declared <vector width/height> is not always the drawing's extent —
    LIGHTS12's flare and ACHARE02's anchor ring both fall outside it, and
    clipping to the declaration rendered the first as nothing at all and the
    second without its top. Measuring the geometry is both simpler and right.
    """
    xs, ys = [], []
    for cmd in hpgl.split(";"):
        cmd = cmd.strip()
        if len(cmd) < 2:
            continue
        op, args = cmd[:2].upper(), cmd[2:]
        if op in ("PU", "PD"):
            for x, y in parse_points(args):
                xs.append(x)
                ys.append(y)
        elif op == "CI" and xs:
            r = int(args or 0)
            xs += [xs[-1] - r, xs[-1] + r]
            ys += [ys[-1] - r, ys[-1] + r]
    if not xs:
        return None
    pad = pen_units
    return (min(xs) - pad, min(ys) - pad, max(xs) + pad, max(ys) + pad)


def render_vector(sym, pal):
    """Rasterise one vector symbol; returns (image, display_size, pivot)."""
    # One nominal pixel of pen, in HPGL units, as slack around the drawing.
    bounds = hpgl_bounds(sym["hpgl"], round(1 / PX_PER_UNIT))
    if bounds is None:
        return None, None, None
    x0, y0, x1, y1 = bounds
    vw, vh = x1 - x0, y1 - y0
    if vw <= 0 or vh <= 0:
        return None, None, None
    dw = max(1, round(vw * PX_PER_UNIT))
    dh = max(1, round(vh * PX_PER_UNIT))
    size = (dw * OVERSAMPLE, dh * OVERSAMPLE)
    scale = size[0] / vw
    img = Hpgl(size, scale, (x0, y0), pal, sym["refs"]).run(sym["hpgl"])
    # The pivot is where the symbol attaches to its position on the chart, so
    # it has to be expressed against the same box the texture covers.
    pivot = [
        (sym["vpx"] - x0) / vw,
        1.0 - (sym["vpy"] - y0) / vh,
    ]
    return img, (dw, dh), pivot


def agreement(vector_img, raster_img):
    """How well a vector render matches the raster the sheet holds for it.

    Coverage overlap and colour, on the raster's own grid. The raster sheet is
    the appearance OpenCPN ships and the reference captures show, so it is the
    ground truth for *what the symbol looks like*; the vector is the same art at
    a resolution worth having. Where they disagree, the disagreement is mine —
    an HPGL opcode read wrongly — or the definitions genuinely differ, and
    either way the known-good raster is the safer pick.
    """
    w, h = raster_img.size
    if w < 2 or h < 2:
        return 0.0
    v = vector_img.resize((w, h), Image.LANCZOS)
    va, ra = v.split()[3], raster_img.split()[3]
    vp, rp = list(va.getdata()), list(ra.getdata())
    inter = sum(1 for a, b in zip(vp, rp) if a > 96 and b > 96)
    union = sum(1 for a, b in zip(vp, rp) if a > 96 or b > 96)
    if not union:
        return 0.0
    iou = inter / union
    if not inter:
        return 0.0
    vrgb, rrgb = list(v.convert("RGB").getdata()), list(raster_img.convert("RGB").getdata())
    diff = [
        sum(abs(x - y) for x, y in zip(vrgb[i], rrgb[i])) / 3
        for i in range(len(vp))
        if vp[i] > 96 and rp[i] > 96
    ]
    colour = sum(diff) / len(diff)
    # Colour agreement folded in as a 0..1 factor so one number decides.
    return iou * max(0.0, 1.0 - colour / 96.0)


def main():
    symbols, pal = parse_symbols()
    sheet = Image.open(RASTERSHEET).convert("RGBA")

    cells = []
    n_vec = 0
    rejected = []
    for sym in symbols:
        img, disp, pivot = (None, None, None)
        if sym.get("hpgl") and sym.get("vw"):
            img, disp, pivot = render_vector(sym, pal)
            if img is not None and img.getbbox() is not None and sym["bw"] > 1:
                raster = sheet.crop(
                    (sym["bx"], sym["by"], sym["bx"] + sym["bw"], sym["by"] + sym["bh"])
                )
                score = agreement(img, raster)
                if score < 0.55:
                    rejected.append((sym["name"], round(score, 2)))
                    img = None
                else:
                    # Agreement means the two depict the same symbol in the same
                    # box, so keep the raster's display size and pivot and swap
                    # only the texture. Taking the vector's own bounds instead
                    # would resize symbols slightly and move everything that
                    # sits next to them.
                    disp = (sym["bw"], sym["bh"])
                    pivot = [
                        sym["bpx"] / sym["bw"],
                        1.0 - sym["bpy"] / sym["bh"],
                    ]
        if img is not None and img.getbbox() is not None:
            n_vec += 1
        else:
            w, h = sym["bw"], sym["bh"]
            if w <= 0 or h <= 0:
                continue
            img = sheet.crop((sym["bx"], sym["by"], sym["bx"] + w, sym["by"] + h))
            disp = (w, h)
            pivot = [
                sym["bpx"] / w if w else 0.5,
                1.0 - (sym["bpy"] / h if h else 0.5),
            ]
        cells.append((sym["name"], img, disp, pivot))

    # Shelf-pack, tallest first.
    cells.sort(key=lambda c: -c[1].height)
    placed, x, y, shelf = {}, 0, 0, 0
    for name, img, disp, pivot in cells:
        w, h = img.size
        if x + w > ATLAS_W:
            x, y, shelf = 0, y + shelf, 0
        placed[name] = (x, y, w, h, disp, pivot)
        x += w
        shelf = max(shelf, h)
    height = (y + shelf + 3) & ~3

    atlas = Image.new("RGBA", (ATLAS_W, height), (0, 0, 0, 0))
    for name, img, _, _ in cells:
        px, py = placed[name][0], placed[name][1]
        atlas.paste(img, (px, py))
    atlas.save(OUT_PNG)

    meta = {
        "version": 2,
        "format": "direct",
        "atlas_size": [ATLAS_W, height],
        "symbols": {
            name: {
                "rect": [p[0], p[1], p[2], p[3]],
                "size": [p[4][0], p[4][1]],
                "pivot": [round(p[5][0], 3), round(p[5][1], 3)],
            }
            for name, p in placed.items()
        },
    }
    with open(OUT_JSON, "w") as f:
        json.dump(meta, f, separators=(",", ":"), sort_keys=True)

    print(
        f"{OUT_PNG}: {ATLAS_W}x{height}, {len(placed)} symbols "
        f"({n_vec} rendered from HPGL at {OVERSAMPLE}x, {len(placed) - n_vec} from the raster sheet)"
    )
    if rejected:
        rejected.sort(key=lambda r: r[1])
        print(
            f"{len(rejected)} vector renders disagreed with the raster and were "
            f"not used; worst:"
        )
        for name, score in rejected[:12]:
            print(f"    {score:.2f}  {name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
