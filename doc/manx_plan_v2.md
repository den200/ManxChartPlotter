# NavCore Implementation Plan v2.0

**Revised based on Nautograf + QuteNav + OpenCPN analysis**

---

## Executive Summary

NavCore is a Rust-based nautical chart plotter targeting Raspberry Pi 4/5 and Mac ARM. It decrypts and renders o-charts oeSENC/oesu locally using WGPU. All navigation data arrives via Signal K.

**Target charts:** oeSENC (OpenCPN Encrypted System Electronic Nautical Charts)  
— Economical S-57 data encrypted by o-charts, decrypted via oexserverd

**Performance Targets:**
- 60 fps @ 1080p on Pi 4
- Cold boot to chart < 30 seconds  
- ≤256 MB GPU memory budget

---

## Why We Can Skip Full S-52 (For Now)

### What S-52 Is (Simple Terms)

S-52 is a **400-page IHO standard** that defines exactly how charts should **LOOK**:
- Which exact RGB colors to use (50+ named tokens)
- Which symbol to show for each buoy, light, hazard
- 40+ "conditional symbology" procedures that change appearance based on attributes
- Day/Dusk/Night palette switching

**S-52 does NOT change what navigation data is shown — only how it's styled.**

### What Nautograf Proves

> **Nautograf achieves 60fps on Raspberry Pi WITHOUT implementing S-52.**
> All colors and symbols are hardcoded C++. The chart shows the same data — just with simpler styling.

### What This Means for Users

| Aspect | With Full S-52 | Without S-52 (Our MVP) |
|--------|----------------|------------------------|
| **Appearance** | Identical to official plotters | Slightly different colors |
| **Navigation data** | All depths, buoys, hazards | ✅ **Same data, all visible** |
| **Safety** | Certified for commercial vessels | ✅ Safe for recreational if we get depth coloring right |
| **Development** | +3 months | Ship MVP in 14 weeks |
| **Certification** | Required for SOLAS vessels | Not needed for recreational use |

### Safety-Critical Features (We MUST Implement)

Even without full S-52, these are non-negotiable:

1. **Depth area coloring** — Shallow water MUST look dangerous (pink/red)
2. **Safety contour emphasis** — Thick line at vessel draft depth
3. **Hazard visibility** — Wrecks, obstructions, rocks must be obvious

### What We Skip for MVP

- Exact S-52 RGB palette (use similar colors)
- 40 conditional symbology procedures
- Light sector arc generation (LIGHTS05)
- Pattern fills (hatching, stipples)
- Day/Dusk/Night switching

**Post-MVP:** Full S-52 compliance can be added in Appendix A when needed for certification.

---

## Key Insight from Nautograf Analysis

> **Nautograf does NOT implement S-52. All styling is hardcoded C++. It achieves 60fps on Pi anyway.**

This fundamentally changes the plan:

| Old Assumption | Reality | New Approach |
|----------------|---------|--------------|
| S-52 required for rendering | S-52 is for compliance, not performance | Defer S-52 to post-MVP |
| Must parse edge topology | oesenc delivers pre-assembled polygons | Use oesenc if available, skip topology |
| Need complex atlas packing | Row-based packing works fine | Fixed grid is sufficient |
| Line patterns essential | Solid lines work for demo | Add patterns later |

---

## Critical Finding: OSENC Has Pre-Triangulated Areas

**Confirmed from OpenCPN + QuteNav analysis:**

> **OSENC files ALWAYS contain pre-triangulated area geometry.**
> `triprim_count > 0` for all area features. No runtime tessellation needed.

This is a **massive simplification** for NavCore:

| Without This Finding | With This Finding |
|---------------------|-------------------|
| Parse polygon rings | Parse triangle arrays directly |
| Handle holes with earcut | Holes already tessellated |
| CPU tessellation per area | Just read floats into GPU buffer |
| earcut dependency needed | **No earcut dependency** |
| Complex winding order logic | Already correct in file |

### Triangle Primitive Format (from OpenCPN [Osenc.h:193-203])

```rust
// Each area record contains:
struct AreaGeometryRecord {
    extent: BBox,           // 4 × f64
    contour_count: u32,     // Number of rings (ignored for fill)
    triprim_count: u32,     // ALWAYS > 0 for areas
    edge_count: u32,        // For outline rendering
    // Then: triprim_count × TriPrim
    // Then: edge_count × EdgeRef
}

// Each triangle primitive:
struct TriPrim {
    prim_type: u8,          // 4=GL_TRIANGLES, 5=STRIP, 6=FAN
    nvert: u32,             // Vertex count
    bbox: [f64; 4],         // min_x, max_x, min_y, max_y
    vertices: Vec<[f32; 2]>, // nvert × (x, y)
}
```

### NavCore Parser (Simplified)

```rust
// navcore-senc/src/area.rs

pub struct AreaGeometry {
    pub triangles: Vec<TriPrim>,  // Pre-tessellated from file
    pub edge_refs: Vec<EdgeRef>,  // For outline (optional)
}

impl AreaGeometry {
    pub fn parse(reader: &mut impl Read) -> Result<Self> {
        let _extent = BBox::read(reader)?;
        let _contour_count = reader.read_u32::<LE>()?;
        let triprim_count = reader.read_u32::<LE>()?;
        let edge_count = reader.read_u32::<LE>()?;
        
        // Skip contour counts (not needed for fill)
        reader.seek(SeekFrom::Current((_contour_count * 4) as i64))?;
        
        // Read pre-triangulated data directly
        let mut triangles = Vec::with_capacity(triprim_count as usize);
        for _ in 0..triprim_count {
            triangles.push(TriPrim::read(reader)?);
        }
        
        // Read edge refs for outline (can skip for MVP)
        let edge_refs = EdgeRef::read_array(reader, edge_count)?;
        
        Ok(Self { triangles, edge_refs })
    }
    
    /// Convert to GPU vertex buffer — no tessellation needed!
    pub fn to_vertices(&self) -> Vec<Vertex> {
        let mut vertices = Vec::new();
        for tri in &self.triangles {
            // Convert STRIP/FAN to plain triangles
            vertices.extend(tri.to_triangles());
        }
        vertices
    }
}
```

### Impact on Dependencies

```toml
[workspace.dependencies]
# Core (M0-M1)
wgpu = "0.26"
winit = "0.30"
bytemuck = { version = "1.21", features = ["derive"] }
byteorder = "1.5"      # For SENC parsing

# NOT NEEDED for MVP:
# earcut = "0.4"       # ← REMOVED: OSENC has pre-triangulated areas
```

**KISS Application (Updated):**
1. See pixels by Week 4 ("First Light")
2. Hardcoded colors until rendering works
3. Single chart before tile cache
4. No disk persistence initially
5. **No earcut — use pre-triangulated data from file**

---

## Milestones (Revised)

```
M0: Decoder Proven    [Week 1-2]   CLI decrypts OESU, parser extracts features
M1: First Light       [Week 3-4]   LNDARE + DEPARE visible, pan/zoom works
M2: Chart Viewer      [Week 5-8]   All features, gestures, in-memory cache
M3: Signal K          [Week 9-10]  Own-ship overlay
M4: AIS               [Week 11-12] AIS targets with declutter
M5: Packaging         [Week 13-14] Pi-gen image, .deb
```

**Critical change:** M1 is "First Light" (GPU proof), not parsing completion.

---

## Workspace Structure

