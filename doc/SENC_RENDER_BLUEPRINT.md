SENC_RENDER_BLUEPRINT.md

Unified Documentation — Nautograf · QuteNav · OpenCPN

⸻

1. Overview

This document describes the complete data and rendering pipeline converting OESU/OSENC/SENC nautical charts into pixels on screen across three codebases:

Project	Platform	Render Backend	Role in Ecosystem
OpenCPN	C++/wxWidgets	OpenGL / ocpnDC	Legacy reference; defines SENC structure, S-52 portrayal, projection math
QuteNav	C++/QtQuick	OpenGL (desktop + GLES)	Modernized engine, introduces full scene graph, cameras, shaders
Nautograf	Rust/QML hybrid	QtQuick (WGPU planned)	Experimental; fully asynchronous tile/tessellation; modern geometry pipeline

Goal: Document how parsed SENC data flows through projection, styling, geometry preparation, GPU upload, and rendering, regardless of language.

⸻

2. End-to-End Flow Summary

flowchart TD
  A[OESU File (Encrypted)] -->|oexserverd| B[SENC File (Decrypted)]
  B --> C[Parser → S57 Objects]
  C --> D[Feature Model + Attributes]
  D --> E[Style Rules (S-52 / XML / internal tables)]
  E --> F[Geometry Prep → Triangulate / Line Mesh / Symbol Quads]
  F --> G[Projection & Transform → Mercator / CM93]
  G --> H[GPU Upload (VBO / UBO / Textures)]
  H --> I[Draw Calls → Framebuffer]
  I --> J[Screen Display (with interaction layer)]


⸻

3. SENC Parsing Layer

3.1 Source & Decryption

Project	Step	Implementation
OpenCPN	Calls BuildSENCFile() or Osenc::ingest200()	Reads OESU via o-charts plugin → decrypts using oexserverd
Nautograf	External call to oexserverd binary via subprocess	Output fed directly to SENC parser (no disk intermediate)
QuteNav	Reads SENC directly (plugin handles decryption)	SENC parsed into feature vectors (S57Feature structs)

Key Records (from OpenCPN):

Record ID	Description	Type
1–8	Header / metadata	Global
64	FEATURE_ID_RECORD	New object
65	FEATURE_ATTRIBUTE_RECORD	Key–value attributes
80–83	FEATURE_GEOMETRY_RECORD_*	Point / line / area
96–97	Vector edge & connectivity tables	Topology


⸻

4. Feature Model & Internal Representation

Common Abstraction

All three engines represent a feature as:

S57Object / S57Feature
 ├─ id: u32
 ├─ class: enum (DEPARE, LNDARE, BOYSPP, ...)
 ├─ geometry_type: POINT | LINE | AREA
 ├─ geometry: Vec<Coordinate>
 ├─ attributes: HashMap<String, AttributeValue>
 ├─ style_ref: LUP (Look-Up Pres)
 └─ bounding_box: Rect

Storage
	•	OpenCPN: S57Obj (C++ struct), attributes parsed inline, stored in ObjRazRules.
	•	QuteNav: S57Feature (C++/Qt), serialized via Cap’n Proto for tile caching.
	•	Nautograf: FeatureGeometry + ChartData (Cap’n Proto schema, Rust structs).

Relationships
	•	Edge→Area reconstruction via vector edge tables.
	•	Hole handling using ring lists (outer + inner polygons).
	•	Shared edges de-duplicated in memory.

⸻

5. Styling & Portrayal (S-52 Layer)

Concept	OpenCPN	QuteNav	Nautograf
Rule Source	chartsymbols.xml, S52PLIB CSVs	XML + internal palette	Built-in S-52 lookup JSON
Resolution	ps52plib->S52_LUPLookup() → rules	StyleResolver → Material	Pre-resolved at parse time
Rule Types	Point, Line, Area, Conditional	Material categories	Style enums
Priority	0–9 (PRIO_NODATA → PRIO_MARINERS)	Material draw order	Render pass order

Example: DEPARE (depth area)

Layer	Color	Symbol	Pass
Area fill	DEPTH_SHALLOW	blue	Geometry pass
Contour	SNDG_CONTOUR	line pattern	Line pass
Text	DEPTH_LABEL	numeric label	Annotation pass

Pseudocode

