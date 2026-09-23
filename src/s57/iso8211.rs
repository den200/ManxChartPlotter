//! ISO/IEC 8211, the container every S-57 file is written in.
//!
//! An ISO 8211 file is one Data Descriptive Record (DDR) describing each
//! field — its name, its subfield labels and their formats — followed by Data
//! Records (DRs) holding the values. S-57 uses a small, regular corner of the
//! standard: fixed leaders, no field concatenation beyond repeating groups,
//! and a handful of subfield formats. This reads exactly that corner and
//! refuses anything else loudly rather than guessing.
//!
//! Formats handled: `A`, `I`, `R` (fixed width `A(n)` or delimited by the unit
//! terminator), `B(n)` bit strings, and the binary integers `b11 b12 b14 b21
//! b22 b24` (little-endian, unsigned `b1w` and signed `b2w`). Repeat counts
//! (`3A`, `2b24`) and nested groups (`2(b11,b12)`) are expanded.

use std::collections::HashMap;

/// Unit terminator: ends a variable-length subfield.
pub const UT: u8 = 0x1f;
/// Field terminator: ends a field.
pub const FT: u8 = 0x1e;

#[derive(Debug, thiserror::Error)]
pub enum Iso8211Error {
    #[error("ISO 8211: {0}")]
    Malformed(String),
}

type Result<T> = std::result::Result<T, Iso8211Error>;

fn bad<T>(msg: impl Into<String>) -> Result<T> {
    Err(Iso8211Error::Malformed(msg.into()))
}

/// How one subfield is stored.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Format {
    /// Character data; `Some(n)` fixed width, `None` delimited.
    A(Option<usize>),
    /// Integer written as characters.
    I(Option<usize>),
    /// Real written as characters.
    R(Option<usize>),
    /// Bit string of `n` bits (fixed; S-57 uses it for LNAM-style keys).
    B(usize),
    /// Binary little-endian integer: width in bytes, signed.
    Bin { width: usize, signed: bool },
}

/// One field's description from the DDR.
#[derive(Debug, Clone)]
pub struct FieldDefn {
    pub tag: String,
    pub name: String,
    /// Subfield labels, in order.
    pub labels: Vec<String>,
    /// One format per label.
    pub formats: Vec<Format>,
    /// The label list began with `*`: the whole group repeats until the
    /// field terminator (coordinates, pointers, attributes).
    pub repeating: bool,
}

/// A decoded subfield value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Int(i64),
    Real(f64),
    Bytes(Vec<u8>),
    /// A numeric subfield left blank — "unknown", which S-57 distinguishes
    /// from zero.
    Empty,
}

impl Value {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Real(r) => Some(*r as i64),
            Value::Str(s) => s.trim().parse().ok(),
            _ => None,
        }
    }
    pub fn as_real(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Real(r) => Some(*r),
            Value::Str(s) => s.trim().parse().ok(),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }
}

/// One field of a data record: its tag and raw bytes (terminator stripped).
#[derive(Debug, Clone)]
pub struct Field {
    pub tag: String,
    pub data: Vec<u8>,
}

/// One data record: its fields in file order.
#[derive(Debug, Clone)]
pub struct Record {
    pub fields: Vec<Field>,
}

impl Record {
    pub fn field(&self, tag: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.tag == tag)
    }
    pub fn fields_named<'a>(&'a self, tag: &'a str) -> impl Iterator<Item = &'a Field> + 'a {
        self.fields.iter().filter(move |f| f.tag == tag)
    }
}

/// A parsed ISO 8211 file: the field descriptions and every data record.
#[derive(Debug)]
pub struct Module {
    pub defns: HashMap<String, FieldDefn>,
    pub records: Vec<Record>,
}

struct Leader {
    record_len: usize,
    base_addr: usize,
    size_len: usize,
    size_pos: usize,
    size_tag: usize,
}

fn digits(b: &[u8]) -> Result<usize> {
    let s = std::str::from_utf8(b).map_err(|_| Iso8211Error::Malformed("leader digits".into()))?;
    let t = s.trim();
    if t.is_empty() {
        return Ok(0);
    }
    t.parse()
        .map_err(|_| Iso8211Error::Malformed(format!("leader number {s:?}")))
}

