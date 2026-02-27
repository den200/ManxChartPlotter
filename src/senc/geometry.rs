//! SENC geometry parsing.
//!
//! Key insight from navcore_plan_v2.md:
//! **OSENC areas ALWAYS contain pre-triangulated geometry.**
//! No runtime tessellation needed - just read triangles from file.
//!
//! IMPORTANT: OSENC vertices are in SM (Simple Mercator) coordinates
//! relative to the chart reference point (extent centroid).
//! SM coords can be used directly for rendering with camera at (0,0).
//! See doc/OSENC_FORMAT_REFERENCE.md for details.

use std::io::{Cursor, Read, Seek, SeekFrom};
use byteorder::{LittleEndian, ReadBytesExt};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum GeometryError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Invalid geometry: {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, GeometryError>;

/// Triangle primitive types from OpenCPN [Osenc.h]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TriPrimType {
    /// GL_TRIANGLES - plain triangles (3 vertices each)
    Triangles = 0x04,
    /// GL_TRIANGLE_STRIP - shared vertices
    TriangleStrip = 0x05,
    /// GL_TRIANGLE_FAN - fan from first vertex
    TriangleFan = 0x06,
}

impl TryFrom<u8> for TriPrimType {
    type Error = GeometryError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            0x04 => Ok(TriPrimType::Triangles),
            0x05 => Ok(TriPrimType::TriangleStrip),
            0x06 => Ok(TriPrimType::TriangleFan),
            _ => Err(GeometryError::Invalid(format!(
                "Unknown triangle primitive type: 0x{:02x}",
                value
            ))),
        }
    }
}

/// A triangle primitive from OSENC file
#[derive(Debug, Clone)]
pub struct TriPrim {
    pub prim_type: TriPrimType,
    /// Vertices as (x, y) pairs in Mercator meters (chart coordinates)
    pub vertices: Vec<[f32; 2]>,
}

impl TriPrim {
    /// Parse a triangle primitive from binary data
    /// prim_index is for diagnostic logging only
    pub fn read<R: Read>(reader: &mut R, prim_index: u32) -> Result<Self> {
        let prim_type_byte = reader.read_u8()?;
        let prim_type = TriPrimType::try_from(prim_type_byte)?;
        let nvert = reader.read_u32::<LittleEndian>()?;

        // Skip bounding box (4 × f64 = 32 bytes)
        let mut bbox = [0u8; 32];
        reader.read_exact(&mut bbox)?;

        // Read vertex data
        let mut vertices = Vec::with_capacity(nvert as usize);
        for _ in 0..nvert {
            let x = reader.read_f32::<LittleEndian>()?;
            let y = reader.read_f32::<LittleEndian>()?;
            vertices.push([x, y]);
        }

        // DEBUG: Check for corrupt data (NaN or extremely huge coordinates).
        //
        // Note: Many valid charts have SM coordinates tens/hundreds of km from the chart
        // reference; treat only truly extreme values as suspicious.
        let has_nan = vertices.iter().any(|v| v[0].is_nan() || v[1].is_nan());
        let has_huge = vertices
            .iter()
            .any(|v| v[0].abs() > 5_000_000.0 || v[1].abs() > 5_000_000.0);
        if has_nan || has_huge {
            eprintln!("WARN: Corrupt prim[{}] type=0x{:02x} nvert={} nan={} huge={}",
                prim_index, prim_type_byte, nvert, has_nan, has_huge);
            eprintln!("  first={:?} last={:?}", vertices.first(), vertices.last());
        }

        Ok(Self { prim_type, vertices })
    }

    /// Convert STRIP/FAN to plain triangles for GPU
    pub fn to_triangles(&self) -> Vec<[f32; 2]> {
        match self.prim_type {
            TriPrimType::Triangles => {
                // Plain triangles - just verify count is divisible by 3
                if self.vertices.len() % 3 != 0 {
                    eprintln!("WARN: Plain triangles has {} vertices (not divisible by 3)",
                        self.vertices.len());
                }
                self.vertices.clone()
            },
            TriPrimType::TriangleStrip => self.strip_to_triangles(),
            TriPrimType::TriangleFan => self.fan_to_triangles(),
        }
    }

    /// Convert triangle strip to plain triangles
    fn strip_to_triangles(&self) -> Vec<[f32; 2]> {
        if self.vertices.len() < 3 {
            return Vec::new();
        }

        let mut result = Vec::with_capacity((self.vertices.len() - 2) * 3);
        for i in 2..self.vertices.len() {
            // Alternate winding for consistent face orientation
            let (v0, v1, v2) = if i % 2 == 0 {
                (self.vertices[i - 2], self.vertices[i - 1], self.vertices[i])
            } else {
                (self.vertices[i - 1], self.vertices[i - 2], self.vertices[i])
            };
            result.push(v0);
            result.push(v1);
            result.push(v2);
        }
        result
    }