for feature in S57Objects:
    LUP = lookup(feature.class, feature.geometry_type)
    material = MaterialCreator.create(LUP.style)
    draw_queue[LUP.priority].append(RenderItem(feature.geometry, material))


⸻

6. Projection & Coordinate Transforms

6.1 Shared Math

Formula	Purpose	Implemented in
x = (lon - lon0) * DEGREE * z	Mercator easting	OpenCPN georef.cpp, QuteNav SimpleMercator::fromWGS84()
y = 0.5 * log((1 + sin(lat)) / (1 - sin(lat))) * z	Mercator northing	Same
lat = 2 * atan(exp((y + y0)/z)) - π/2	Inverse Mercator	Same

Constants:
z = 6378137 * 0.9996 (WGS84, k₀), DEGREE = π / 180.

6.2 Projection Variants

Name	Used by	Notes
SimpleMercator	QuteNav / Nautograf	WGS84 + scale factor
CM93Mercator	QuteNav	Non-uniform scaling
Transverse Mercator	OpenCPN	UTM support
Orthographic / Stereographic	OpenCPN	Polar charts
NoopProjection	QuteNav	Identity mapping (for debugging)

6.3 Local Origin Strategy

All three use a local origin near viewport center:

y = Mercator(lat) - Mercator(lat_ref)
x = (lon - lon_ref) * cos(lat_ref)

→ minimizes float precision loss (sub-millimeter over 100 km²).

⸻

7. Geometry Preparation (CPU Tessellation)

Pipeline

flowchart LR
  A[S57 Features] --> B[Tessellator]
  B --> C[Polygons → Triangles]
  B --> D[Lines → Quad Strips]
  B --> E[Points → Annotation Quads]
  C & D & E --> F[GPU Buffers]

7.1 Polygons (Areas)
	•	Nautograf: Earcut triangulation (with hole support).
	•	QuteNav: Internal Triangulator::calc.
	•	OpenCPN: CPU-side tesselation in GL_AREA_Render().
	•	Stored as vertex arrays (x,y,z,rgba).

7.2 Lines
	•	Converted to quad strips with miter joins.
	•	Line shaders handle width and anti-aliasing in clip space.

7.3 Points (Symbols / Text)
	•	Mapped to symbol atlas or MSDF font quads.
	•	Each carries minZoom for visibility fading.

⸻

8. GPU Resource Model

Resource	Description	Typical Use
VBO	Vertex positions	Per geometry type
IBO	Triangle indices	Shared by tiles
UBO	Per-frame uniforms (MVP, colors, zoom)	Updated per draw
Textures	Symbol & font atlases	Sampled in fragment shader

OpenCPN
	•	Immediate mode historically → later GL vertex arrays.
QuteNav / Nautograf
	•	Persistent VBOs per tile or chart; shaders handle all styling.

⸻

9. Render Pipeline

QuteNav / Nautograf Scene Graph

Pass	Content	Notes
1. Geometry	Areas & lines	Depth-tested opaque draw
2. Symbols	Buoys, lights, ATONs	Alpha-blended
3. Text	MSDF labels	Blended, screen-space
4. Overlay	UI, routes, cursor	Non-depth blended

OpenCPN

Procedural rendering per priority/layer via ObjRazRules[10][5] →
calls RenderRegionViewOnGL() or RenderRegionViewOnDC().

Matrix Composition

gl_Position = ProjectionMatrix * ModelMatrix * vec4(vertex.xy, depth, 1.0)

	•	ProjectionMatrix: from camera (Ortho or Perspective).
	•	ModelMatrix: chart offset + scale.
	•	vertex.xy: local chart coordinates (meters or pixels).

⸻

10. Caching and Tiling

Layer	Cached Data	Purpose
Chart Cache	Full parsed SENC → Cap’n Proto binary (all_1000.bin)	Avoid reparse
Tile Cache	Visible viewport subset (tile_12_34.bin)	Fast re-entry
GPU Cache	Vertex buffers per tile	Minimal uploads

	•	Nautograf: tile-level async workers for tessellation.
	•	QuteNav: synchronous, but caches per ChartTile.
	•	OpenCPN: caches SENC on disk + SM coords per viewport.

⸻

11. Event and Interaction Layer