fn leader(b: &[u8]) -> Result<Leader> {
    if b.len() < 24 {
        return bad("short leader");
    }
    Ok(Leader {
        record_len: digits(&b[0..5])?,
        base_addr: digits(&b[12..17])?,
        size_len: digits(&b[20..21])?,
        size_pos: digits(&b[21..22])?,
        size_tag: digits(&b[23..24])?,
    })
}

/// Split one record (DDR or DR) into (tag, field bytes) pairs.
fn split_fields(rec: &[u8]) -> Result<Vec<(String, &[u8])>> {
    let l = leader(rec)?;
    if l.size_tag == 0 || l.size_len == 0 || l.size_pos == 0 {
        return bad("zero-width directory entry");
    }
    let entry = l.size_tag + l.size_len + l.size_pos;
    let mut out = Vec::new();
    let mut p = 24;
    while p < rec.len() && rec[p] != FT {
        if p + entry > rec.len() {
            return bad("directory runs past the record");
        }
        let tag = String::from_utf8_lossy(&rec[p..p + l.size_tag]).into_owned();
        let len = digits(&rec[p + l.size_tag..p + l.size_tag + l.size_len])?;
        let pos = digits(&rec[p + l.size_tag + l.size_len..p + entry])?;
        let start = l.base_addr + pos;
        let end = start + len;
        if end > rec.len() {
            return bad(format!("field {tag} runs past the record"));
        }
        out.push((tag, &rec[start..end]));
        p += entry;
    }
    Ok(out)
}

/// Strip a trailing field terminator (and a lexical-level-2 padding NUL).
fn strip_ft(b: &[u8]) -> &[u8] {
    let mut e = b.len();
    if e > 0 && b[e - 1] == FT {
        e -= 1;
    } else if e > 1 && b[e - 1] == 0 && b[e - 2] == FT {
        e -= 2;
    }
    &b[..e]
}

impl Module {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let ddr_len = leader(bytes)?.record_len;
        if ddr_len == 0 || ddr_len > bytes.len() {
            return bad("DDR length");
        }
        let mut defns = HashMap::new();
        for (tag, data) in split_fields(&bytes[..ddr_len])? {
            if tag == "0000" {
                continue; // file control field
            }
            defns.insert(tag.clone(), parse_defn(&tag, data)?);
        }

        let mut records = Vec::new();
        let mut p = ddr_len;
        while p + 24 <= bytes.len() {
            let len = leader(&bytes[p..])?.record_len;
            if len < 24 || p + len > bytes.len() {
                return bad(format!("record at byte {p} has length {len}"));
            }
            let fields = split_fields(&bytes[p..p + len])?
                .into_iter()
                .filter(|(tag, _)| tag != "0001")
                .map(|(tag, data)| Field {
                    tag,
                    data: strip_ft(data).to_vec(),
                })
                .collect();
            records.push(Record { fields });
            p += len;
        }
        Ok(Self { defns, records })
    }

    /// Decode a field into rows of subfield values: one row for an ordinary
    /// field, one per repetition for a repeating one. `ucs2` reads delimited
    /// `A` subfields as UCS-2 (S-57 lexical level 2, used by NATF).
    pub fn decode(&self, field: &Field, ucs2: bool) -> Result<Vec<Vec<Value>>> {
        let Some(defn) = self.defns.get(&field.tag) else {
            return bad(format!("no description for field {}", field.tag));
        };
        let mut rows = Vec::new();
        let mut p = 0;
        let d = &field.data;
        loop {
            if p >= d.len() {
                break;
            }
            let mut row = Vec::with_capacity(defn.formats.len());
            for f in &defn.formats {
                let (v, used) = read_subfield(&d[p..], *f, ucs2)?;
                row.push(v);
                p += used;
            }
            rows.push(row);
            if !defn.repeating {
                break;
            }
        }
        Ok(rows)
    }

    /// The first row of a field, by label.
    pub fn row_map(&self, field: &Field, ucs2: bool) -> Result<HashMap<String, Value>> {
        let defn = &self.defns[&field.tag];
        let rows = self.decode(field, ucs2)?;
        let row = rows.into_iter().next().unwrap_or_default();
        Ok(defn.labels.iter().cloned().zip(row).collect())
    }
}