    /// Convert triangle fan to plain triangles
    fn fan_to_triangles(&self) -> Vec<[f32; 2]> {
        if self.vertices.len() < 3 {
            return Vec::new();
        }

        let mut result = Vec::with_capacity((self.vertices.len() - 2) * 3);
        for i in 2..self.vertices.len() {
            result.push(self.vertices[0]); // Center vertex
            result.push(self.vertices[i - 1]);
            result.push(self.vertices[i]);
        }
        result
    }

    /// Total number of triangles this primitive represents
    pub fn triangle_count(&self) -> usize {
        if self.vertices.len() < 3 {
            return 0;
        }
        match self.prim_type {
            TriPrimType::Triangles => self.vertices.len() / 3,
            TriPrimType::TriangleStrip | TriPrimType::TriangleFan => self.vertices.len() - 2,
        }
    }
}

/// Parsed area geometry with pre-triangulated data
#[derive(Debug, Clone)]
pub struct AreaGeometry {
    /// Bounding box
    pub extent: BBox,
    /// Pre-tessellated triangle primitives
    pub triangles: Vec<TriPrim>,
    /// Edge references for outlines (optional, for ring extraction)
    pub edge_refs: Vec<EdgeRef>,
}

/// Classifies the spatial relationship between a feature's bbox and a tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BBoxTileRelation {
    /// No overlap — skip entirely
    Outside,
    /// Partially overlaps — needs per-triangle clipping
    Intersects,
    /// All geometry guaranteed inside tile — skip clipping
    FullyInside,
}

/// Bounding box
#[derive(Debug, Clone, Copy, Default)]
pub struct BBox {
    /// West longitude (degrees)
    pub min_x: f64,
    /// East longitude (degrees)
    pub max_x: f64,
    /// South latitude (degrees)
    pub min_y: f64,
    /// North latitude (degrees)
    pub max_y: f64,
}

impl BBox {
    /// Classify the spatial relationship between this WGS84 bbox and tile bounds (Mercator meters).
    pub fn tile_relation(&self, tile: &crate::tiles::TileBounds) -> BBoxTileRelation {
        let (min_x_m, min_y_m) = crate::tiles::latlon_to_mercator(self.min_y, self.min_x);
        let (max_x_m, max_y_m) = crate::tiles::latlon_to_mercator(self.max_y, self.max_x);

        if max_x_m < tile.min_x || min_x_m > tile.max_x
            || max_y_m < tile.min_y || min_y_m > tile.max_y
        {
            BBoxTileRelation::Outside
        } else if min_x_m >= tile.min_x && max_x_m <= tile.max_x
               && min_y_m >= tile.min_y && max_y_m <= tile.max_y
        {
            BBoxTileRelation::FullyInside
        } else {
            BBoxTileRelation::Intersects
        }
    }

    /// Check if this WGS84 bounding box intersects tile bounds (global Mercator meters).
    pub fn intersects_tile(&self, tile: &crate::tiles::TileBounds) -> bool {
        self.tile_relation(tile) != BBoxTileRelation::Outside
    }
}

