#!/usr/bin/env python3
"""
Render S-52 area patterns from HPGL commands to PNG tiles.

This script:
1. Parses chartsymbols.xml to extract pattern definitions
2. Renders HPGL commands to PNG tiles
3. Creates a pattern atlas (patterns.png + patterns.json)

Usage:
    python3 tools/render_patterns.py

HPGL Commands supported:
- SP (Select Pen/Color)
- SW (Select Width)
- PU (Pen Up - move to)
- PD (Pen Down - draw line to)
- CI (Circle)
- PM (Polygon Mode)
- FP (Fill Polygon)
"""

import xml.etree.ElementTree as ET
import json
import os
import re
from pathlib import Path
from PIL import Image, ImageDraw

# Paths relative to project root
CHARTSYMBOLS_PATH = "assets/s52/chartsymbols.xml"
OUTPUT_ATLAS_PNG = "assets/patterns/atlas.png"
OUTPUT_ATLAS_JSON = "assets/patterns/atlas.json"

# S-52 color palette (Day_Bright scheme) - subset for patterns
# Color tokens from chartsymbols.xml color-ref
S52_COLORS = {
    # Land/Shore
    "LANDF": (201, 185, 122),    # Land fill
    "ALANDF": (201, 185, 122),   # LANDA fill - same as land
    "CHGRD": (156, 142, 106),    # Chart ground
    "ACHGRD": (156, 142, 106),
    "CHGRF": (156, 142, 106),    # Chart ground fill

    # Water/Depth
    "DEPVS": (129, 194, 236),    # Very shallow
    "DEPDW": (255, 255, 255),    # Deep water
    "DEPMD": (201, 226, 245),    # Medium depth
    "DEPMS": (164, 210, 241),    # Medium shallow

    # Features
    "CHMGD": (136, 96, 81),      # Chart magenta dark
    "CHMGF": (214, 163, 185),    # Chart magenta fill
    "CHBRN": (136, 96, 81),      # Chart brown
    "DNGHL": (255, 0, 0),        # Danger highlight (red)
    "TRFCD": (214, 163, 185),    # Traffic separation (magenta)
    "RESBL": (200, 200, 255),    # Restricted blue
    "RADHI": (0, 255, 0),        # Radar high

    # Special
    "NODTA": (178, 178, 178),    # No data
    "CHBLK": (0, 0, 0),          # Black
    "CHRED": (255, 0, 0),        # Red
    "CHYLW": (255, 255, 0),      # Yellow
    "CHGRN": (0, 200, 0),        # Green

    # Default fallback
    "DEFAULT": (128, 128, 128),
}

# Color reference mapping: single letter in HPGL -> color token
# Format in chartsymbols.xml: "A" -> lookup in color-ref attribute
# The color-ref contains comma-separated letter:TOKEN pairs
def parse_color_ref(color_ref_str: str) -> dict:
    """Parse color reference string like 'ALANDF' or 'A:CHBLK,B:CHRED'."""
    result = {}
    if ':' in color_ref_str:
        # New format: A:TOKEN,B:TOKEN
        for pair in color_ref_str.split(','):
            if ':' in pair:
                letter, token = pair.split(':')
                result[letter.strip()] = token.strip()
    else:
        # Old format: single token applies to 'A'
        result['A'] = color_ref_str
    return result