Action	Implemented In	Description
Pan	QuteNav: Camera::pan() · OpenCPN: SetViewPoint()	Moves local origin (setReference())
Zoom	Logarithmic scale adjustment	Updates view_scale_ppm or Camera::scale
Rotate	View matrix Z-rotation	Updates rotation uniform
Click / Query	ChartDisplay::infoQuery()	Converts pixel → WGS84 (reverse transform)

No inertial scrolling in QuteNav/OpenCPN; Nautograf roadmap includes it.

⸻

12. Precision Management

Stage	Type	Units	Notes
Geographic	double	degrees	stored in SENC, WGS84
Projected	double	meters	local origin per chart
Screen	float	pixels	GPU, stable within ±1e4 px

→ Combined error < 1e-5° ≈ 1 meter even on 32-bit GPU floats.

⸻

13. Performance Patterns (esp. on Raspberry Pi)

Principle	Implemented In	Effect
Avoid per-frame buffer rebuild	Nautograf	Keeps GPU bandwidth low
Tile-based redraw	Nautograf / QuteNav	Partial frame updates
Shared textures & materials	QuteNav	Reduces state changes
Simplification (RDP algo)	Nautograf (Rust FFI)	Fewer vertices per poly
Early-Z and opaque-first ordering	QuteNav	Cuts overdraw
Local origin shifts	All	Prevents float drift


⸻

14. Unified Pseudocode

function render_chart(viewport, senc_path):
    features = parse_senc(senc_path)
    for f in features:
        proj_xy = projection.from_wgs84(f.geometry)
        style = style_lookup(f.class, f.attrs)
        mesh = tessellate(f.geometry_type, proj_xy)
        batch[style.pass].append({mesh, style})

    for pass in render_order:
        set_pipeline_state(pass)
        for item in batch[pass]:
            upload(item.mesh)
            bind(item.style.material)
            draw(item.mesh)


⸻

15. Data Contracts for Reimplementation

Module	Input	Output	Invariants
parse_senc()	Binary SENC	Feature list	All features valid geometry
projection.from_wgs84()	(lat,lon)	(x,y) meters	Continuous within tile
style_lookup()	feature class, attributes	Material parameters	Deterministic
tessellate()	Geometry type	Vertex buffer	Closed polygons, CW winding
upload()	Vertex buffer	GPU handle	Immutable until eviction
draw()	Mesh + Material	Frame output	Ordered, batched by material


⸻

16. Open Questions / Gaps

Topic	Observed in	Clarification Needed
Vector edge topology merging	OpenCPN only	Confirm algorithm for holes reconstruction
S-52 conditional rules	OpenCPN / Nautograf	Which LUP attributes affect dynamic display?
Depth color scaling	QuteNav	Derived or fixed palette?
Annotation layering	Nautograf	Sorted by zoom threshold or priority?
GPU instancing for buoys	Nautograf planned	Feasibility on RPi Vulkan?


⸻

17. Key Constants Reference

Constant	Value	Used In
WGS84_RADIUS	6378137.0 m	All
MERCATOR_K0	0.9996	OpenCPN / QuteNav
EQUALITY_RADIUS	20 m	QuteNav
EPS_CLIP	0.1 m	QuteNav geomutils
EPS_POLE	1e-5	PersCam pole handling
MIN_SCALE_ORTHO	800	QuteNav
MIN_SCALE_PERSPECTIVE	20000	QuteNav


⸻

18. Diagram: Unified Coordinate Flow

flowchart TD
  A[Lat/Lon (WGS84)] --> B[GeoProjection::fromWGS84()]
  B --> C[Projection Space (meters)]
  C --> D[Chart Model Transform]
  D --> E[Camera View Matrix (rotation)]
  E --> F[Camera Projection Matrix (ortho/persp)]
  F --> G[Clip Space (-1..1)]
  G --> H[Viewport Transform → Screen Pixels]


⸻

19. Conclusion

Across all three systems:
	•	OpenCPN establishes the SENC file format, projection math, and S-52 portrayal logic.
	•	QuteNav refines the rendering model — camera abstraction, GPU batching, and QML integration.
	•	Nautograf modernizes it further: asynchronous tile parsing, earcut tessellation, and GPU-first architecture.

Together, they define a reproducible and portable SENC rendering pipeline that can be reimplemented in any language (e.g., Rust + WGPU) while maintaining full parity with S-52 and CM93 behavior.