```
navcore/
├── Cargo.toml
├── crates/
│   ├── navcore-cli/        # CLI tools (decrypt, info)
│   ├── navcore-senc/       # SENC parser (reads pre-triangulated data)
│   ├── navcore-render/     # WGPU + projection (no tessellation needed)
│   └── navcore-app/        # Window, input, Signal K
├── assets/
│   ├── shaders/            # area.wgsl, line.wgsl, symbol.wgsl
│   └── symbols/            # Atlas PNG + metadata JSON
└── tests/
    └── golden/             # Reference screenshots
```

**Removed:** Separate `navcore-signalk` crate. It's ~200 lines, lives in app.

---

## Dependencies

```toml
[workspace.dependencies]
# Core (M0-M1)
wgpu = "0.26"
winit = "0.30"
pollster = "0.4"
glam = "0.29"
bytemuck = { version = "1.21", features = ["derive"] }
byteorder = "1.5"          # For SENC binary parsing

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# Async (M3+)
tokio = { version = "1", features = ["rt", "net", "sync"] }
tokio-tungstenite = "0.26"
futures-util = "0.3"

# Utilities
thiserror = "2"
tracing = "0.1"
tracing-subscriber = "0.3"

# Deferred (add when needed)
# rkyv = "0.8"        # Disk cache - M2 if memory pressure
# rstar = "0.12"      # R-tree - when multi-chart needed
```

---

## Phase 1: Decoder CLI (M0)

### 1.1 oexserverd Integration

**Ref:** SENC_RENDER_BLUEPRINT §3.1

```rust
// navcore-senc/src/decrypt.rs
pub struct Decryptor {
    oexserverd_path: PathBuf,
}

impl Decryptor {
    /// Decrypt OESU to SENC bytes via oexserverd socket
    pub fn decrypt(&self, oesu_path: &Path, key: &str) -> Result<Vec<u8>> {
        // Spawn oexserverd if not running
        // Send decrypt request via Unix socket
        // Return decrypted bytes (no disk intermediate)
    }
}
```

### 1.2 Critical Insight: Polygon Assembly

**From Nautograf analysis §2:**

> **oesenc library delivers PRE-ASSEMBLED polygons with holes.**
> No edge topology reconstruction (SENC records 96/97) occurs in Nautograf.

This means:
- `oesenc::S57::polygons()` returns vector of coordinate rings
- **First ring** = outer boundary
- **Subsequent rings** = holes
- **No edge stitching needed** — oesenc handles it internally

**Implication for NavCore:**
- If we use oesenc (via FFI or port): no topology code needed
- If we parse raw SENC ourselves: we must implement edge assembly
- **Decision:** Start with oesenc FFI if available, implement fallback later

### 1.2.1 Edge Topology Fallback (If oesenc Unavailable)

**From QuteNav [osenc.cpp:565-752] + OpenCPN [s57chart.cpp:982-1300]:**

If we need to parse raw SENC without oesenc, here's the algorithm:

> **⚠️ CLARIFICATION NEEDED:** OpenCPN uses **signed edge IDs** (negative = reverse), 
> QuteNav uses **boolean flag**. Must check what oeSENC actually outputs.

```rust
// navcore-senc/src/topology.rs (only needed if no oesenc)

/// Edge table built from SENC records 96-97
pub struct EdgeTable {
    /// Record 96: edge_index → list of points
    pub edges: HashMap<u32, Vec<[f32; 2]>>,
    /// Record 97: node_index → single connection point  
    pub nodes: HashMap<u32, [f32; 2]>,
}

/// Edge reference from area geometry record (type 82)
/// Two encoding variants exist in the wild:
/// - OpenCPN: signed edge_index (negative = reversed)
/// - QuteNav: separate reversed bool
pub struct EdgeRef {
    pub begin_node: u32,
    pub edge_index: u32,  // If signed: abs() for index, sign for direction
    pub end_node: u32,
    pub reversed: bool,   // CRITICAL: affects node swap
}

impl EdgeTable {
    /// Resolve edge reference to points
    /// Key insight: reversed flag swaps begin/end NODES, not point order
    pub fn resolve(&self, edge_ref: &EdgeRef) -> ResolvedEdge {
        let (begin, end) = if edge_ref.reversed {
            // SWAP nodes when reversed
            (self.nodes[&edge_ref.end_node], 
             self.nodes[&edge_ref.begin_node])
        } else {
            (self.nodes[&edge_ref.begin_node],
             self.nodes[&edge_ref.end_node])
        };
        
        ResolvedEdge { begin, end, 
            points: self.edges[&edge_ref.edge_index].clone(),
            reversed: edge_ref.reversed,
        }
    }
}

/// Assemble polygon from edge references
/// First contour = outer ring, rest = holes
pub fn assemble_area(edge_refs: &[Vec<EdgeRef>], table: &EdgeTable) -> Polygon {
    let outer = assemble_ring(&edge_refs[0], table);
    let holes = edge_refs[1..].iter()
        .map(|refs| assemble_ring(refs, table))
        .collect();
    Polygon { outer, holes }
}

fn assemble_ring(refs: &[EdgeRef], table: &EdgeTable) -> Vec<[f32; 2]> {
    let mut ring = Vec::new();
    for edge_ref in refs {
        let resolved = table.resolve(edge_ref);
        ring.push(resolved.begin);
        if resolved.reversed {
            ring.extend(resolved.points.iter().rev());
        } else {
            ring.extend(resolved.points.iter());
        }
    }
    // Close ring
    if ring.first() != ring.last() {
        ring.push(ring[0]);
    }
    ring
}
```

**Critical gotcha:** The `reversed` flag means swap begin/end *nodes*, then reverse *point iteration order*. Getting this wrong produces broken polygons with gaps.

### 1.3 Minimal Parser (Just Enough for M1)

For First Light, we only need:
- Header (cell name, extent, scale)
- LNDARE polygons (land)
- DEPARE polygons (depth areas)

```rust
// navcore-senc/src/parser.rs

/// Minimal feature set for First Light
pub struct ChartData {
    pub header: SencHeader,
    pub land_areas: Vec<Polygon>,
    pub depth_areas: Vec<DepthArea>,
}

pub struct Polygon {
    pub outer: Vec<Coord>,
    pub holes: Vec<Vec<Coord>>,  // Pre-assembled (first ring = outer, rest = holes)
}

pub struct DepthArea {
    pub polygon: Polygon,
    pub depth_min: f32,  // DRVAL1
    pub depth_max: f32,  // DRVAL2
}

pub struct Coord {
    pub lat: f64,
    pub lon: f64,
}
```

**Winding order:** Earcut requires CCW outer, CW holes. Validate or fix during parse.

### 1.3 CLI Verification

```bash
navcore-cli decrypt --chart test.oesu --key XXX --output test.senc
navcore-cli info test.senc
# Output: Cell: US5WA123, Scale: 1:22000, Features: 1847, LNDARE: 23, DEPARE: 156
```

**M0 Exit Criteria:**
- [ ] `decrypt` produces valid SENC bytes
- [ ] `info` shows correct feature count
- [ ] Bounding box matches OpenCPN for same chart

---

## Phase 2: First Light (M1)

**Goal:** See LNDARE (tan) and DEPARE (blue gradient) in a WGPU window, with working pan/zoom.

### 2.1 Bring Up WGPU First

Before parsing is complete, prove the GPU pipeline works:

```rust
// navcore-render/src/lib.rs

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    pipeline: wgpu::RenderPipeline,
}

impl Renderer {
    pub async fn new(window: &Window) -> Self {
        // Standard WGPU init per wgpu_ref_notes §1
    }
    
    /// Draw a test triangle to prove GPU works
    pub fn draw_test_triangle(&mut self) {
        // Hardcoded triangle vertices
        // If this works, GPU pipeline is valid
    }
}
```