class HPGLRenderer:
    """Simple HPGL renderer for S-52 patterns."""

    def __init__(self, width: int, height: int, scale: float = 1.0):
        """
        Initialize renderer.

        Args:
            width: Canvas width in pixels
            height: Canvas height in pixels
            scale: Scale factor from HPGL units to pixels
        """
        self.width = width
        self.height = height
        self.scale = scale

        # RGBA image with transparent background
        self.image = Image.new('RGBA', (width, height), (0, 0, 0, 0))
        self.draw = ImageDraw.Draw(self.image)

        # Current state
        self.pen_color = (0, 0, 0, 255)
        self.pen_width = 1
        self.pos = (0, 0)
        self.polygon_points = []

        # Offset for centering
        self.offset_x = 0
        self.offset_y = 0

    def set_offset(self, ox: int, oy: int):
        """Set offset from HPGL origin to canvas origin."""
        self.offset_x = ox
        self.offset_y = oy

    def _transform(self, x: int, y: int) -> tuple:
        """Transform HPGL coordinates to canvas coordinates."""
        # Apply offset and scale
        px = int((x - self.offset_x) * self.scale)
        py = int((y - self.offset_y) * self.scale)
        # Flip Y (HPGL is Y-up, image is Y-down)
        py = self.height - 1 - py
        return (px, py)

    def set_color(self, color_letter: str, color_refs: dict):
        """Set pen color from color letter and reference mapping."""
        token = color_refs.get(color_letter, color_refs.get('A', 'DEFAULT'))
        rgb = S52_COLORS.get(token, S52_COLORS['DEFAULT'])
        self.pen_color = (*rgb, 255)

    def set_width(self, width: int):
        """Set pen width."""
        self.pen_width = max(1, int(width * self.scale))

    def pen_up(self, x: int, y: int):
        """Move pen without drawing."""
        self.pos = (x, y)

    def pen_down(self, x: int, y: int):
        """Draw line from current position to (x, y)."""
        start = self._transform(*self.pos)
        end = self._transform(x, y)
        self.draw.line([start, end], fill=self.pen_color, width=self.pen_width)
        self.pos = (x, y)

    def circle(self, radius: int, filled: bool = False):
        """Draw circle at current position."""
        center = self._transform(*self.pos)
        r = int(radius * self.scale)
        bbox = [center[0] - r, center[1] - r, center[0] + r, center[1] + r]
        if filled:
            self.draw.ellipse(bbox, fill=self.pen_color, outline=self.pen_color)
        else:
            self.draw.ellipse(bbox, outline=self.pen_color, width=self.pen_width)

    def start_polygon(self):
        """Start polygon mode."""
        self.polygon_points = [self._transform(*self.pos)]

    def add_polygon_point(self, x: int, y: int):
        """Add point to current polygon."""
        self.polygon_points.append(self._transform(x, y))
        self.pos = (x, y)

    def fill_polygon(self):
        """Fill and close current polygon."""
        if len(self.polygon_points) >= 3:
            self.draw.polygon(self.polygon_points, fill=self.pen_color, outline=self.pen_color)
        self.polygon_points = []

    def render_hpgl(self, hpgl: str, color_refs: dict):
        """
        Render HPGL command string.

        Args:
            hpgl: HPGL command string (semicolon-separated)
            color_refs: Color reference mapping {letter: token}
        """
        # Parse commands (semicolon-separated)
        commands = [cmd.strip() for cmd in hpgl.split(';') if cmd.strip()]

        in_polygon = False

        for cmd in commands:
            if len(cmd) < 2:
                continue

            op = cmd[:2].upper()
            args = cmd[2:]

            if op == 'SP':
                # Select Pen (color)
                if args:
                    self.set_color(args[0], color_refs)

            elif op == 'SW':
                # Select Width
                if args:
                    try:
                        self.set_width(int(args))
                    except ValueError:
                        pass

            elif op == 'PU':
                # Pen Up (move to)
                coords = self._parse_coords(args)
                if coords:
                    self.pen_up(*coords[0])

            elif op == 'PD':
                # Pen Down (draw to)
                coords = self._parse_coords(args)
                if coords:
                    for x, y in coords:
                        if in_polygon:
                            self.add_polygon_point(x, y)
                        else:
                            self.pen_down(x, y)
                elif not in_polygon:
                    # Bare `PD;` puts the pen down where it already is: a dot.
                    # Skipping it rasterised DRGARE01 — whose whole definition is
                    # `PU1500,1300;PD;PU1700,1500;PD;` — as an empty tile, so
                    # every dredged area lost its stipple.
                    self.pen_down(*self.pos)

            elif op == 'CI':
                # Circle
                if args:
                    try:
                        radius = int(args)
                        self.circle(radius, filled=in_polygon)
                    except ValueError:
                        pass

            elif op == 'PM':
                # Polygon Mode
                if args == '0':
                    in_polygon = True
                    self.start_polygon()
                elif args == '2':
                    in_polygon = False

            elif op == 'FP':
                # Fill Polygon
                self.fill_polygon()
                in_polygon = False

    def _parse_coords(self, args: str) -> list:
        """Parse coordinate pairs from HPGL argument string."""
        if not args:
            return []

        coords = []
        # Split on commas, pair up x,y values
        parts = args.split(',')
        for i in range(0, len(parts) - 1, 2):
            try:
                x = int(parts[i])
                y = int(parts[i + 1])
                coords.append((x, y))
            except ValueError:
                pass
        return coords

    def get_image(self) -> Image.Image:
        """Return the rendered image."""
        return self.image