impl AreaGeometry {
    /// Parse area geometry from record payload (record type 82)
    ///
    /// Format from OpenCPN [Osenc.h:193-203]:
    /// - 4 × f64: extent (min_x, max_x, min_y, max_y)
    /// - u32: contour_count
    /// - u32: triprim_count (ALWAYS > 0 for areas)
    /// - u32: edge_count
    /// - [u32 × contour_count]: contour vertex counts (skipped for fill)
    /// - [TriPrim × triprim_count]: pre-triangulated data
    /// - [EdgeRef × edge_count]: edge references for outline (optional)
    ///
    /// v200: Each EdgeRef = 12 bytes; v201+: Each EdgeRef = 16 bytes (+ reversed flag)
    pub fn parse(payload: &[u8], senc_version: u16) -> Result<Self> {
        let mut cursor = Cursor::new(payload);

        // Read bounding box (WGS84 degrees): south_lat, north_lat, west_lon, east_lon
        let extent = BBox {
            min_y: cursor.read_f64::<LittleEndian>()?,
            max_y: cursor.read_f64::<LittleEndian>()?,
            min_x: cursor.read_f64::<LittleEndian>()?,
            max_x: cursor.read_f64::<LittleEndian>()?,
        };

        let contour_count = cursor.read_u32::<LittleEndian>()?;
        let triprim_count = cursor.read_u32::<LittleEndian>()?;
        let edge_count = cursor.read_u32::<LittleEndian>()?;

        // Skip contour counts (not needed for fill rendering)
        cursor.seek(SeekFrom::Current((contour_count * 4) as i64))?;

        // Verify offset to first TriPrim.
        // Expected offset = 44 + 4*contour_count (32 extent + 12 counts + contour array)
        let pos = cursor.position() as usize;
        let expected_offset = 44 + (contour_count as usize * 4);
        if pos != expected_offset {
            eprintln!("WARN: TriPrim offset mismatch: cursor at {}, expected {}", pos, expected_offset);
        }
        if log::log_enabled!(log::Level::Debug) && triprim_count > 0 && pos + 20 <= payload.len() {
            // Expected: byte[0]=0x04/05/06 (type), bytes[1-4]=nvert (little-endian u32)
            log::debug!(
                "contour_count={} triprim_count={} cursor_at={} expected={}",
                contour_count,
                triprim_count,
                pos,
                expected_offset
            );
            log::debug!("first 20 TriPrim bytes: {:02x?}", &payload[pos..pos + 20]);
        }

        // Read pre-triangulated data directly
        let mut triangles = Vec::with_capacity(triprim_count as usize);
        for i in 0..triprim_count {
            triangles.push(TriPrim::read(&mut cursor, i)?);
        }

        // Read edge refs for outline/ring reconstruction (if present).
        let has_reversed = senc_version > 200;
        let mut edge_refs = Vec::with_capacity(edge_count as usize);
        for _ in 0..edge_count {
            let start_node = cursor.read_u32::<LittleEndian>()?;
            let edge_index = cursor.read_i32::<LittleEndian>()?;
            let end_node = cursor.read_u32::<LittleEndian>()?;
            let reversed = if has_reversed {
                Some(cursor.read_u32::<LittleEndian>()? != 0)
            } else {
                None
            };
            edge_refs.push(EdgeRef {
                start_node,
                edge_index,
                end_node,
                reversed,
            });
        }

        Ok(Self { extent, triangles, edge_refs })
    }

    /// Convert all triangles to a flat vertex list for GPU.
    ///
    /// Returns SM (Simple Mercator) coordinates directly.
    /// SM coordinates are relative to the chart reference point (extent centroid)
    /// and can be used directly for rendering with camera at (0,0).
    pub fn to_vertices(&self) -> Vec<[f32; 2]> {
        let mut vertices = Vec::new();
        for tri in &self.triangles {
            vertices.extend(tri.to_triangles());
        }
        vertices
    }

    /// Analyze triangles for quality issues (skinny/degenerate triangles).
    /// Returns (total_triangles, skinny_count, degenerate_count)
    /// Also prints warnings for any suspicious triangles found.
    pub fn analyze_triangle_quality(&self) -> (usize, usize, usize) {
        let mut total = 0;
        let mut skinny = 0;
        let mut degenerate = 0;

        for (prim_idx, tri) in self.triangles.iter().enumerate() {
            let verts = tri.to_triangles();
            for (tri_idx, chunk) in verts.chunks(3).enumerate() {
                if chunk.len() < 3 {
                    continue;
                }
                total += 1;

                let v0 = chunk[0];
                let v1 = chunk[1];
                let v2 = chunk[2];

                // Calculate edge lengths
                let edge1 = ((v1[0] - v0[0]).powi(2) + (v1[1] - v0[1]).powi(2)).sqrt();
                let edge2 = ((v2[0] - v1[0]).powi(2) + (v2[1] - v1[1]).powi(2)).sqrt();
                let edge3 = ((v0[0] - v2[0]).powi(2) + (v0[1] - v2[1]).powi(2)).sqrt();

                let max_edge = edge1.max(edge2).max(edge3);
                let min_edge = edge1.min(edge2).min(edge3);

                // Check for degenerate (zero-area) triangles
                if min_edge < 0.001 {
                    degenerate += 1;
                    eprintln!("WARN: Degenerate tri prim[{}] tri[{}]: edges={:.4}/{:.4}/{:.4}",
                        prim_idx, tri_idx, edge1, edge2, edge3);
                    eprintln!("      verts: ({:.2},{:.2}), ({:.2},{:.2}), ({:.2},{:.2})",
                        v0[0], v0[1], v1[0], v1[1], v2[0], v2[1]);
                    continue;
                }

                // Check for skinny triangles (edge ratio > 50 is suspicious)
                let ratio = max_edge / min_edge;
                if ratio > 50.0 {
                    skinny += 1;
                    eprintln!("WARN: Skinny tri prim[{}] tri[{}]: ratio={:.1} edges={:.1}/{:.1}/{:.1}",
                        prim_idx, tri_idx, ratio, edge1, edge2, edge3);
                    eprintln!("      verts: ({:.2},{:.2}), ({:.2},{:.2}), ({:.2},{:.2})",
                        v0[0], v0[1], v1[0], v1[1], v2[0], v2[1]);
                }
            }
        }

        (total, skinny, degenerate)
    }

