# OSENC File Format Reference

> Derived from OpenCPN source code analysis (Osenc.cpp, Osenc.h, mygeom.cpp, georef.cpp)

## Record Types

| Type | Name | Section | Notes |
|------|------|---------|-------|
| 1 | HEADER_SENC_VERSION | Header | |
| 2 | HEADER_CELL_NAME | Header | |
| 3 | HEADER_CELL_PUBLISHDATE | Header | |
| 4 | HEADER_CELL_EDITION | Header | |
| 5 | HEADER_CELL_UPDATEDATE | Header | |
| 6 | HEADER_CELL_UPDATE | Header | |
| 7 | HEADER_CELL_NATIVESCALE | Header | |
| 8 | HEADER_CELL_SENCCREATEDATE | Header | **NavCore was missing this!** |
| 9-63 | UNUSED | - | |
| 64 | FEATURE_ID_RECORD | Features | |
| 65 | FEATURE_ATTRIBUTE_RECORD | Features | |
| 66-79 | UNUSED | - | |
| 80 | FEATURE_GEOMETRY_RECORD_POINT | Features | |
| 81 | FEATURE_GEOMETRY_RECORD_LINE | Features | |
| 82 | FEATURE_GEOMETRY_RECORD_AREA | Features | |
| 83 | FEATURE_GEOMETRY_RECORD_MULTIPOINT | Features | |
| 84-95 | UNUSED | - | |
| 96 | VECTOR_EDGE_NODE_TABLE_RECORD | After features | |
| 97 | VECTOR_CONNECTED_NODE_TABLE_RECORD | After features | |
| 98 | CELL_COVR_RECORD | Header section | Multiple |
| 99 | CELL_NOCOVR_RECORD | Header section | Multiple |
| 100 | CELL_EXTENT_RECORD | Header section | |

### Record Order (Guaranteed)

```
1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 100 → 98* → 99* → 64+
```

## Coordinate Systems

### CRITICAL: Vertices are SM (Simple Mercator) Relative to Reference Point

**NOT global Mercator meters!**

The reference point is the **centroid of the extent**:
```
ref_lat = (extent.NLAT + extent.SLAT) / 2
ref_lon = (extent.WLON + extent.ELON) / 2
```

### CellExtent (type 100) Payload

**WGS84 lat/lon** stored as 4 doubles:
- extent_sw_lat
- extent_sw_lon
- extent_ne_lat (or nw/se corners)
- extent_ne_lon

### AreaGeometry (type 82) Payload

Mixed coordinate systems:
- **Record extent (bbox)**: WGS84 lat/lon
- **Triangle primitive bboxes**: WGS84 lat/lon
- **Triangle vertices**: **SM meters** (f32) relative to reference point

## SM (Simple Mercator) Formulas

From OpenCPN `georef.cpp:354-375`:

### Constants

```rust
const DEGREE: f64 = PI / 180.0;
const WGS84_SEMIMAJOR: f64 = 6378137.0;
const MERCATOR_K0: f64 = 0.9996;
const Z: f64 = WGS84_SEMIMAJOR * MERCATOR_K0;  // ~6378123.44
```

### toSM (lat/lon → SM meters)

```rust
fn to_sm(lat: f64, lon: f64, ref_lat: f64, ref_lon: f64) -> (f64, f64) {
    const Z: f64 = 6378137.0 * 0.9996;

    // X: Linear in longitude
    let x = (lon - ref_lon) * DEGREE * Z;

    // Y: Web Mercator formula with reference offset
    let s = (lat * DEGREE).sin();
    let y3 = (0.5 * ((1.0 + s) / (1.0 - s)).ln()) * Z;

    let s0 = (ref_lat * DEGREE).sin();
    let y30 = (0.5 * ((1.0 + s0) / (1.0 - s0)).ln()) * Z;

    let y = y3 - y30;  // Subtract reference point's Y

    (x, y)
}
```

### fromSM (SM meters → lat/lon)

```rust
fn from_sm(x: f64, y: f64, ref_lat: f64, ref_lon: f64) -> (f64, f64) {
    const Z: f64 = 6378137.0 * 0.9996;

    let s0 = (ref_lat * DEGREE).sin();
    let y0 = (0.5 * ((1.0 + s0) / (1.0 - s0)).ln()) * Z;

    let lat = (2.0 * ((y0 + y) / Z).exp().atan() - PI / 2.0) / DEGREE;
    let lon = ref_lon + (x / (DEGREE * Z));

    (lat, lon)
}
```

### SM to Global Mercator Conversion

To convert SM vertices to global Mercator for rendering:

```rust
// First compute reference point's global Mercator Y
let s0 = (ref_lat * DEGREE).sin();
let y0_ref = (0.5 * ((1.0 + s0) / (1.0 - s0)).ln()) * Z;

// Then for each vertex:
global_x = sm_x + ref_lon * DEGREE * Z;
global_y = sm_y + y0_ref;
```

## Feature ID Record (type 64) Payload

5 bytes:
- 2 bytes: feature_type_code (u16) - S-57 object code
- 2 bytes: feature_ID (u16)
- 1 byte: feature_primitive (u8) - 1=point, 2=line, 3=area, 4=multipoint

## Attribute Record (type 65) Payload

- 2 bytes: attribute_type (u16) - S-57 attribute code
- 1 byte: attribute_value_type:
  - 0 = Integer (4 bytes)
  - 1 = Integer List (N × 4 bytes)
  - 2 = Double (8 bytes)
  - 3 = Double List (N × 8 bytes)
  - 4 = String (null-terminated UTF-8)
- Variable: value data

## Common S-57 Object Codes

| Code | Acronym | Description |
|------|---------|-------------|
| 4 | BCNLAT | Beacon lateral |
| 17 | BOYLAT | Buoy lateral |
| 30 | COALNE | Coastline |
| 42 | DEPARE | Depth area |
| 43 | DEPCNT | Depth contour |
| 71 | LNDARE | Land area |
| 129 | SOUNDG | Sounding |
| 153 | UWTROC | Underwater rock |

## Common S-57 Attribute Codes

| Code | Acronym | Description |
|------|---------|-------------|
| 87 | DRVAL1 | Depth range value 1 (shallow) |
| 88 | DRVAL2 | Depth range value 2 (deep) |
| 174 | VALDCO | Value of depth contour |
| 178 | VALSOU | Value of sounding |
| 76 | OBJNAM | Object name |
| 142 | SCAMIN | Scale minimum |
| 143 | SCAMAX | Scale maximum |