def parse_patterns(xml_path: str) -> list:
    """Parse chartsymbols.xml and extract pattern definitions."""
    tree = ET.parse(xml_path)
    root = tree.getroot()

    patterns = []

    for pattern in root.iter('pattern'):
        name_elem = pattern.find('name')
        definition_elem = pattern.find('definition')
        filltype_elem = pattern.find('filltype')
        spacing_elem = pattern.find('spacing')

        if name_elem is None or definition_elem is None:
            continue

        name = name_elem.text
        if name is None:
            continue

        # Get definition type: V=vector, R=raster
        def_type = definition_elem.text or 'V'

        # Get fill type: S=staggered, L=linear
        fill_type = filltype_elem.text if filltype_elem is not None else 'L'

        # Get color reference (outside vector element)
        color_ref_elem = pattern.find('color-ref')
        color_ref = color_ref_elem.text if color_ref_elem is not None and color_ref_elem.text else ""

        # Get vector definition
        vector_elem = pattern.find('vector')
        hpgl = None
        width = 0
        height = 0
        pivot_x = 0
        pivot_y = 0
        origin_x = 0
        origin_y = 0

        # Get HPGL commands (outside vector element)
        hpgl_elem = pattern.find('HPGL')
        hpgl = hpgl_elem.text if hpgl_elem is not None and hpgl_elem.text else None

        # Get distance (min/max) values
        min_dist = 0
        max_dist = 0

        if vector_elem is not None:
            width = int(vector_elem.get('width', 0))
            height = int(vector_elem.get('height', 0))

            # Get pivot
            pivot_elem = vector_elem.find('pivot')
            if pivot_elem is not None:
                pivot_x = int(pivot_elem.get('x', 0))
                pivot_y = int(pivot_elem.get('y', 0))

            # Get origin
            origin_elem = vector_elem.find('origin')
            if origin_elem is not None:
                origin_x = int(origin_elem.get('x', 0))
                origin_y = int(origin_elem.get('y', 0))

            # Get distance
            dist_elem = vector_elem.find('distance')
            if dist_elem is not None:
                min_dist = int(dist_elem.get('min', 0))
                max_dist = int(dist_elem.get('max', 0))

        if hpgl is None or width == 0 or height == 0:
            continue

        patterns.append({
            'name': name,
            'type': def_type,
            'fill_type': fill_type,
            'width': width,
            'height': height,
            'pivot_x': pivot_x,
            'pivot_y': pivot_y,
            'origin_x': origin_x,
            'origin_y': origin_y,
            'color_ref': color_ref,
            'hpgl': hpgl,
            'min_dist': min_dist,
            'max_dist': max_dist,
        })

    return patterns


def render_pattern(pattern: dict, scale: float = 0.1) -> Image.Image:
    """
    Render a pattern to an image.

    Args:
        pattern: Pattern definition dict
        scale: Scale from HPGL units to pixels (100 HPGL units = 1mm)
               At 96 DPI, 1mm = ~3.78 pixels, so scale ~0.0378
               We use 0.1 for clearer patterns (~10x)

    Returns:
        RGBA Image of the pattern tile
    """
    # Calculate canvas size from HPGL dimensions
    # Add some padding for lines that extend to edges
    padding = 2
    canvas_w = int(pattern['width'] * scale) + padding * 2
    canvas_h = int(pattern['height'] * scale) + padding * 2

    # Minimum size
    canvas_w = max(canvas_w, 4)
    canvas_h = max(canvas_h, 4)

    renderer = HPGLRenderer(canvas_w, canvas_h, scale)

    # Set offset: HPGL origin is at pattern origin
    renderer.set_offset(pattern['origin_x'] - padding, pattern['origin_y'] - padding)

    # Parse color reference
    color_refs = parse_color_ref(pattern['color_ref'])

    # Render HPGL
    renderer.render_hpgl(pattern['hpgl'], color_refs)

    return renderer.get_image()