fn parse_defn(tag: &str, data: &[u8]) -> Result<FieldDefn> {
    let data = strip_ft(data);
    // Field controls (9 chars in S-57: e.g. "1600;&   "), then name, UT,
    // array descriptor (labels), UT, format controls.
    let rest = if data.len() > 9 { &data[9..] } else { &[][..] };
    let parts: Vec<&[u8]> = rest.split(|&b| b == UT).collect();
    let name = String::from_utf8_lossy(parts.first().copied().unwrap_or(&[])).into_owned();
    let labels_raw = String::from_utf8_lossy(parts.get(1).copied().unwrap_or(&[])).into_owned();
    let formats_raw = String::from_utf8_lossy(parts.get(2).copied().unwrap_or(&[])).into_owned();

    let repeating = labels_raw.starts_with('*');
    let labels: Vec<String> = labels_raw
        .trim_start_matches('*')
        .split('!')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let formats = if formats_raw.trim().is_empty() {
        Vec::new()
    } else {
        parse_formats(&formats_raw)?
    };
    if !labels.is_empty() && labels.len() != formats.len() {
        return bad(format!(
            "field {tag}: {} labels but {} formats ({labels_raw} / {formats_raw})",
            labels.len(),
            formats.len()
        ));
    }
    Ok(FieldDefn {
        tag: tag.to_string(),
        name,
        labels,
        formats,
        repeating,
    })
}

/// Expand a format control string such as `(b11,b14,2b11,3A,2A(8),R(4))`.
pub fn parse_formats(s: &str) -> Result<Vec<Format>> {
    let s = s.trim();
    let inner = s
        .strip_prefix('(')
        .and_then(|t| t.strip_suffix(')'))
        .unwrap_or(s);
    let mut out = Vec::new();
    for item in split_top(inner) {
        expand(item.trim(), &mut out)?;
    }
    Ok(out)
}

/// Split on commas not inside parentheses.
fn split_top(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0i32, 0usize);
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

fn expand(item: &str, out: &mut Vec<Format>) -> Result<()> {
    if item.is_empty() {
        return Ok(());
    }
    let digits_end = item.find(|c: char| !c.is_ascii_digit()).unwrap_or(item.len());
    let count: usize = if digits_end == 0 { 1 } else { item[..digits_end].parse().unwrap_or(1) };
    let body = &item[digits_end..];
    if body.starts_with('(') {
        let inner = parse_formats(body)?;
        for _ in 0..count {
            out.extend(inner.iter().copied());
        }
        return Ok(());
    }
    let one = parse_one(body)?;
    for _ in 0..count {
        out.push(one);
    }
    Ok(())
}

fn parse_one(f: &str) -> Result<Format> {
    let width = |rest: &str| -> Result<Option<usize>> {
        if rest.is_empty() {
            return Ok(None);
        }
        let n = rest
            .strip_prefix('(')
            .and_then(|t| t.strip_suffix(')'))
            .ok_or_else(|| Iso8211Error::Malformed(format!("format {f:?}")))?;
        n.parse()
            .map(Some)
            .map_err(|_| Iso8211Error::Malformed(format!("format width {f:?}")))
    };
    let (kind, rest) = f.split_at(1);
    Ok(match kind {
        "A" => Format::A(width(rest)?),
        "I" => Format::I(width(rest)?),
        "R" => Format::R(width(rest)?),
        "B" => Format::B(width(rest)?.ok_or_else(|| Iso8211Error::Malformed(format!("{f:?}")))?),
        "b" => {
            let b = rest.as_bytes();
            if b.len() != 2 {
                return bad(format!("binary format {f:?}"));
            }
            let signed = match b[0] {
                b'1' => false,
                b'2' => true,
                _ => return bad(format!("binary format {f:?}")),
            };
            let width = (b[1] - b'0') as usize;
            if !matches!(width, 1 | 2 | 4) {
                return bad(format!("binary width {f:?}"));
            }
            Format::Bin { width, signed }
        }
        _ => return bad(format!("unsupported format {f:?}")),
    })
}

