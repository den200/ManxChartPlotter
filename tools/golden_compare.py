#!/usr/bin/env python3
"""Compare a capture with its golden reference.

    tools/golden_compare.py <reference.png> <capture.png> <diff.png>

A pixel counts as changed when any channel differs by more than CHANNEL_TOL;
the capture fails when more than MAX_CHANGED of its pixels changed. Captures
are deterministic on one machine (the same view twice is identical to the
pixel), so the tolerance is not there to absorb noise in Manx: it absorbs a
GPU driver or OS update shifting antialiasing by a level, and nothing larger.
A real change to the chart — a symbol, a colour, a line style, a label —
moves thousands of pixels by far more.

On failure, <diff.png> shows the capture greyed out with every changed pixel
in magenta, so the reader sees where the picture moved, not just that it did.
Exit 0 on a match, 1 on a change, 2 when the two cannot be compared.
"""
import sys

from PIL import Image, ImageChops

CHANNEL_TOL = 4
MAX_CHANGED = 0.0005  # 0.05 % of the pixels: ~800 of a 1400x1100 capture


def main():
    ref_path, cap_path, diff_path = sys.argv[1:4]
    ref = Image.open(ref_path).convert("RGB")
    cap = Image.open(cap_path).convert("RGB")
    if ref.size != cap.size:
        print(f"size {cap.size} differs from the reference {ref.size}")
        return 2
    diff = ImageChops.difference(ref, cap)
    # The largest channel difference per pixel, then a mask of the ones over
    # the tolerance.
    r, g, b = diff.split()
    worst = ImageChops.lighter(ImageChops.lighter(r, g), b)
    mask = worst.point(lambda v: 255 if v > CHANNEL_TOL else 0)
    changed = mask.histogram()[255]
    total = ref.size[0] * ref.size[1]
    share = changed / total
    if share <= MAX_CHANGED:
        print(f"ok ({changed} pixels changed, {100 * share:.3f} %)")
        return 0
    grey = cap.convert("L").convert("RGB")
    grey = Image.blend(grey, Image.new("RGB", cap.size, (255, 255, 255)), 0.6)
    grey.paste((255, 0, 255), mask=mask)
    grey.save(diff_path)
    print(f"CHANGED: {changed} pixels ({100 * share:.2f} %), bounds {mask.getbbox()}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