    /// Total triangle count across all primitives
    pub fn total_triangles(&self) -> usize {
        self.triangles.iter().map(|t| t.triangle_count()).sum()
    }

    /// Get statistics on primitive types (triangles, strips, fans)
    pub fn primitive_stats(&self) -> (usize, usize, usize) {
        let mut plain = 0;
        let mut strip = 0;
        let mut fan = 0;
        for tri in &self.triangles {
            match tri.prim_type {
                TriPrimType::Triangles => plain += tri.triangle_count(),
                TriPrimType::TriangleStrip => strip += tri.triangle_count(),
                TriPrimType::TriangleFan => fan += tri.triangle_count(),
            }
        }
        (plain, strip, fan)
    }

    /// Emit all vertices directly as f32 global Mercator coordinates.
    ///
    /// Zero-allocation fast path for features fully inside a tile.
    /// Expands STRIP/FAN to plain triangles (3 verts each) in-place.
    /// Stays in f32 throughout — avoids the f32→f64→clip→f64→f32 round-trip.
    pub fn for_each_vertex_direct<F>(&self, ref_mx: f32, ref_my: f32, mut callback: F)
    where
        F: FnMut([f32; 2]),
    {
        for prim in &self.triangles {
            let verts = &prim.vertices;
            match prim.prim_type {
                TriPrimType::Triangles => {
                    for v in verts {
                        callback([ref_mx + v[0], ref_my + v[1]]);
                    }
                }
                TriPrimType::TriangleStrip => {
                    for i in 0..verts.len().saturating_sub(2) {
                        let (a, b, c) = if i % 2 == 0 {
                            (i, i + 1, i + 2)
                        } else {
                            (i + 1, i, i + 2)
                        };
                        callback([ref_mx + verts[a][0], ref_my + verts[a][1]]);
                        callback([ref_mx + verts[b][0], ref_my + verts[b][1]]);
                        callback([ref_mx + verts[c][0], ref_my + verts[c][1]]);
                    }
                }
                TriPrimType::TriangleFan => {
                    if verts.len() >= 3 {
                        let center = &verts[0];
                        for i in 1..verts.len() - 1 {
                            callback([ref_mx + center[0], ref_my + center[1]]);
                            callback([ref_mx + verts[i][0], ref_my + verts[i][1]]);
                            callback([ref_mx + verts[i + 1][0], ref_my + verts[i + 1][1]]);
                        }
                    }
                }
            }
        }
    }

    /// Iterate triangles as global Mercator coordinates via callback.
    ///
    /// Zero-allocation: uses callback pattern instead of intermediate Vecs.
    /// Each triangle is passed as [[f64; 2]; 3] in global Mercator coords.
    pub fn for_each_triangle_global<F>(&self, ref_lat: f64, ref_lon: f64, mut callback: F)
    where
        F: FnMut([[f64; 2]; 3])
    {
        let (ref_mx, ref_my) = crate::tiles::latlon_to_mercator(ref_lat, ref_lon);

        for prim in &self.triangles {
            let verts = &prim.vertices;

            match prim.prim_type {
                TriPrimType::Triangles => {
                    // Every 3 vertices is a triangle
                    for chunk in verts.chunks_exact(3) {
                        callback([
                            [ref_mx + chunk[0][0] as f64, ref_my + chunk[0][1] as f64],
                            [ref_mx + chunk[1][0] as f64, ref_my + chunk[1][1] as f64],
                            [ref_mx + chunk[2][0] as f64, ref_my + chunk[2][1] as f64],
                        ]);
                    }
                }
                TriPrimType::TriangleStrip => {
                    // v0,v1,v2, then v1,v2,v3, etc. with alternating winding
                    for i in 0..verts.len().saturating_sub(2) {
                        let (a, b, c) = if i % 2 == 0 {
                            (i, i + 1, i + 2)
                        } else {
                            (i + 1, i, i + 2)  // Flip winding for odd triangles
                        };
                        callback([
                            [ref_mx + verts[a][0] as f64, ref_my + verts[a][1] as f64],
                            [ref_mx + verts[b][0] as f64, ref_my + verts[b][1] as f64],
                            [ref_mx + verts[c][0] as f64, ref_my + verts[c][1] as f64],
                        ]);
                    }
                }
                TriPrimType::TriangleFan => {
                    // v0 is center, then v0,v1,v2, then v0,v2,v3, etc.
                    if verts.len() >= 3 {
                        let center = &verts[0];
                        for i in 1..verts.len() - 1 {
                            callback([
                                [ref_mx + center[0] as f64, ref_my + center[1] as f64],
                                [ref_mx + verts[i][0] as f64, ref_my + verts[i][1] as f64],
                                [ref_mx + verts[i + 1][0] as f64, ref_my + verts[i + 1][1] as f64],
                            ]);
                        }
                    }
                }
            }
        }
    }

