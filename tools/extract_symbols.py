#!/usr/bin/env python3
"""
Extract symbol metadata from OpenCPN chartsymbols.xml and generate atlas.json.

This script:
1. Parses chartsymbols.xml to extract symbol positions in rastersymbols-*.png
2. Generates atlas.json with all symbol metadata
3. Copies rastersymbols-day.png to assets/symbols/atlas.png

Usage:
    python3 tools/extract_symbols.py
"""

import xml.etree.ElementTree as ET
import json
import shutil
import os
from pathlib import Path

# Paths relative to project root
CHARTSYMBOLS_PATH = "assets/s52/chartsymbols.xml"
RASTERSYMBOLS_PATH = "doc/reference projects/OpenCPN/data/s57data/rastersymbols-day.png"
OUTPUT_ATLAS_PNG = "assets/symbols/atlas.png"
OUTPUT_ATLAS_JSON = "assets/symbols/atlas.json"

def parse_symbols(xml_path: str) -> list:
    """Parse chartsymbols.xml and extract symbol metadata."""
    tree = ET.parse(xml_path)
    root = tree.getroot()

    symbols = []

    # Find all symbol elements
    for symbol in root.iter('symbol'):
        name_elem = symbol.find('name')
        bitmap_elem = symbol.find('bitmap')

        if name_elem is None or bitmap_elem is None:
            continue

        name = name_elem.text
        if name is None:
            continue

        # Get bitmap dimensions
        width = int(bitmap_elem.get('width', 0))
        height = int(bitmap_elem.get('height', 0))

        if width == 0 or height == 0:
            continue

        # Get graphics location (position in rastersymbols.png)
        graphics_loc = bitmap_elem.find('graphics-location')
        if graphics_loc is None:
            continue

        x = int(graphics_loc.get('x', 0))
        y = int(graphics_loc.get('y', 0))

        # Get pivot point (origin for rotation/placement)
        pivot_elem = bitmap_elem.find('pivot')
        if pivot_elem is not None:
            pivot_x = int(pivot_elem.get('x', width // 2))
            pivot_y = int(pivot_elem.get('y', height // 2))
        else:
            pivot_x = width // 2
            pivot_y = height // 2

        # Calculate normalized pivot (0.0 - 1.0)
        # OpenCPN uses Y-down (0=top), shader uses Y-up (0=bottom), so flip Y
        pivot_x_norm = pivot_x / width if width > 0 else 0.5
        pivot_y_norm = 1.0 - (pivot_y / height) if height > 0 else 0.5

        symbols.append({
            'name': name,
            'x': x,
            'y': y,
            'width': width,
            'height': height,
            'pivot': [round(pivot_x_norm, 3), round(pivot_y_norm, 3)]
        })

    return symbols


def generate_atlas_json(symbols: list) -> dict:
    """Generate atlas.json format from extracted symbols."""
    # Get the image dimensions (rastersymbols-day.png is typically 1024x1024 or similar)
    # We'll use the OpenCPN image directly

    atlas = {
        "version": 2,
        "format": "direct",  # Direct pixel coordinates, not cell-based
        "atlas_size": [1024, 1024],  # Will be updated if needed
        "symbols": {}
    }

    max_x = 0
    max_y = 0

    for sym in symbols:
        atlas["symbols"][sym['name']] = {
            "rect": [sym['x'], sym['y'], sym['width'], sym['height']],
            "pivot": sym['pivot']
        }
        max_x = max(max_x, sym['x'] + sym['width'])
        max_y = max(max_y, sym['y'] + sym['height'])

    # Round up atlas size to power of 2
    def next_power_of_2(x):
        p = 1
        while p < x:
            p *= 2
        return p

    atlas["atlas_size"] = [next_power_of_2(max_x), next_power_of_2(max_y)]

    return atlas


def main():
    # Change to project root
    script_dir = Path(__file__).parent
    project_root = script_dir.parent
    os.chdir(project_root)

    print(f"Working directory: {os.getcwd()}")

    # Check paths exist
    if not os.path.exists(CHARTSYMBOLS_PATH):
        print(f"ERROR: chartsymbols.xml not found at {CHARTSYMBOLS_PATH}")
        return 1

    if not os.path.exists(RASTERSYMBOLS_PATH):
        print(f"ERROR: rastersymbols-day.png not found at {RASTERSYMBOLS_PATH}")
        return 1

    # Parse symbols
    print(f"Parsing {CHARTSYMBOLS_PATH}...")
    symbols = parse_symbols(CHARTSYMBOLS_PATH)
    print(f"Found {len(symbols)} symbols with bitmap definitions")

    # Generate atlas.json
    print("Generating atlas.json...")
    atlas = generate_atlas_json(symbols)

    # Write atlas.json
    os.makedirs(os.path.dirname(OUTPUT_ATLAS_JSON), exist_ok=True)
    with open(OUTPUT_ATLAS_JSON, 'w') as f:
        json.dump(atlas, f, indent=2)
    print(f"Written {OUTPUT_ATLAS_JSON} with {len(atlas['symbols'])} symbols")

    # Copy rastersymbols-day.png to atlas.png
    print(f"Copying {RASTERSYMBOLS_PATH} -> {OUTPUT_ATLAS_PNG}")
    shutil.copy(RASTERSYMBOLS_PATH, OUTPUT_ATLAS_PNG)

    # Print some stats
    print("\n=== Symbol Atlas Stats ===")
    print(f"Total symbols: {len(symbols)}")
    print(f"Atlas size: {atlas['atlas_size']}")

    # Show first few symbols
    print("\nFirst 10 symbols:")
    for name in list(atlas['symbols'].keys())[:10]:
        sym = atlas['symbols'][name]
        print(f"  {name}: rect={sym['rect']}, pivot={sym['pivot']}")

    return 0


if __name__ == "__main__":
    exit(main())