**Week 3 checkpoint:** Test triangle renders on both Mac and Pi.

### 2.2 Projection (Mercator)

**Ref:** SENC_RENDER_BLUEPRINT §6

```rust
// navcore-render/src/projection.rs

pub const WGS84_RADIUS: f64 = 6_378_137.0;

pub struct MercatorProjection {
    pub origin_lat: f64,
    pub origin_lon: f64,
    cos_origin_lat: f64,
    origin_y: f64,
}

impl MercatorProjection {
    /// WGS84 → local Mercator meters
    pub fn project(&self, lat: f64, lon: f64) -> (f64, f64) {
        let x = (lon - self.origin_lon).to_radians() * WGS84_RADIUS * self.cos_origin_lat;
        let y = mercator_y(lat) - self.origin_y;
        (x, y)
    }
}

fn mercator_y(lat: f64) -> f64 {
    let lat_rad = lat.to_radians();
    WGS84_RADIUS * ((1.0 + lat_rad.sin()) / (1.0 - lat_rad.sin())).ln() * 0.5
}
```

### 2.3 Camera

```rust
// navcore-render/src/camera.rs

pub struct Camera {
    pub center: glam::DVec2,  // Mercator meters
    pub scale: f64,           // pixels per meter
    pub viewport: glam::UVec2,
}

impl Camera {
    pub fn view_projection_matrix(&self) -> glam::Mat4 {
        let half_w = (self.viewport.x as f64 / self.scale / 2.0) as f32;
        let half_h = (self.viewport.y as f64 / self.scale / 2.0) as f32;
        
        let proj = glam::Mat4::orthographic_rh(-half_w, half_w, -half_h, half_h, 0.0, 1.0);
        let view = glam::Mat4::from_translation(glam::vec3(
            -self.center.x as f32,
            -self.center.y as f32,
            0.0,
        ));
        proj * view
    }
    
    pub fn pan(&mut self, screen_delta: glam::Vec2) {
        self.center -= glam::dvec2(
            screen_delta.x as f64 / self.scale,
            -screen_delta.y as f64 / self.scale,
        );
    }
    
    pub fn zoom(&mut self, factor: f64, screen_center: glam::Vec2) {
        // Zoom centered on point (keep that world point under cursor)
    }
}
```

### 2.4 Area Geometry (Pre-Triangulated from File)

**Confirmed:** OSENC areas ALWAYS contain pre-triangulated geometry.
No earcut needed. Just read triangles from file.

```rust
// navcore-senc/src/triprim.rs

/// Triangle primitive types from OpenCPN [Osenc.h]
#[repr(u8)]
#[derive(Clone, Copy)]
pub enum TriPrimType {
    Triangles = 0x04,      // GL_TRIANGLES
    TriangleStrip = 0x05,  // GL_TRIANGLE_STRIP  
    TriangleFan = 0x06,    // GL_TRIANGLE_FAN
}

pub struct TriPrim {
    pub prim_type: TriPrimType,
    pub vertices: Vec<[f32; 2]>,  // x, y pairs
}

impl TriPrim {
    pub fn read(reader: &mut impl Read) -> Result<Self> {
        let prim_type = reader.read_u8()?;
        let nvert = reader.read_u32::<LE>()?;
        
        // Skip bounding box (4 × f64 = 32 bytes)
        reader.seek(SeekFrom::Current(32))?;
        
        // Read vertex data
        let mut vertices = Vec::with_capacity(nvert as usize);
        for _ in 0..nvert {
            let x = reader.read_f32::<LE>()?;
            let y = reader.read_f32::<LE>()?;
            vertices.push([x, y]);
        }
        
        Ok(Self { prim_type: TriPrimType::from_u8(prim_type)?, vertices })
    }
    
    /// Convert STRIP/FAN to plain triangles for GPU
    pub fn to_triangles(&self) -> Vec<[f32; 2]> {
        match self.prim_type {
            TriPrimType::Triangles => self.vertices.clone(),
            TriPrimType::TriangleStrip => self.strip_to_triangles(),
            TriPrimType::TriangleFan => self.fan_to_triangles(),
        }
    }
    
    fn strip_to_triangles(&self) -> Vec<[f32; 2]> {
        let mut result = Vec::new();
        for i in 2..self.vertices.len() {
            if i % 2 == 0 {
                result.push(self.vertices[i - 2]);
                result.push(self.vertices[i - 1]);
                result.push(self.vertices[i]);
            } else {
                // Flip winding for odd triangles
                result.push(self.vertices[i - 1]);
                result.push(self.vertices[i - 2]);
                result.push(self.vertices[i]);
            }
        }
        result
    }
    
    fn fan_to_triangles(&self) -> Vec<[f32; 2]> {
        let mut result = Vec::new();
        for i in 2..self.vertices.len() {
            result.push(self.vertices[0]);      // Center vertex
            result.push(self.vertices[i - 1]);
            result.push(self.vertices[i]);
        }
        result
    }
}
```

**GPU vertex struct:**
```rust
// navcore-render/src/vertex.rs

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct AreaVertex {
    pub position: [f32; 2],
    pub color: [f32; 4],
}

/// Convert pre-triangulated areas to GPU vertex buffer
pub fn areas_to_vertices(areas: &[AreaGeometry], color_fn: impl Fn(f32) -> [f32; 4]) -> Vec<AreaVertex> {
    let mut vertices = Vec::new();
    
    for area in areas {
        let color = color_fn(area.depth);
        for tri in &area.triangles {
            for pos in tri.to_triangles() {
                vertices.push(AreaVertex { position: pos, color });
            }
        }
    }
    
    vertices
}
```

### 2.5 Hardcoded Colors (Nautograf Style)

**Ref:** Nautograf analysis §6 — no S-52 palettes, just constants

**From `tessellator.cpp:12–26`:**
```cpp
static const QColor landAreaColor(255, 240, 190);
static const QColor criticalWaterColor(165, 202, 159);
static const QColor depthContourColor(110, 190, 230);
```

**Rust equivalent:**
```rust
// navcore-render/src/colors.rs

/// Hardcoded colors for First Light (Nautograf style)
pub const LAND: [f32; 4] = [1.0, 0.94, 0.75, 1.0];           // RGB(255, 240, 190)
pub const CRITICAL_WATER: [f32; 4] = [0.65, 0.79, 0.62, 1.0]; // RGB(165, 202, 159)
pub const DEPTH_CONTOUR: [f32; 4] = [0.43, 0.75, 0.90, 1.0];  // RGB(110, 190, 230)

/// Depth area color — exact Nautograf formula from tessellator.cpp:599-614
pub fn depth_color(depth: f32) -> [f32; 4] {
    if depth < 0.5 {
        return CRITICAL_WATER;  // Greenish for very shallow
    }
    
    // Logarithmic gradient: deeper = lighter (more white)
    // factor ∈ [0, 50], clamped
    let factor = (30.0 - 30.0 * depth.log10() + 20.0).clamp(0.0, 50.0);
    
    [
        (255.0 - 2.0 * factor) / 255.0,   // R: 155-255
        (255.0 - 0.6 * factor) / 255.0,   // G: 225-255  
        (255.0 - 0.2 * factor) / 255.0,   // B: 245-255
        1.0,
    ]
    // depth=1m  → factor≈50 → RGB(155,225,245) — blue
    // depth=5m  → factor≈29 → RGB(197,237,249) — paler
    // depth=20m → factor≈9  → RGB(237,249,254) — almost white
}
```