    /// Resolve edge references into ring polylines in SM coordinates.
    ///
    /// Returns polylines as Vec<[f32; 2]>; the caller can close/transform as needed.
    pub fn resolve_rings(&self, edge_table: &EdgeTable) -> Vec<Vec<[f32; 2]>> {
        if self.edge_refs.is_empty() {
            return Vec::new();
        }

        let mut rings = Vec::new();
        let mut current: Vec<[f32; 2]> = Vec::new();
        let mut prev_end_node: Option<u32> = None;

        for edge_ref in &self.edge_refs {
            let (start_id, end_id) = if edge_ref.is_reversed() {
                (edge_ref.end_node, edge_ref.start_node)
            } else {
                (edge_ref.start_node, edge_ref.end_node)
            };

            if prev_end_node.is_some() && prev_end_node != Some(start_id) && !current.is_empty() {
                if current.len() >= 2 {
                    rings.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
            }

            match edge_table.get_edge(edge_ref.index()) {
                Some(verts) if !verts.is_empty() => {
                    let iter: Box<dyn Iterator<Item = [f32; 2]>> = if edge_ref.is_reversed() {
                        Box::new(verts.iter().rev().copied())
                    } else {
                        Box::new(verts.iter().copied())
                    };
                    for (j, p) in iter.enumerate() {
                        if j == 0 && current.last() == Some(&p) {
                            continue;
                        }
                        current.push(p);
                    }
                }
                Some(verts) if verts.is_empty() => {
                    let (first_node, last_node) = if edge_ref.is_reversed() {
                        (edge_ref.end_node, edge_ref.start_node)
                    } else {
                        (edge_ref.start_node, edge_ref.end_node)
                    };

                    if let (Some(p0), Some(p1)) = (
                        edge_table.get_node(first_node),
                        edge_table.get_node(last_node)
                    ) {
                        if current.last() != Some(&p0) {
                            current.push(p0);
                        }
                        if p0 != p1 && current.last() != Some(&p1) {
                            current.push(p1);
                        }
                    }
                }
                _ => {}
            }

            prev_end_node = Some(end_id);
        }

        if current.len() >= 2 {
            rings.push(current);
        }

        rings
    }
}

/// Point geometry (record type 80)
#[derive(Debug, Clone, Copy)]
pub struct PointGeometry {
    pub x: f64,
    pub y: f64,
}

impl PointGeometry {
    pub fn parse(payload: &[u8]) -> Result<Self> {
        if payload.len() < 16 {
            return Err(GeometryError::Invalid("Point payload too short".into()));
        }

        let mut cursor = Cursor::new(payload);
        let x = cursor.read_f64::<LittleEndian>()?;
        let y = cursor.read_f64::<LittleEndian>()?;

        Ok(Self { x, y })
    }
}

/// Multipoint geometry (record type 83)
/// Used for SOUNDG (depth soundings) - each point has X, Y, Z (depth)
#[derive(Debug, Clone)]
pub struct MultipointGeometry {
    /// Points as (x, y, z) tuples in SM coordinates.
    /// Z is depth in meters for soundings.
    pub points: Vec<[f32; 3]>,
}

impl MultipointGeometry {
    /// Parse multipoint geometry from record payload (record type 83).
    ///
    /// Format (per qutenav osenc.h):
    /// - 4 × f64: extent_s_lat, extent_n_lat, extent_w_lon, extent_e_lon (WGS84 degrees)
    /// - u32: point_count
    /// - point_count × 3 × f32: x, y, z (SM meters, depth)
    pub fn parse(payload: &[u8]) -> Result<Self> {
        // 4 × f64 (extent) + u32 (point_count) = 36 bytes minimum header
        if payload.len() < 36 {
            return Err(GeometryError::Invalid(format!(
                "Multipoint payload too short: {} bytes (need at least 36 for header)",
                payload.len()
            )));
        }

        let mut cursor = Cursor::new(payload);

        // Skip extent bounding box (4 × f64 = 32 bytes)
        // We don't need it for rendering - points have their own coordinates
        let _extent_s_lat = cursor.read_f64::<LittleEndian>()?;
        let _extent_n_lat = cursor.read_f64::<LittleEndian>()?;
        let _extent_w_lon = cursor.read_f64::<LittleEndian>()?;
        let _extent_e_lon = cursor.read_f64::<LittleEndian>()?;

        let point_count = cursor.read_u32::<LittleEndian>()?;

        // Sanity check - soundings shouldn't have millions of points
        if point_count > 100000 {
            return Err(GeometryError::Invalid(format!(
                "Multipoint has unreasonable point count: {}",
                point_count
            )));
        }

        // Each point is 12 bytes (3 × f32)
        let expected_size = 36 + (point_count as usize * 12);
        if payload.len() < expected_size {
            return Err(GeometryError::Invalid(format!(
                "Multipoint payload too short: {} bytes for {} points (need {})",
                payload.len(), point_count, expected_size
            )));
        }

        let mut points = Vec::with_capacity(point_count as usize);
        for _ in 0..point_count {
            let x = cursor.read_f32::<LittleEndian>()?;
            let y = cursor.read_f32::<LittleEndian>()?;
            let z = cursor.read_f32::<LittleEndian>()?;
            points.push([x, y, z]);
        }

        Ok(Self { points })
    }
}

/// Line geometry (record type 81)
///
/// OSENC line geometry contains edge references that point to the edge table
/// (records 96-97). For MVP, we store the raw edge refs and resolve them
/// once the full edge table is available.
#[derive(Debug, Clone)]
pub struct LineGeometry {
    /// Bounding box
    pub extent: BBox,
    /// Edge references (to be resolved against edge table)
    pub edge_refs: Vec<EdgeRef>,
}

/// Reference to an edge in the edge table
#[derive(Debug, Clone, Copy)]
pub struct EdgeRef {
    pub start_node: u32,
    pub edge_index: i32,  // Signed: negative means reversed (v200)
    pub end_node: u32,
    pub reversed: Option<bool>,  // Explicit flag for v201+
}

impl EdgeRef {
    /// Check if edge should be traversed in reverse.
    /// Prefers explicit v201 flag; falls back to sign of edge_index (v200).
    pub fn is_reversed(&self) -> bool {
        self.reversed.unwrap_or_else(|| self.edge_index < 0)
    }

