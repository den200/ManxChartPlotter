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
    pub fn read<R: Read>(reader: &mut R) -> Result<Self> {
        let prim_type = TriPrimType::try_from(reader.read_u8()?)?;
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

        Ok(Self { prim_type, vertices })
    }

    /// Convert STRIP/FAN to plain triangles for GPU
    pub fn to_triangles(&self) -> Vec<[f32; 2]> {
        match self.prim_type {
            TriPrimType::Triangles => self.vertices.clone(),
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
            if i % 2 == 0 {
                result.push(self.vertices[i - 2]);
                result.push(self.vertices[i - 1]);
                result.push(self.vertices[i]);
            } else {
                // Flip winding for odd triangles to maintain consistent face
                result.push(self.vertices[i - 1]);
                result.push(self.vertices[i - 2]);
                result.push(self.vertices[i]);
            }
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
}

/// Bounding box
#[derive(Debug, Clone, Copy, Default)]
pub struct BBox {
    pub min_x: f64,
    pub max_x: f64,
    pub min_y: f64,
    pub max_y: f64,
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
    pub fn parse(payload: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(payload);

        // Read bounding box
        let extent = BBox {
            min_x: cursor.read_f64::<LittleEndian>()?,
            max_x: cursor.read_f64::<LittleEndian>()?,
            min_y: cursor.read_f64::<LittleEndian>()?,
            max_y: cursor.read_f64::<LittleEndian>()?,
        };

        let contour_count = cursor.read_u32::<LittleEndian>()?;
        let triprim_count = cursor.read_u32::<LittleEndian>()?;
        let _edge_count = cursor.read_u32::<LittleEndian>()?;

        // Skip contour counts (not needed for fill rendering)
        cursor.seek(SeekFrom::Current((contour_count * 4) as i64))?;

        // Read pre-triangulated data directly
        let mut triangles = Vec::with_capacity(triprim_count as usize);
        for _ in 0..triprim_count {
            triangles.push(TriPrim::read(&mut cursor)?);
        }

        // Skip edge refs for now (used for outline rendering)

        Ok(Self { extent, triangles })
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

    /// Total triangle count across all primitives
    pub fn total_triangles(&self) -> usize {
        self.triangles.iter().map(|t| t.triangle_count()).sum()
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