**What we are NOT implementing for First Light:**
- ❌ S-52 color tokens (DEPVS, DEPMS, LANDA, etc.)
- ❌ Day/Dusk/Night palettes
- ❌ User-configurable safety depth
- ❌ DEPARE02 conditional symbology
- ❌ Line patterns (solid only)
- ❌ Symbol rendering
- ❌ Text labels

**Why this is OK:** Nautograf achieves 60fps on Pi with exactly this approach.

### 2.6 Minimal Shader

```wgsl
// assets/shaders/area.wgsl

struct Uniforms {
    view_proj: mat4x4<f32>,
}

@group(0) @binding(0)
var<uniform> uniforms: Uniforms;

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) color: vec4<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
}

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = uniforms.view_proj * vec4<f32>(in.position, 0.0, 1.0);
    out.color = in.color;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
```

### 2.7 Frame Stats (Instrumentation)

Per consultant advice, add early:

```rust
// navcore-render/src/stats.rs

pub struct FrameStats {
    pub frame_times: VecDeque<f32>,  // Last 60 frames
    pub vertex_count: u32,
    pub draw_calls: u32,
}

impl FrameStats {
    pub fn log_summary(&self) {
        let avg = self.frame_times.iter().sum::<f32>() / self.frame_times.len() as f32;
        let max = self.frame_times.iter().cloned().fold(0.0, f32::max);
        tracing::info!(
            "FPS: {:.1}, frame_avg: {:.2}ms, frame_max: {:.2}ms, vertices: {}, draws: {}",
            1000.0 / avg, avg, max, self.vertex_count, self.draw_calls
        );
    }
}
```

### 2.8 M1 Exit Criteria

- [ ] LNDARE polygons render as tan
- [ ] DEPARE polygons render with depth gradient
- [ ] Pan works (mouse drag / touch)
- [ ] Zoom works (scroll wheel / pinch)
- [ ] 60 fps on Mac, ≥50 fps on Pi 4
- [ ] Screenshot visually matches OpenCPN/QuteNav (rough shapes align)

---

## Phase 3: Full Chart Viewer (M2)

After First Light proves the pipeline, add remaining features:

### 3.1 Additional Feature Types

| Feature | Geometry | Style (Hardcoded) |
|---------|----------|-------------------|
| LNDARE | Area | Tan fill |
| DEPARE | Area | Blue gradient |
| DEPCNT | Line | Blue, 1.5px |
| COALNE | Line | Brown, 2px |
| BOYLAT | Point | Symbol from atlas |
| BCNLAT | Point | Symbol from atlas |
| UWTROC | Point | Symbol |
| SOUNDG | Point | Text label |

### 3.2 Line Rendering

**From Nautograf analysis §5:**
> Nautograf does NOT implement S-52 line patterns. Only solid lines with variable width.

**Line width categories (from `materialcreator.cpp:67-86`):**
- Thin: 1.5px
- Medium: 2.5px  
- Thick: 4.0px

```rust
// navcore-render/src/tessellate.rs

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct LineVertex {
    pub position: [f32; 2],
    pub normal: [f32; 2],   // Perpendicular to line direction
    pub miter: f32,         // Miter length factor
    pub color: [f32; 4],
}

pub fn tessellate_line(
    points: &[glam::Vec2],
    width: f32,
    color: [f32; 4],
) -> (Vec<LineVertex>, Vec<u32>) {
    // Generate quad strip: 6 vertices per segment (2 triangles)
    // Shader expands vertices ±width/2 along normal
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    
    for i in 1..points.len() {
        let prev = points[i - 1];
        let curr = points[i];
        let dir = (curr - prev).normalize();
        let normal = glam::vec2(-dir.y, dir.x);
        
        // Emit 4 vertices forming a quad
        // ... (miter calculation for joins)
    }
    
    (vertices, indices)
}
```

**Line shader (WGSL):**
```wgsl
// assets/shaders/line.wgsl

struct Uniforms {
    view_proj: mat4x4<f32>,
    line_width: f32,
}

@group(0) @binding(0) var<uniform> uniforms: Uniforms;

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) normal: vec2<f32>,
    @location(2) miter: f32,
    @location(3) color: vec4<f32>,
}

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    // Expand position along normal by half width
    let offset = in.normal * in.miter * uniforms.line_width * 0.5;
    let pos = in.position + offset;
    
    var out: VertexOutput;
    out.clip_position = uniforms.view_proj * vec4<f32>(pos, 0.0, 1.0);
    out.color = in.color;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;  // Solid color for M1/M2
}
```

**Dash Patterns (Add Post-MVP):**

QuteNav uses a clever 18-bit bitmask approach in the fragment shader. This is better than CPU segment splitting:

```wgsl
// Future: GPU dash patterns from QuteNav [chartpainter-lines.frag]
struct LineUniforms {
    view_proj: mat4x4<f32>,
    line_width: f32,
    pattern: u32,        // 18-bit bitmask
    segment_len: f32,    // Pattern repeat length in world units
}

// Pattern constants
const SOLID: u32  = 0x3ffffu;  // 111111111111111111
const DASHED: u32 = 0x3ffc0u;  // 111111111111000000 (10 on, 6 off, 2 on)
const DOTTED: u32 = 0x30c30u;  // 110000110000110000 (2 on, 4 off, repeat)

@fragment
fn fs_main_with_pattern(in: VertexOutput) -> @location(0) vec4<f32> {
    if uniforms.pattern != SOLID {
        let s = in.distance % uniforms.segment_len;
        let bit = u32(18.0 * s / uniforms.segment_len);
        if (uniforms.pattern & (1u << bit)) == 0u {
            discard;
        }
    }
    return in.color;
}
```

**Advantage:** Pattern evaluated in fragment shader. No CPU geometry splitting. Requires `distance` (accumulated line length) passed from vertex shader.

**What we're NOT implementing in M1/M2:**
- ❌ Dash patterns (defer to post-MVP using QuteNav approach above)
- ❌ Line caps (butt only)
- ❌ Proper miter limits

### 3.3 Symbol Atlas (Fixed Grid)

**From Nautograf analysis §4:**
> Simple row-based packing (no fancy bin packing). Atlas 300×300 grayscale.
> Uses MSDF (Multi-channel Signed Distance Field) for sharp scaling.

**NavCore approach — even simpler:**
- Fixed 16×16 grid = 256 symbol slots
- Each cell 64×64 pixels = 1024×1024 atlas total
- Pre-baked PNG (not runtime MSDF generation)
- JSON metadata for UV coordinates and pivots

**Symbol Pivot is Critical (from QuteNav [chartsymbols.xml]):**

```xml
<!-- Example from QuteNav chartsymbols.xml -->
<symbol RCID="1234" name="BOYLAT13">
  <bitmap>
    <graphics-location x="512" y="256" width="32" height="40"/>
    <pivot x="16" y="36"/>  <!-- 4px from bottom = buoy anchor point -->
  </bitmap>
</symbol>
```

| Symbol Type | Pivot Location | Why |
|-------------|----------------|-----|
| Buoys | Bottom center | Buoy anchors to water position |
| Beacons | Bottom center | Structure base at position |
| Lights | Center | Light position is the center |
| Rocks | Center | Hazard at center point |

**Getting pivot wrong = symbols float above or sink below their map position.**