    /// Get the actual edge index (absolute value)
    pub fn index(&self) -> u32 {
        self.edge_index.unsigned_abs()
    }
}

impl LineGeometry {
    /// Parse line geometry from record payload (record type 81)
    ///
    /// Format (from OpenCPN analysis):
    /// - 4 × f64: extent (min_x, max_x, min_y, max_y)
    /// - u32: edge_count
    /// - [EdgeRef × edge_count]: edge references
    ///
    /// v200: Each EdgeRef = start_node (u32), edge_index (i32), end_node (u32) = 12 bytes
    /// v201+: Each EdgeRef += reversed_flag (u32) = 16 bytes
    pub fn parse(payload: &[u8], senc_version: u16) -> Result<Self> {
        if payload.len() < 36 {  // 32 (extent) + 4 (count)
            return Err(GeometryError::Invalid(format!(
                "Line geometry payload too short: {} bytes",
                payload.len()
            )));
        }

        let mut cursor = Cursor::new(payload);

        // Read bounding box (WGS84 degrees): south_lat, north_lat, west_lon, east_lon
        let extent = BBox {
            min_y: cursor.read_f64::<LittleEndian>()?,
            max_y: cursor.read_f64::<LittleEndian>()?,
            min_x: cursor.read_f64::<LittleEndian>()?,
            max_x: cursor.read_f64::<LittleEndian>()?,
        };

        let edge_count = cursor.read_u32::<LittleEndian>()?;
        let has_reversed = senc_version > 200;

        // Read edge references
        let mut edge_refs = Vec::with_capacity(edge_count as usize);
        for _ in 0..edge_count {
            let start_node = cursor.read_u32::<LittleEndian>()?;
            let edge_index = cursor.read_i32::<LittleEndian>()?;
            let end_node = cursor.read_u32::<LittleEndian>()?;
            let reversed = if has_reversed {
                Some(cursor.read_u32::<LittleEndian>()? != 0)
            } else {
                None
            };
            edge_refs.push(EdgeRef {
                start_node,
                edge_index,
                end_node,
                reversed,
            });
        }

        Ok(Self { extent, edge_refs })
    }

