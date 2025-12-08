//! SENC TLV (Type-Length-Value) record reader.
//!
//! SENC format: sequence of records, each with:
//! - u16 record_type
//! - u32 record_length (includes header)
//! - [u8] payload (length - 6 bytes)

use std::io::{self, Cursor, Read};
use byteorder::{LittleEndian, ReadBytesExt};
use thiserror::Error;

use super::records::{RecordType, SencHeader, CellExtent};

#[derive(Error, Debug)]
pub enum SencError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("Invalid SENC format: {0}")]
    Format(String),

    #[error("Unsupported SENC version: {0} (expected 200 or 201)")]
    UnsupportedVersion(u16),

    #[error("Unexpected end of data")]
    UnexpectedEof,
}

pub type Result<T> = std::result::Result<T, SencError>;

/// Raw TLV record from SENC file
#[derive(Debug)]
pub struct RawRecord {
    pub record_type: RecordType,
    pub raw_type: u16,
    pub length: u32,
    pub payload: Vec<u8>,
}

/// Streaming reader for SENC TLV records
pub struct SencReader<R> {
    inner: R,
    position: usize,
}

impl<R: Read> SencReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            inner: reader,
            position: 0,
        }
    }

    /// Read the next TLV record, or None if EOF
    pub fn next_record(&mut self) -> Result<Option<RawRecord>> {
        // Read header (6 bytes)
        let mut header = [0u8; 6];
        match self.inner.read_exact(&mut header) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }

        let raw_type = u16::from_le_bytes([header[0], header[1]]);
        let length = u32::from_le_bytes([header[2], header[3], header[4], header[5]]);

        // Validate length
        if length < 6 {
            return Err(SencError::Format(format!(
                "Invalid record length {} at position {}",
                length, self.position
            )));
        }

        // Read payload
        let payload_len = (length - 6) as usize;
        let mut payload = vec![0u8; payload_len];
        self.inner.read_exact(&mut payload)?;

        self.position += length as usize;

        Ok(Some(RawRecord {
            record_type: RecordType::from(raw_type),
            raw_type,
            length,
            payload,
        }))
    }

    /// Read and parse the SENC header (version + cell info)
    pub fn read_header(&mut self) -> Result<SencHeader> {
        let mut header = SencHeader::default();

        // First record must be version header
        let first = self.next_record()?.ok_or(SencError::UnexpectedEof)?;
        if first.record_type != RecordType::Header {
            return Err(SencError::Format(format!(
                "Expected Header record (type=1), got type={}",
                first.raw_type
            )));
        }

        if first.payload.len() < 2 {
            return Err(SencError::Format("Header payload too short".into()));
        }

        header.version = u16::from_le_bytes([first.payload[0], first.payload[1]]);

        // Validate version
        if header.version != 200 && header.version != 201 {
            return Err(SencError::UnsupportedVersion(header.version));
        }

        // Read subsequent header records until we hit a non-header type
        loop {
            let record = match self.next_record()? {
                Some(r) => r,
                None => break,
            };

            // DEBUG: Trace every record type to find unknown types
            println!("DEBUG: Header record type {:?} (raw: {})", record.record_type, record.raw_type);

            match record.record_type {
                RecordType::CellName => {
                    header.cell_name = parse_string(&record.payload);
                }
                RecordType::CellNativeScale => {
                    if record.payload.len() >= 4 {
                        header.native_scale = u32::from_le_bytes([
                            record.payload[0],
                            record.payload[1],
                            record.payload[2],
                            record.payload[3],
                        ]);
                    }
                }
                RecordType::CellExtent => {
                    header.extent = parse_extent(&record.payload);
                }
                RecordType::CellPublishDate
                | RecordType::CellEdition
                | RecordType::CellUpdateDate
                | RecordType::CellUpdate
                | RecordType::CellSencCreateDate
                | RecordType::CellSoundingDatum => {
                    // Skip these header records - we don't need them for rendering
                }
                RecordType::Unknown if record.raw_type < 64 => {
                    // Skip unknown header records (types 10-63 are reserved)
                    // Continue parsing until we hit feature records (64+)
                }
                // Feature record (64+) or other - header section done
                _ => break,
            }
        }

        // Compute reference point from extent centroid (required for SM coordinate conversion)
        if let Some(ref extent) = header.extent {
            header.ref_lat = (extent.max_lat + extent.min_lat) / 2.0;
            header.ref_lon = (extent.max_lon + extent.min_lon) / 2.0;
        }

        // Debug output to verify header parsing
        println!("DEBUG: Parsed SENC header:");
        println!("  cell_name: '{}'", header.cell_name);
        println!("  native_scale: {}", header.native_scale);
        println!("  extent: {:?}", header.extent);
        println!("  ref_lat: {}, ref_lon: {}", header.ref_lat, header.ref_lon);

        Ok(header)
    }
}

impl SencReader<Cursor<Vec<u8>>> {
    /// Create a reader from a byte slice
    pub fn from_bytes(data: Vec<u8>) -> Self {
        Self::new(Cursor::new(data))
    }
}

/// Parse null-terminated string from payload
fn parse_string(payload: &[u8]) -> String {
    let end = payload.iter().position(|&b| b == 0).unwrap_or(payload.len());
    String::from_utf8_lossy(&payload[..end]).to_string()
}

/// Parse CellExtent (8 x f64) from payload
/// Format per OpenCPN Osenc.h _OSENC_EXTENT_Record_Payload:
///   SW_lat, SW_lon, NW_lat, NW_lon, NE_lat, NE_lon, SE_lat, SE_lon
/// We extract min/max from SW and NE corners.
fn parse_extent(payload: &[u8]) -> Option<CellExtent> {
    // 8 doubles = 64 bytes
    if payload.len() < 64 {
        return None;
    }

    let mut cursor = Cursor::new(payload);
    // SW corner
    let sw_lat = cursor.read_f64::<LittleEndian>().ok()?;
    let sw_lon = cursor.read_f64::<LittleEndian>().ok()?;
    // NW corner (skip - we only need SW and NE)
    let _nw_lat = cursor.read_f64::<LittleEndian>().ok()?;
    let _nw_lon = cursor.read_f64::<LittleEndian>().ok()?;
    // NE corner
    let ne_lat = cursor.read_f64::<LittleEndian>().ok()?;
    let ne_lon = cursor.read_f64::<LittleEndian>().ok()?;
    // SE corner (skip)
    // let _se_lat = cursor.read_f64::<LittleEndian>().ok()?;
    // let _se_lon = cursor.read_f64::<LittleEndian>().ok()?;

    Some(CellExtent {
        min_lat: sw_lat,
        max_lat: ne_lat,
        min_lon: sw_lon,
        max_lon: ne_lon,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_string_null_terminated() {
        assert_eq!(parse_string(b"hello\0world"), "hello");
        assert_eq!(parse_string(b"hello"), "hello");
        assert_eq!(parse_string(b""), "");
    }
}