```rust
// navcore-render/src/symbols.rs

pub struct SymbolAtlas {
    texture: wgpu::Texture,
    texture_view: wgpu::TextureView,
    sampler: wgpu::Sampler,
    symbols: HashMap<&'static str, SymbolDef>,
}

pub struct SymbolDef {
    pub uv: [f32; 4],      // [u_min, v_min, u_max, v_max] normalized
    pub size: [f32; 2],    // Pixel dimensions
    pub pivot: [f32; 2],   // Anchor point in pixels from top-left
}

impl SymbolAtlas {
    pub fn load(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        // Load pre-baked assets/symbols/atlas.png
        // Parse assets/symbols/atlas.json for metadata
        let bytes = include_bytes!("../../assets/symbols/atlas.png");
        let image = image::load_from_memory(bytes).unwrap();
        // Create wgpu::Texture...
    }
    
    pub fn get(&self, name: &str) -> Option<&SymbolDef> {
        self.symbols.get(name)
    }
}
```

**Example atlas.json:**
```json
{
  "BOYLAT_PORT": {
    "uv": [0.0, 0.0, 0.0625, 0.0625],
    "size": [64, 64],
    "pivot": [0.5, 1.0]
  },
  "BOYLAT_STBD": {
    "uv": [0.0625, 0.0, 0.125, 0.0625],
    "size": [64, 64],
    "pivot": [0.5, 1.0]
  }
}
```

**Pivot points matter:**
- Buoys: bottom-center (0.5, 1.0) — anchored at waterline
- Lights: center (0.5, 0.5)
- Beacons: bottom-center

**MSDF vs Bitmap decision:**
- MSDF: Sharp at any zoom, smaller atlas, more complex shader
- Bitmap: Simpler, but needs mipmaps or looks blurry at small sizes

**For MVP:** Start with bitmap (PNG). Consider MSDF later if scaling artifacts appear.
```

### 3.4 Instance Rendering for Symbols

**Ref:** wgpu_ref_notes §6 - use instancing for repeated elements

```rust
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct SymbolInstance {
    pub position: [f32; 2],    // World coords
    pub uv_offset: [f32; 2],
    pub uv_size: [f32; 2],
    pub rotation: f32,
    pub scale: f32,
}

// One draw call for all symbols of same type
render_pass.draw(0..6, 0..instance_count);  // 6 vertices per quad
```

### 3.5 In-Memory Tile Cache

**Ref:** Consultant feedback - defer disk cache

```rust
// navcore-render/src/cache.rs

pub struct TileCache {
    tiles: HashMap<TileKey, TileData>,
    lru: VecDeque<TileKey>,
    memory_used: usize,
    memory_limit: usize,  // 256 MB for Pi
}

pub struct TileData {
    pub area_buffer: wgpu::Buffer,
    pub area_indices: wgpu::Buffer,
    pub line_buffer: wgpu::Buffer,
    pub line_indices: wgpu::Buffer,
    pub symbols: wgpu::Buffer,
    pub memory_size: usize,
}

impl TileCache {
    pub fn get_or_create(&mut self, key: TileKey, chart: &ChartData) -> &TileData {
        // LRU eviction when memory_used > memory_limit
    }
}
```

**Disk persistence (rkyv) added later if needed.**

### 3.6 Gestures

```rust
// navcore-app/src/input.rs

pub enum Gesture {
    Pan { delta: glam::Vec2 },
    Zoom { center: glam::Vec2, factor: f32 },
    Tap { position: glam::Vec2 },
}

pub struct GestureRecognizer {
    // Handle mouse + touch uniformly
}

// Inertial pan (optional, add if time permits)
pub struct InertiaAnimator {
    velocity: glam::Vec2,
    friction: f32,
}
```

### 3.7 M2 Exit Criteria

- [ ] All major feature types render
- [ ] Lines render (solid, correct width)
- [ ] Symbols render from atlas
- [ ] Pinch-zoom feels natural
- [ ] Inertial pan (nice to have)
- [ ] 60 fps maintained on Pi 4 with full chart
- [ ] Memory stays under 256 MB

---

## Phase 4: Signal K Overlay (M3)

### 4.1 Signal K Client

```rust
// navcore-app/src/signalk.rs

use tokio_tungstenite::connect_async;

pub struct SignalKClient {
    url: String,
}

pub enum Update {
    Position { lat: f64, lon: f64 },
    Heading(f64),
    COG(f64),
    SOG(f64),
    Ais(AisTarget),
}

impl SignalKClient {
    pub async fn connect(&self, tx: mpsc::Sender<Update>) -> Result<()> {
        let (ws, _) = connect_async(&self.url).await?;
        // Parse delta messages, send updates
    }
}
```

### 4.2 Own-Ship Overlay

```rust
// navcore-app/src/overlay.rs

pub struct OwnShip {
    position: Option<(f64, f64)>,
    heading: Option<f64>,
    cog: Option<f64>,
    sog: Option<f64>,
}

impl OwnShip {
    pub fn render(&self, render_pass: &mut wgpu::RenderPass, projection: &MercatorProjection) {
        // Draw ship icon at position
        // Draw heading line
        // Draw COG vector
    }
}
```

### 4.3 M3 Exit Criteria

- [ ] Connects to Signal K server
- [ ] Own-ship position updates in < 100ms
- [ ] Ship icon visible at correct position
- [ ] Heading indicator rotates correctly

---

## Phase 5: AIS Layer (M4)

### 5.1 AIS Target Management

```rust
pub struct AisLayer {
    targets: HashMap<u32, AisTarget>,
    instance_buffer: wgpu::Buffer,
}

pub struct AisTarget {
    pub mmsi: u32,
    pub lat: f64,
    pub lon: f64,
    pub cog: f64,
    pub sog: f64,
    pub name: Option<String>,
    pub ship_type: u8,
    pub last_update: Instant,
}
```

### 5.2 Basic Declutter

**Ref:** Nautograf analysis §7 - zoom-based visibility

```rust
pub fn declutter(&mut self, viewport: Rect, zoom: f32) {
    // Simple grid-based: one target per cell
    // Assign min_zoom based on importance/overlap
}
```

### 5.3 M4 Exit Criteria

- [ ] AIS targets from Signal K render
- [ ] 200 targets at ≥30 fps
- [ ] Overlapping targets decluttered
- [ ] Expired targets (>5 min) removed

---

## Phase 6: Packaging (M5)

### 6.1 Configuration

```toml
# /etc/navcore/config.toml
[charts]
directory = "/home/navcore/charts"

[display]
fullscreen = true
vsync = true

[signalk]
url = "ws://localhost:3000/signalk/v1/stream"
```

### 6.2 Systemd Service

```ini
[Unit]
Description=NavCore Chart Plotter
After=graphical.target signalk.service

[Service]
Type=simple
User=navcore
ExecStart=/usr/bin/navcore
Restart=always