    /// Resolve edge references to actual vertex coordinates using the edge table.
    /// Returns a list of polylines (each polyline is a Vec of vertices).
    ///
    /// Key insight from QuteNav: A single LineGeometry can contain multiple
    /// disjoint chains. We only stitch edges when they're actually connected
    /// (prev_end_node == current_start_node), and start a new polyline when
    /// connectivity breaks. This prevents spurious diagonal lines.
    pub fn resolve(&self, edge_table: &EdgeTable) -> Vec<Vec<[f32; 2]>> {
        let mut polylines = Vec::new();
        let mut current: Vec<[f32; 2]> = Vec::new();
        let mut prev_end_node: Option<u32> = None;

        for edge_ref in &self.edge_refs {
            // Account for edge direction when determining node connectivity
            let (start_id, end_id) = if edge_ref.is_reversed() {
                (edge_ref.end_node, edge_ref.start_node)
            } else {
                (edge_ref.start_node, edge_ref.end_node)
            };

            // KEY: Split when connectivity breaks (QuteNav pattern)
            if prev_end_node.is_some() && prev_end_node != Some(start_id) && !current.is_empty() {
                if current.len() >= 2 {
                    polylines.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
            }

            match edge_table.get_edge(edge_ref.index()) {
                Some(verts) if !verts.is_empty() => {
                    // Normal edge with vertices
                    let iter: Box<dyn Iterator<Item = [f32; 2]>> = if edge_ref.is_reversed() {
                        Box::new(verts.iter().rev().copied())
                    } else {
                        Box::new(verts.iter().copied())
                    };
                    for (j, p) in iter.enumerate() {
                        // Skip duplicate joint vertex
                        if j == 0 && current.last() == Some(&p) {
                            continue;
                        }
                        current.push(p);
                    }
                }
                Some(verts) if verts.is_empty() => {
                    // Edge exists but has 0 intermediate vertices - use node positions
                    let (first_node, last_node) = if edge_ref.is_reversed() {
                        (edge_ref.end_node, edge_ref.start_node)
                    } else {
                        (edge_ref.start_node, edge_ref.end_node)
                    };

                    if let (Some(p0), Some(p1)) = (
                        edge_table.get_node(first_node),
                        edge_table.get_node(last_node)
                    ) {
                        // Log but don't skip long zero-vertex edges - they may be legitimate
                        // straight segments (e.g., breakwaters, piers)
                        #[cfg(debug_assertions)]
                        {
                            let dist = ((p1[0]-p0[0]).powi(2) + (p1[1]-p0[1]).powi(2)).sqrt();
                            if dist > 100.0 {
                                log::trace!("Long zero-vertex edge: {:.0}m from node {} to {}",
                                           dist, first_node, last_node);
                            }
                        }
                        // Add start node (skip if duplicate of last vertex)
                        if current.last() != Some(&p0) {
                            current.push(p0);
                        }
                        // Add end node (skip if duplicate)
                        if p0 != p1 && current.last() != Some(&p1) {
                            current.push(p1);
                        }
                    }
                }
                _ => {
                    // Edge not found - skip silently
                }
            }

            prev_end_node = Some(end_id);
        }

        // Don't forget the last polyline
        if current.len() >= 2 {
            polylines.push(current);
        }
        polylines
    }

    /// Resolve and return polylines in global Mercator coordinates.
    ///
    /// Transforms local SM coords to global Mercator using ref_lat/ref_lon.
    pub fn resolve_global(&self, edge_table: &EdgeTable, ref_lat: f64, ref_lon: f64)
        -> Vec<Vec<[f64; 2]>>
    {
        let (ref_mx, ref_my) = crate::tiles::latlon_to_mercator(ref_lat, ref_lon);

        self.resolve(edge_table)
            .into_iter()
            .map(|polyline| {
                polyline.into_iter()
                    .map(|p| [ref_mx + p[0] as f64, ref_my + p[1] as f64])
                    .collect()
            })
            .collect()
    }
}

/// Edge table built from records 96 (VectorEdge) and 97 (VectorConnectedNode).
/// Used to resolve edge references in LineGeometry to actual vertices.
#[derive(Debug, Clone, Default)]
pub struct EdgeTable {
    /// Edge ID → list of intermediate vertices (SM coordinates)
    pub edges: std::collections::HashMap<u32, Vec<[f32; 2]>>,
    /// Node ID → single vertex (SM coordinates)
    pub nodes: std::collections::HashMap<u32, [f32; 2]>,
}

impl EdgeTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse a VectorEdge record (type 96)
    ///
    /// Format:
    /// - u32: edge_id
    /// - u32: vertex_count
    /// - [f32 × 2 × vertex_count]: vertices (x, y pairs in SM coords)
    pub fn add_edge(&mut self, payload: &[u8]) -> Result<()> {
        if payload.len() < 8 {
            return Err(GeometryError::Invalid("VectorEdge payload too short".into()));
        }

        let mut cursor = Cursor::new(payload);
        let edge_id = cursor.read_u32::<LittleEndian>()?;
        let vertex_count = cursor.read_u32::<LittleEndian>()?;

        let mut vertices = Vec::with_capacity(vertex_count as usize);
        for _ in 0..vertex_count {
            let x = cursor.read_f32::<LittleEndian>()?;
            let y = cursor.read_f32::<LittleEndian>()?;
            vertices.push([x, y]);
        }

        self.edges.insert(edge_id, vertices);
        Ok(())
    }