def pack_atlas(patterns: list, images: list, padding: int = 2) -> tuple:
    """
    Pack pattern images into an atlas.

    Returns:
        (atlas_image, atlas_metadata)
    """
    # Simple horizontal packing (patterns are small)
    # Calculate total width and max height
    total_width = sum(img.width + padding for img in images) + padding
    max_height = max(img.height for img in images) + padding * 2

    # Round up to power of 2
    def next_power_of_2(x):
        p = 1
        while p < x:
            p *= 2
        return p

    atlas_w = next_power_of_2(total_width)
    atlas_h = next_power_of_2(max_height)

    # Create atlas image
    atlas = Image.new('RGBA', (atlas_w, atlas_h), (0, 0, 0, 0))

    # Place patterns
    metadata = {
        'version': 1,
        'atlas_size': [atlas_w, atlas_h],
        'patterns': {}
    }

    x = padding
    for pattern, img in zip(patterns, images):
        y = padding
        atlas.paste(img, (x, y))

        metadata['patterns'][pattern['name']] = {
            'rect': [x, y, img.width, img.height],
            'tile_size': [pattern['width'], pattern['height']],
            'pivot': [pattern['pivot_x'], pattern['pivot_y']],
            'origin': [pattern['origin_x'], pattern['origin_y']],
            'fill_type': pattern['fill_type'],
            'min_dist': pattern['min_dist'],
            'max_dist': pattern['max_dist'],
        }

        x += img.width + padding

    return atlas, metadata


def main():
    # Change to project root
    script_dir = Path(__file__).parent
    project_root = script_dir.parent
    os.chdir(project_root)

    print(f"Working directory: {os.getcwd()}")

    # Check input exists
    if not os.path.exists(CHARTSYMBOLS_PATH):
        print(f"ERROR: chartsymbols.xml not found at {CHARTSYMBOLS_PATH}")
        return 1

    # Parse patterns
    print(f"Parsing {CHARTSYMBOLS_PATH}...")
    patterns = parse_patterns(CHARTSYMBOLS_PATH)
    print(f"Found {len(patterns)} vector patterns")

    if not patterns:
        print("ERROR: No patterns found!")
        return 1

    # Render patterns
    print("Rendering patterns...")
    scale = 0.1  # 100 HPGL units = 10 pixels
    images = []

    for pattern in patterns:
        print(f"  Rendering {pattern['name']}...", end='')
        img = render_pattern(pattern, scale)
        images.append(img)
        print(f" {img.width}x{img.height}px")

    # Pack atlas
    print("\nPacking atlas...")
    atlas, metadata = pack_atlas(patterns, images)
    print(f"Atlas size: {atlas.width}x{atlas.height}")

    # Create output directory
    os.makedirs(os.path.dirname(OUTPUT_ATLAS_PNG), exist_ok=True)

    # Save atlas
    atlas.save(OUTPUT_ATLAS_PNG)
    print(f"Saved {OUTPUT_ATLAS_PNG}")

    with open(OUTPUT_ATLAS_JSON, 'w') as f:
        json.dump(metadata, f, indent=2)
    print(f"Saved {OUTPUT_ATLAS_JSON}")

    # Summary
    print(f"\n=== Pattern Atlas Stats ===")
    print(f"Total patterns: {len(patterns)}")
    print(f"Atlas size: {atlas.width}x{atlas.height}")

    # Show first few patterns
    print("\nFirst 10 patterns:")
    for name in list(metadata['patterns'].keys())[:10]:
        pat = metadata['patterns'][name]
        print(f"  {name}: rect={pat['rect']}, fill={pat['fill_type']}")

    return 0


if __name__ == "__main__":
    exit(main())