[Install]
WantedBy=graphical.target
```

### 6.3 Pi-gen Image

- Auto-start NavCore on boot
- Disable screen blanking
- Pre-install oexserverd

### 6.4 M5 Exit Criteria

- [ ] .deb installs on Raspberry Pi OS
- [ ] Cold boot to chart < 30 seconds
- [ ] Kiosk mode works

---

## Appendix: Full S-52 (Post-MVP)

**From Nautograf analysis §3:**
> Nautograf does NOT implement S-52 conditional symbology procedures.
> No DEPARE02, DEPCNT02, LIGHTS05, or other CS functions exist.

### A.1 What Nautograf is Missing (vs S-52 Compliance)

| Feature | S-52 Requirement | Nautograf Status | NavCore Priority |
|---------|------------------|------------------|------------------|
| Conditional symbology | Full CS procedures | ❌ Not implemented | Post-MVP |
| Safety contour | User-configurable | ❌ Hardcoded 0.5m | High (safety) |
| Color palettes | Day/Dusk/Night | ❌ Day only | Medium |
| Line patterns | SOLD/DASH/DOTT/etc | ❌ SOLD only | Low |
| Text decluttering | Priority-based | ⚠️ Basic zoom sweep | Medium |
| Symbol rotation | Dynamic | ❌ Fixed orientation | Low |
| Area patterns | Hatching/stipple | ❌ Solid fill only | Low |
| Light sectors | Arc rendering | ❌ Not implemented | Medium |

### A.2 Conditional Symbology Procedures

These must be implemented for certification:

| Procedure | Purpose | Complexity | Source |
|-----------|---------|------------|--------|
| **DEPARE02** | Safety depth area fill | Medium | OpenCPN [s52cnsy.cpp:607-670] |
| **DEPCNT02** | Safety contour emphasis | Medium | OpenCPN [s52cnsy.cpp:704-810] |
| **LIGHTS05** | Light sector arcs | High | OpenCPN [s52cnsy.cpp:1200-1500] |
| **RESARE02** | Restricted area symbology | Low | OpenCPN [s52cnsy.cpp:1950-2050] |
| **OBSTRN04** | Obstruction symbols | Medium | OpenCPN [s52cnsy.cpp:520-600] |
| **WRECKS02** | Wreck symbols | Medium | OpenCPN [s52cnsy.cpp:3238-3410] |

**OpenCPN CS Return Strings (Actual Format from [s52cnsy.cpp]):**

```rust
// CS procedures return instruction strings like:
// "AC(DEPMS);LS(SOLD,1,DEPSC)"  
//  ^^area fill  ^^line style
// "SY(ISODGR01)"
//  ^^symbol

enum S52Instruction {
    AC(ColorToken),                    // Area Color fill
    AP(PatternName),                   // Area Pattern fill  
    LS(LineStyle, Width, ColorToken),  // Line Simple
    LC(LineName),                      // Line Complex (pattern)
    SY(SymbolName),                    // Symbol
    TX(Text),                          // Text
    TE(TextExtended),                  // Text with formatting
    CS(ProcedureName),                 // Call another CS procedure
}
```

**Safety Contour Selection (from OpenCPN [s52cnsy.cpp:704-810]):**
```rust
/// Select the safety contour from available chart contours
/// Algorithm: exact match OR next deeper if exact not available
fn select_safety_contour(chart_contours: &[f64], safety_depth: f64) -> f64 {
    // First: look for exact match
    if chart_contours.contains(&safety_depth) {
        return safety_depth;
    }
    // Second: find next deeper contour
    chart_contours.iter()
        .filter(|&&c| c > safety_depth)
        .min_by(|a, b| a.partial_cmp(b).unwrap())
        .copied()
        .unwrap_or(safety_depth)  // Fallback if no deeper exists
}
```

**DEPARE02 Implementation (from OpenCPN [s52cnsy.cpp:607-670]):**
```rust
/// Returns S-52 instruction string for area rendering
fn cs_depare02(
    drval1: Option<f64>,  // Min depth (DRVAL1 attribute)
    drval2: Option<f64>,  // Max depth (DRVAL2 attribute)
    settings: &MarinerSettings,
) -> &'static str {
    let min_d = drval1.unwrap_or(0.0);
    let max_d = drval2.unwrap_or(f64::MAX);
    let safety = settings.effective_safety_depth();
    let shallow = settings.shallow_contour;
    let deep = settings.deep_contour;
    
    // OpenCPN's exact logic from s52cnsy.cpp:607-670
    if max_d <= safety {
        // Entire area shallower than safety — DANGER
        "AC(DEPIT);LS(SOLD,1,DEPCN)"  // Green fill, thin contour
    } else if min_d < safety && max_d > safety {
        // Area spans safety threshold — CAUTION  
        "AC(DEPMS);AP(DIAMOND1);LS(SOLD,1,DEPSC)"  // Blue + pattern
    } else if min_d < deep {
        // Medium depth
        "AC(DEPMD);LS(SOLD,1,DEPSC)"  // Medium blue
    } else {
        // Deep water
        "AC(DEPDW);LS(SOLD,1,DEPCN)"  // Dark blue
    }
}
```

**DEPCNT02 Implementation (from OpenCPN [s52cnsy.cpp:704-810]):**
```rust
/// Returns S-52 instruction string for contour line rendering
fn cs_depcnt02(
    valdco: f64,           // Contour depth value (VALDCO attribute)
    safety_contour: f64,   // Selected safety contour for chart
) -> &'static str {
    if (valdco - safety_contour).abs() < 0.01 {
        // IS the safety contour — make it PROMINENT
        "LS(SOLD,2,DEPSC)"  // Width 2 (thick), safety contour color
    } else {
        // Regular depth contour
        "LS(SOLD,1,DEPCN)"  // Width 1 (normal), contour color
    }
}
```

### A.3 Mariner Settings

```rust
pub struct MarinerSettings {
    pub vessel_draft: f32,      // meters (e.g., 1.5)
    pub safety_margin: f32,     // meters (e.g., 0.5)
    pub shallow_contour: f32,   // meters (e.g., 2.0)
    pub safety_contour: f32,    // meters (e.g., 5.0)
    pub deep_contour: f32,      // meters (e.g., 30.0)
}

impl MarinerSettings {
    pub fn effective_safety_depth(&self) -> f32 {
        self.vessel_draft + self.safety_margin
    }
}
```

### A.4 Day/Dusk/Night Palettes

**From QuteNav [chartsymbols.xml:3-336]:** ~50 color tokens, each with 3 variants.

```rust
pub enum PaletteMode { Day, Dusk, Night }

pub struct S52Palette {
    mode: PaletteMode,
    colors: HashMap<&'static str, [[u8; 3]; 3]>,  // [day, dusk, night]
}

/// Complete S-52 color table from QuteNav chartsymbols.xml
const S52_COLORS: &[(&str, [[u8; 3]; 3])] = &[
    // Depth colors
    ("DEPSC", [[82,90,92],     [82,90,92],     [41,45,46]]),      // Safety contour
    ("DEPVS", [[255,204,204],  [255,204,204],  [127,102,102]]),   // Very shallow (danger pink)
    ("DEPDW", [[176,226,255],  [176,226,255],  [88,113,127]]),    // Deep water
    
    // Land and built-up
    ("LANDA", [[204,204,153],  [153,153,102],  [102,102,51]]),    // Land area
    ("CHBLK", [[7,7,7],        [7,7,7],        [7,7,7]]),         // Chart black
    ("CHGRD", [[204,204,204],  [153,153,153],  [38,38,38]]),      // Chart gray dark
    
    // Lights  
    ("LITRD", [[255,0,0],      [255,85,85],    [127,0,0]]),       // Red light
    ("LITGN", [[0,255,0],      [85,255,85],    [0,127,0]]),       // Green light
    ("LITYW", [[255,255,0],    [255,255,85],   [127,127,0]]),     // Yellow light
    
    // Navigation
    ("RESBL", [[0,0,255],      [85,85,255],    [0,0,127]]),       // Restricted blue
    ("CHMGD", [[255,0,255],    [255,85,255],   [127,0,127]]),     // Magenta
    
    // ... ~40 more tokens
];

impl S52Palette {
    pub fn get(&self, token: &str) -> [u8; 3] {
        let entry = self.colors.get(token).expect("Unknown color token");
        match self.mode {
            PaletteMode::Day => entry[0],
            PaletteMode::Dusk => entry[1], 
            PaletteMode::Night => entry[2],
        }
    }
    