    /// Parse a VectorConnectedNode record (type 97)
    ///
    /// Format:
    /// - u32: node_id
    /// - f32: x (SM coords)
    /// - f32: y (SM coords)
    pub fn add_node(&mut self, payload: &[u8]) -> Result<()> {
        if payload.len() < 12 {
            return Err(GeometryError::Invalid("VectorConnectedNode payload too short".into()));
        }

        let mut cursor = Cursor::new(payload);
        let node_id = cursor.read_u32::<LittleEndian>()?;
        let x = cursor.read_f32::<LittleEndian>()?;
        let y = cursor.read_f32::<LittleEndian>()?;

        self.nodes.insert(node_id, [x, y]);
        Ok(())
    }

    pub fn get_edge(&self, edge_id: u32) -> Option<&Vec<[f32; 2]>> {
        self.edges.get(&edge_id)
    }

    pub fn get_node(&self, node_id: u32) -> Option<[f32; 2]> {
        self.nodes.get(&node_id).copied()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Parse a VectorEdge TABLE record (type 96) containing multiple edges
    ///
    /// Format: count-prefixed table
    /// - u32: total_edge_count
    /// - For each edge:
    ///   - u32: edge_id
    ///   - u32: vertex_count
    ///   - [f32 × 2 × vertex_count]: intermediate vertices (SM coords)
    pub fn parse_edge_table(&mut self, payload: &[u8]) -> Result<()> {
        let mut cursor = Cursor::new(payload);

        let total_edges = cursor.read_u32::<LittleEndian>()?;
        let mut parsed_count = 0;

        for _ in 0..total_edges {
            if cursor.position() + 8 > payload.len() as u64 {
                eprintln!("WARN: Edge table truncated at edge {}/{}", parsed_count, total_edges);
                break;
            }

            let edge_id = cursor.read_u32::<LittleEndian>()?;
            let vertex_count = cursor.read_u32::<LittleEndian>()?;

            // Relaxed sanity check (was 10000, now 100000)
            // Use continue instead of break to skip just this edge, not abort all parsing
            if vertex_count > 100000 {
                eprintln!("WARN: Edge {} has {} vertices, skipping", edge_id, vertex_count);
                // Skip this edge's vertex data (vertex_count * 8 bytes per vertex)
                let skip_bytes = vertex_count as i64 * 8;
                cursor.seek(SeekFrom::Current(skip_bytes))?;
                continue;
            }

            let mut vertices = Vec::with_capacity(vertex_count as usize);
            for _ in 0..vertex_count {
                let x = cursor.read_f32::<LittleEndian>()?;
                let y = cursor.read_f32::<LittleEndian>()?;
                vertices.push([x, y]);
            }

            self.edges.insert(edge_id, vertices);
            parsed_count += 1;
        }

        // Warn on incomplete parsing
        if parsed_count < total_edges {
            eprintln!("WARN: Edge table incomplete: parsed {}/{} edges", parsed_count, total_edges);
        }

        Ok(())
    }

    /// Parse a VectorConnectedNode TABLE record (type 97) containing multiple nodes
    ///
    /// Format: count-prefixed table
    /// - u32: total_node_count
    /// - For each node:
    ///   - u32: node_id
    ///   - f32: x (SM coords)
    ///   - f32: y (SM coords)
    pub fn parse_node_table(&mut self, payload: &[u8]) -> Result<()> {
        let mut cursor = Cursor::new(payload);

        // First u32 is node count
        let total_nodes = cursor.read_u32::<LittleEndian>()?;
        let mut parsed_count = 0;

        for _ in 0..total_nodes {
            if cursor.position() + 12 > payload.len() as u64 {
                eprintln!("WARN: Node table truncated at node {}/{}", parsed_count, total_nodes);
                break;
            }
            let node_id = cursor.read_u32::<LittleEndian>()?;
            let x = cursor.read_f32::<LittleEndian>()?;
            let y = cursor.read_f32::<LittleEndian>()?;

            self.nodes.insert(node_id, [x, y]);
            parsed_count += 1;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_to_triangles_correct() {
        let strip = TriPrim {
            prim_type: TriPrimType::TriangleStrip,
            vertices: vec![[0.0, 0.0], [1.0, 0.0], [0.5, 1.0], [1.5, 1.0]],
        };

        let triangles = strip.to_triangles();
        assert_eq!(triangles.len(), 6); // 2 triangles × 3 vertices
    }

    #[test]
    fn fan_to_triangles_correct() {
        let fan = TriPrim {
            prim_type: TriPrimType::TriangleFan,
            vertices: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
        };

        let triangles = fan.to_triangles();
        assert_eq!(triangles.len(), 6); // 2 triangles × 3 vertices
        // First vertex of each triangle should be the center
        assert_eq!(triangles[0], [0.0, 0.0]);
        assert_eq!(triangles[3], [0.0, 0.0]);
    }
}