/// Read one subfield; returns the value and the bytes consumed (including a
/// delimiter).
fn read_subfield(d: &[u8], f: Format, ucs2: bool) -> Result<(Value, usize)> {
    // S-57 lexical levels 0 and 1 are ASCII and ISO 8859-1; Latin-1 maps
    // each byte to the code point of the same value, so decoding every
    // byte that way reads both. (Level 2, UCS-2, is handled below.)
    let text = |bytes: &[u8]| bytes.iter().map(|&b| b as char).collect::<String>().trim_end().to_string();
    let delimited = |d: &[u8]| -> (Vec<u8>, usize) {
        if ucs2 {
            // Two-byte units; ends at 0x1f 0x00 (or the end of the field).
            let mut i = 0;
            while i + 1 < d.len() && !(d[i] == UT && d[i + 1] == 0) {
                i += 2;
            }
            let end = i.min(d.len());
            (d[..end].to_vec(), (end + 2).min(d.len()))
        } else {
            let end = d.iter().position(|&b| b == UT).unwrap_or(d.len());
            (d[..end].to_vec(), (end + 1).min(d.len()))
        }
    };
    match f {
        Format::A(w) | Format::I(w) | Format::R(w) => {
            let (raw, used) = match w {
                Some(n) => {
                    if n > d.len() {
                        return bad("fixed subfield runs past the field");
                    }
                    (d[..n].to_vec(), n)
                }
                None => delimited(d),
            };
            let s = if ucs2 && w.is_none() && matches!(f, Format::A(_)) {
                let units: Vec<u16> = raw
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                String::from_utf16_lossy(&units).trim_end().to_string()
            } else {
                text(&raw)
            };
            let v = match f {
                Format::A(_) => Value::Str(s),
                Format::I(_) => match s.trim() {
                    "" => Value::Empty,
                    t => t.parse().map(Value::Int).unwrap_or(Value::Str(s)),
                },
                _ => match s.trim() {
                    "" => Value::Empty,
                    t => t.parse().map(Value::Real).unwrap_or(Value::Str(s)),
                },
            };
            Ok((v, used))
        }
        Format::B(bits) => {
            let n = bits.div_ceil(8);
            if n > d.len() {
                return bad("bit field runs past the field");
            }
            Ok((Value::Bytes(d[..n].to_vec()), n))
        }
        Format::Bin { width, signed } => {
            if width > d.len() {
                return bad("binary subfield runs past the field");
            }
            let b = &d[..width];
            let v = match (width, signed) {
                (1, false) => b[0] as i64,
                (1, true) => b[0] as i8 as i64,
                (2, false) => u16::from_le_bytes([b[0], b[1]]) as i64,
                (2, true) => i16::from_le_bytes([b[0], b[1]]) as i64,
                (4, false) => u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as i64,
                (4, true) => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as i64,
                _ => unreachable!(),
            };
            Ok((Value::Int(v), width))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_expand_counts_widths_and_groups() {
        let f = parse_formats("(b11,b14,2b11,3A,2A(8),R(4),b11,2A,b11,b12,A)").unwrap();
        assert_eq!(f.len(), 16);
        assert_eq!(f[0], Format::Bin { width: 1, signed: false });
        assert_eq!(f[1], Format::Bin { width: 4, signed: false });
        assert_eq!(f[4], Format::A(None));
        assert_eq!(f[7], Format::A(Some(8)));
        assert_eq!(f[9], Format::R(Some(4)));
        let g = parse_formats("(2b24)").unwrap();
        assert_eq!(g, vec![Format::Bin { width: 4, signed: true }; 2]);
        let h = parse_formats("(B(40),3b11)").unwrap();
        assert_eq!(h[0], Format::B(40));
        assert_eq!(parse_formats("(2(b11,b12))").unwrap().len(), 4);
    }

    #[test]
    fn latin1_text_decodes() {
        let (v, used) = read_subfield(&[b'L', 0xa0, b'1', 0xb1, UT], Format::A(None), false).unwrap();
        assert_eq!(v, Value::Str("L\u{a0}1±".into()));
        assert_eq!(used, 5);
    }

    #[test]
    fn a_noaa_cell_parses_into_records() {
        let bytes = include_bytes!("../../tests/fixtures/s57/US4MA1BD.000");
        let m = Module::parse(bytes).expect("parses");
        assert!(m.defns.contains_key("DSID"));
        assert!(m.defns["SG2D"].repeating);
        assert_eq!(m.defns["SG2D"].labels, vec!["YCOO", "XCOO"]);
        // The first record is the data set identification.
        let dsid = m.records[0].field("DSID").expect("DSID first");
        let row = m.row_map(dsid, false).unwrap();
        assert_eq!(row["DSNM"].as_str(), Some("US4MA1BD.000"));
        // Vector and feature records follow.
        assert!(m.records.iter().any(|r| r.field("VRID").is_some()));
        assert!(m.records.iter().any(|r| r.field("FRID").is_some()));
    }
}