    pub fn set_mode(&mut self, mode: PaletteMode) {
        self.mode = mode;
        // Triggers re-render (update uniform buffer or re-tessellate)
    }
}
```

**Night mode critical:** Reduced brightness helps mariner's dark adaptation. Red/green lights are dimmed by ~50% to avoid night blindness.

**GPU approach (avoid re-tessellation on palette switch):**
```wgsl
@group(0) @binding(1) var<uniform> palette: array<vec3<f32>, 50>;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let color = palette[in.color_index];
    return vec4<f32>(color, 1.0);
}
```

### A.5 Line Patterns (Post-MVP)

**Two implementation approaches from reference projects:**

| Approach | Source | Method |
|----------|--------|--------|
| **Pixel Array** | OpenCPN [s52plib.cpp:4750] | `[6.0, 2.0]` = 6px on, 2px off |
| **18-bit Bitmask** | QuteNav [chartpainter-lines.frag] | `0x3ffc0` = 111111111111000000 |

**OpenCPN pixel arrays:**
| Style | S-52 Code | Pixels [on, off, ...] |
|-------|-----------|----------------------|
| SOLD | 1 | `[]` (solid) |
| DASH | 2 | `[6.0, 2.0]` |
| DOTT | 3 | `[1.0, 2.0]` |
| DASHDOT | 4 | `[6.0, 2.0, 1.0, 2.0]` |

**QuteNav 18-bit bitmask (GPU-native):**
```wgsl
const SOLID: u32  = 0x3ffffu;  // 111111111111111111 (all on)
const DASHED: u32 = 0x3ffc0u;  // 111111111111000000 (75% on)
const DOTTED: u32 = 0x30c30u;  // 110000110000110000 (33% on)

@fragment
fn fs_line(in: VertexOutput) -> @location(0) vec4<f32> {
    if uniforms.pattern != SOLID {
        let s = in.distance % uniforms.segment_len;
        let bit = u32(18.0 * s / uniforms.segment_len);
        if (uniforms.pattern & (1u << bit)) == 0u {
            discard;  // ⚠️ discard hurts tile GPU performance
        }
    }
    return in.color;
}
```

**Recommendation for NavCore:** Start with solid lines only. If patterns needed:
1. Pixel array approach is simpler to implement
2. 18-bit bitmask is more GPU-efficient but uses `discard`
3. Best: alpha mask in fragment shader instead of `discard`

### A.6 Text Decluttering (NavCore Must Build)

**Critical finding:** NEITHER reference project has proper label collision detection:

| Project | Status | Notes |
|---------|--------|-------|
| **Nautograf** | Basic zoom-sweep | Each label tagged with `min_zoom` threshold |
| **QuteNav** | **No collision detection** | Labels drawn in z-order, may overlap |
| **OpenCPN** | **Priority-based rect collision** | [s52plib.cpp:2310-2320] |

**OpenCPN approach [s52plib.cpp:2310-2320]:**
```rust
// Per frame: clear collision list, process labels by priority
fn CheckTextRectList(test_rect: Rect, priority: u8, placed: &[PlacedText]) -> bool {
    for existing in placed {
        if test_rect.intersects(&existing.rect) {
            // Higher priority (lower number) wins
            if priority < existing.priority {
                return false;  // Allow overlap, we win
            }
            return true;  // Collision, skip this label
        }
    }
    false  // No collision
}
```

**NavCore must implement proper decluttering:**

```rust
pub fn declutter_labels(labels: &mut [Label], grid_size: f32) {
    // Sort by priority — lower number = more important = placed first
    labels.sort_by_key(|l| l.priority);
    
    // Spatial hash grid for O(1) collision checks
    let mut grid: HashMap<(i32, i32), Vec<Rect>> = HashMap::new();
    
    for label in labels.iter_mut() {
        let cell = (
            (label.center.x / grid_size) as i32,
            (label.center.y / grid_size) as i32,
        );
        
        // Check 3x3 neighborhood for overlaps
        let overlaps = (-1..=1).any(|dx| (-1..=1).any(|dy| {
            grid.get(&(cell.0 + dx, cell.1 + dy))
                .map(|rects| rects.iter().any(|r| r.intersects(&label.bbox.expand(4.0))))
                .unwrap_or(false)
        }));
        
        if !overlaps {
            grid.entry(cell).or_default().push(label.bbox);
            label.visible = true;
        } else {
            label.visible = false;
        }
    }
}
```

**Zoom-based visibility (Nautograf approach for fallback):**
```rust
// Store min_zoom in vertex, shader discards if current_zoom < min_zoom
@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    if uniforms.current_zoom < in.min_zoom {
        discard;
    }
    return textureSample(glyph_atlas, glyph_sampler, in.uv);
}
```

---

**Conclusion:** Nautograf is a **technology demonstrator**, not an S-52 compliant renderer.
NavCore must implement these features for certification and safe navigation use.
However, for MVP (M0-M2), hardcoded Nautograf-style rendering is sufficient to prove the architecture.

---

## Timeline Summary

| Week | Milestone | Deliverable |
|------|-----------|-------------|
| 1-2 | M0 | CLI decrypts, parser extracts LNDARE/DEPARE |
| 3 | M1 (start) | Test triangle in WGPU on Mac + Pi |
| 4 | M1 (done) | **First Light: pan/zoom chart at 60fps** |
| 5-6 | M2 | Lines, symbols, full chart |
| 7-8 | M2 | Gestures, tile cache, polish |
| 9-10 | M3 | Signal K own-ship |
| 11-12 | M4 | AIS layer |
| 13-14 | M5 | Packaging, Pi-gen image |

---

## Verification Checkpoints

### Week 4 (M1 Complete)

You should be able to:
- Load one SENC
- See tan land and blue water
- Pan and zoom smoothly
- Take a screenshot that roughly matches QuteNav

If not, **stop and debug** before proceeding.

### Before M3

These must be true:
- [ ] Pre-triangulated areas render correctly (just read from file)
- [ ] Camera projection roundtrip passes unit tests
- [ ] Single chart renders at 60fps on Pi

**Note:** Polygon holes are already handled in OSENC triangulation — no special code needed.

---

## Research Still Needed

**Status after Nautograf + QuteNav + OpenCPN analysis:**

| Topic | Status | Source | Notes |
|-------|--------|--------|-------|
| **oexserverd socket protocol** | ❓ **BLOCKER** | o-charts plugin source | Must investigate Week 1 |
| **oesenc library availability** | ❓ Unknown | github.com/hornang/oesenc | Can we use it? FFI or port? |
| **Edge direction encoding** | ✅ **ANSWERED** | QuteNav [osenc.cpp:458-462] | Version-dependent (see below) |
| **SENC version** | ✅ **ANSWERED** | OpenCPN [s52s57.h:38] | **201** (uint16_t) |
| **Pre-triangulated areas?** | ✅ **CONFIRMED** | OpenCPN + QuteNav | **YES, ALWAYS** — skip earcut |
| **SENC record 96-97 format** | ✅ **ANSWERED** | QuteNav + OpenCPN | Edge topology documented |
| **Complete S-52 color palette** | ✅ **ANSWERED** | QuteNav + OpenCPN | ~50 tokens in Appendix A.4 |
| **Safety contour procedures** | ✅ **ANSWERED** | All three projects | DEPARE02, DEPCNT02 pseudocode |
| **Line pattern encoding** | ✅ **ANSWERED** | QuteNav + OpenCPN | Two approaches documented |
| **Symbol pivot format** | ✅ **ANSWERED** | QuteNav + OpenCPN | XML structure and semantics |
| **Text decluttering** | ⚠️ **Partial** | OpenCPN [s52plib.cpp:2310] | Priority-based collision |

### Edge Direction Encoding (CONFIRMED)

**From QuteNav [osenc.cpp:381-384, 458-462]:**

The encoding is **version-dependent**:

| SENC Version | Format | How to Detect Reversed |
|--------------|--------|------------------------|
| **≤ 200** | Signed i32 | `edge_index < 0` |
| **> 200 (201)** | u32 + flag | Separate `reversed` field |

```rust
/// Version-aware edge reference parsing
fn parse_edge_ref(data: &[i32], version: u16) -> EdgeRef {
    if version > 200 {
        // New format: [begin, index, end, reversed_flag]
        EdgeRef {
            begin_node: data[0] as u32,
            edge_index: data[1].unsigned_abs(),
            end_node: data[2] as u32,
            reversed: data[3] != 0,  // Explicit flag
        }
    } else {
        // Old format: [begin, signed_index, end]
        EdgeRef {
            begin_node: data[0] as u32,
            edge_index: data[1].unsigned_abs(),
            end_node: data[2] as u32,
            reversed: data[1] < 0,   // Sign bit
        }
    }
}
```

**Critical from QuteNav [osenc.cpp:716-718]:**
> "OSENC does not reverse begin and end like other formats, so do it here"

When `reversed=true`, you must:
1. **Swap begin/end nodes** (in code, not file)
2. **Reverse point iteration order**

### Pre-Triangulated Areas (CONFIRMED)

**From OpenCPN [Osenc.h:193-203] + user confirmation:**

> **OSENC areas ALWAYS have `triprim_count > 0`.**
> No runtime tessellation needed. Just read triangles from file.

Triangle primitive types:
- `0x04` = GL_TRIANGLES (plain triangles)
- `0x05` = GL_TRIANGLE_STRIP (convert to triangles)
- `0x06` = GL_TRIANGLE_FAN (convert to triangles)

**NavCore can skip earcut entirely for oeSENC charts.**

### Remaining Blockers

Only **TWO** blockers remain:

1. **oexserverd protocol** — Must reverse-engineer or find documentation
2. **oesenc licensing** — Can we use/redistribute it?

| # | Question | How to Check | Impact if Wrong |
|---|----------|--------------|-----------------|
| 1 | **Edge direction encoding** | Check if edge_index in record type 82 can be negative | Parser bug → broken polygons |
| 2 | **SENC version** | Read first 4 bytes after decryption | Wrong record formats |
| 3 | **Pre-triangulated?** | Look for `triprim_count` field in geometry records | If yes, skip earcut entirely |
| 4 | **Connector segments needed?** | Try rendering without CE/EC/CC segments | Small gaps at line joints |
| 5 | **Winding order** | Check if rings are CCW outer, CW holes | Earcut will fail or produce inside-out |

### Edge Encoding Difference

**OpenCPN [s57chart.cpp:982]:**
```c++
if (edge_id < 0) edge_dir = reverse;  // Signed integer
```

**QuteNav [osenc.cpp:720]:**
```c++
bool reversed = edge_ref.reversed;  // Separate flag
```

**NavCore parser should handle both:**
```rust
pub struct EdgeRef {
    pub edge_index: i32,  // Might be signed!
    pub reversed: bool,   // Or might use this flag
}

impl EdgeRef {
    pub fn is_reversed(&self) -> bool {
        self.edge_index < 0 || self.reversed
    }
    
    pub fn index(&self) -> u32 {
        self.edge_index.unsigned_abs()
    }
}
```

**Remaining blockers:**
1. **oexserverd protocol** — Must reverse-engineer or find documentation
2. **oesenc licensing** — Can we use/redistribute it?

### oesenc Library Decision

**Option A: Use oesenc via FFI**
- Pros: Polygon assembly already done, battle-tested
- Cons: C++ dependency, cross-compilation complexity

**Option B: Port oesenc to Rust**
- Pros: Pure Rust, no FFI overhead
- Cons: Significant effort, risk of bugs

**Option C: Implement SENC parser from scratch**
- Pros: Full control, learn the format
- Cons: Must handle edge topology (records 96-97)

**Recommendation:** Start with Option C for header + basic geometry. 
If edge topology proves too complex, investigate Option A.

### Edge Topology (Only if oesenc unavailable)

From OpenCPN `s57chart.cpp`, edge assembly algorithm:
1. Group edges by face ID
2. Order edges to form closed rings (adjacency graph walk)
3. Classify rings: outer vs holes (signed area test)
4. Handle degenerates (self-intersecting, duplicate vertices)

**Nautograf sidesteps this entirely** by using oesenc.

---

## Risk Mitigation

### High Priority Risks

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| **WGPU doesn't work on Pi** | Low | Critical | Test triangle in Week 3, before parsing complete |
| **Edge topology too complex** | Medium | High | Use oesenc if available; polygons pre-assembled |
| **S-52 scope creep** | High | High | Hardcoded colors until M2 complete. No exceptions. |
| **First pixels too late** | Medium | High | Reordered plan: GPU proof in Week 3-4 |

### Medium Priority Risks

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| **Memory pressure on Pi** | Medium | Medium | In-memory LRU first; disk cache only if needed |
| **Signal K before chart stable** | Low | Medium | M3 starts only after M2 verified at 60fps |
| **Projection/matrix bugs** | Medium | Medium | Unit tests for round-trip, visual comparison to QuteNav |
| **oexserverd protocol unknown** | Medium | Medium | Research in Week 1; fallback to file-based if needed |

### Low Priority Risks

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| **Symbol atlas too small** | Low | Low | 1024×1024 with 256 slots is plenty for MVP |
| **Line width rendering bugs** | Low | Low | Solid lines are simple; test on both platforms |

### Consultant-Identified Failure Modes (Addressed)

1. **"First pixels come too late"**
   - ✅ Fixed: M1 is now "First Light" with WGPU proof in Week 3-4

2. **"S-52 completeness trap"**
   - ✅ Fixed: Hardcoded Nautograf-style colors. Full S-52 is post-MVP appendix.

3. **"Tile cache complexity before necessity"**
   - ✅ Fixed: In-memory only. rkyv disk cache is deferred.

4. **"Too much logic in one crate layer"**
   - ⚠️ Partially addressed: 4 crates with clear boundaries. Monitor during M2.

5. **"Signal K before chart is rock solid"**
   - ✅ Fixed: M3 only starts after M2 verified at 60fps.

### Go/No-Go Checkpoints

**Week 3 Checkpoint (Test Triangle):**
- [ ] WGPU initializes on Mac
- [ ] WGPU initializes on Pi 4
- [ ] Colored triangle renders on both
- [ ] Window resize works without crash

**If failed:** Stop. Debug GPU setup before any more parsing work.

**Week 4 Checkpoint (First Light):**
- [ ] One SENC loads without error
- [ ] LNDARE polygons visible (tan)
- [ ] DEPARE polygons visible (blue gradient)
- [ ] Pan works (mouse drag)
- [ ] Zoom works (scroll wheel)
- [ ] ≥50 fps on Pi 4

**If failed:** Do NOT proceed to M2. Fix fundamentals first.

**Before M3 Checkpoint:**
- [ ] Full chart renders at 60fps on Pi
- [ ] Memory stays under 256 MB
- [ ] Polygon holes render correctly
- [ ] Screenshot roughly matches QuteNav for same chart

---

**End of Plan v2.0**
