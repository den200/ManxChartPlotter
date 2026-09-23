//! An S-57 cell as records: the dataset's parameters, its vector primitives
//! (nodes and edges) and its features — with its update files applied.
//!
//! This is the S-57 data model (IHO S-57 3.1, Part 3) and nothing more:
//! records keyed by their name (RCNM, RCID), coordinates still in the
//! dataset's integer units until [`Cell::lonlat`] scales them. Assembling
//! geometry from these records is [`super::geometry`]'s job.
//!
//! Updates (`.001`, `.002`, …) are applied in sequence, as ER update files
//! define them: whole records inserted, deleted or modified, and within a
//! modified record its attributes (ATTF/NATF/ATTV), its pointers (FSPT, FFPT
//! and VRPT, under FSPC/FFPC/VRPC control) and its coordinates (SG2D/SG3D,
//! under SGCC) edited in place.

use std::collections::HashMap;

use super::iso8211::{Field, Module, Value};

#[derive(Debug, thiserror::Error)]
pub enum CellError {
    #[error(transparent)]
    Iso(#[from] super::iso8211::Iso8211Error),
    #[error("S-57: {0}")]
    Malformed(String),
    #[error("reading {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
}

type Result<T> = std::result::Result<T, CellError>;

/// Record name: record type (RCNM) and record id (RCID).
pub type Name = (u8, u32);

/// RCNM values.
pub const RCNM_FE: u8 = 100;
pub const RCNM_VI: u8 = 110;
pub const RCNM_VC: u8 = 120;
pub const RCNM_VE: u8 = 130;
pub const RCNM_VF: u8 = 140;

/// Data set identification and parameters.
#[derive(Debug, Clone, Default)]
pub struct Dataset {
    /// Data set name, e.g. `US5MA1AQ.000`.
    pub dsnm: String,
    pub edition: u32,
    pub update: u32,
    /// Issue date of the last update applied, `YYYYMMDD`.
    pub issue_date: String,
    /// Coordinate multiplication factor: integer units per degree.
    pub comf: f64,
    /// Sounding multiplication factor: integer units per metre.
    pub somf: f64,
    /// Compilation scale, 1:CSCL.
    pub cscl: u32,
    /// Lexical level of NATF strings (2 = UCS-2).
    pub nall: u8,
    /// Lexical level of ATTF strings.
    pub aall: u8,
}

/// A pointer from a vector record to another (an edge's end nodes).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VectorPtr {
    pub name: Name,
    /// 1 forward, 2 reverse, 255 null.
    pub ornt: u8,
    /// 1 exterior, 2 interior, 3 exterior truncated by the data limit.
    pub usag: u8,
    /// 1 beginning node, 2 end node, 3 left face, 4 right face, 5 contained.
    pub topi: u8,
    pub mask: u8,
}

/// A vector (spatial) record: an isolated or connected node, or an edge.
#[derive(Debug, Clone, Default)]
pub struct Vector {
    pub name: Name,
    pub rver: u32,
    /// Integer coordinates: `[y, x]` for SG2D, `[y, x, z]` for SG3D.
    pub coords: Vec<[i32; 3]>,
    /// Coordinates came from SG3D (soundings): the third value is depth.
    pub three_d: bool,
    pub ptrs: Vec<VectorPtr>,
    pub attrs: Vec<(u16, String)>,
}

/// A pointer from a feature to a spatial record.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpatialPtr {
    pub name: Name,
    /// 1 forward, 2 reverse, 255 null.
    pub ornt: u8,
    /// 1 exterior, 2 interior, 3 exterior truncated by the data limit.
    pub usag: u8,
    /// 1 masked, 2 shown, 255 null.
    pub mask: u8,
}

/// A pointer from a feature to another feature (FFPT).
#[derive(Debug, Clone, PartialEq)]
pub struct FeaturePtr {
    /// LNAM of the target: agency, feature id number, subdivision.
    pub lnam: (u16, u32, u16),
    /// Relationship indicator: 1 master, 2 slave, 3 peer.
    pub rind: u8,
    pub comt: String,
}

/// A feature record.
#[derive(Debug, Clone, Default)]
pub struct FeatureRec {
    pub rcid: u32,
    /// 1 point, 2 line, 3 area, 255 none.
    pub prim: u8,
    pub grup: u8,
    pub objl: u16,
    pub rver: u32,
    /// Feature object identifier: agency, id number, subdivision.
    pub foid: (u16, u32, u16),
    pub attf: Vec<(u16, String)>,
    pub natf: Vec<(u16, String)>,
    pub fspt: Vec<SpatialPtr>,
    pub ffpt: Vec<FeaturePtr>,
}

/// A whole cell, updates applied.
#[derive(Debug, Clone, Default)]
pub struct Cell {
    pub dataset: Dataset,
    pub vectors: HashMap<Name, Vector>,
    /// In file order; updates append inserted features at the end.
    pub features: Vec<FeatureRec>,
    /// How many update files were applied.
    pub updates_applied: u32,
}

/// The value S-57 writes to delete an attribute in an update.
const DELETE: &str = "\u{7f}";

fn int(row: &HashMap<String, Value>, k: &str) -> i64 {
    row.get(k).and_then(Value::as_int).unwrap_or(0)
}

fn name_of(bytes: &[u8]) -> Name {
    // B(40): RCNM (1 byte) then RCID (u32, little-endian).
    if bytes.len() < 5 {
        return (0, 0);
    }
    (bytes[0], u32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]))
}

fn lnam_of(bytes: &[u8]) -> (u16, u32, u16) {
    // B(64): AGEN (u16), FIDN (u32), FIDS (u16).
    if bytes.len() < 8 {
        return (0, 0, 0);
    }
    (
        u16::from_le_bytes([bytes[0], bytes[1]]),
        u32::from_le_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]),
        u16::from_le_bytes([bytes[6], bytes[7]]),
    )
}

/// Edit a list under an update control field: `op` 1 insert, 2 delete,
/// 3 modify; `index` is 1-based; `count` items are affected and, for
/// insert and modify, taken from `items`.
fn edit_list<T: Clone>(list: &mut Vec<T>, op: i64, index: i64, count: i64, items: &[T]) {
    let at = (index.max(1) as usize - 1).min(list.len());
    let n = count.max(0) as usize;
    match op {
        1 => {
            for (k, it) in items.iter().take(n).enumerate() {
                list.insert((at + k).min(list.len()), it.clone());
            }
        }
        2 => {
            let end = (at + n).min(list.len());
            list.drain(at..end);
        }
        3 => {
            for (k, it) in items.iter().take(n).enumerate() {
                if at + k < list.len() {
                    list[at + k] = it.clone();
                }
            }
        }
        _ => {}
    }
}

/// Set, replace or delete attributes by code, as an update's ATTF does.
fn merge_attrs(list: &mut Vec<(u16, String)>, update: Vec<(u16, String)>) {
    for (code, value) in update {
        let at = list.iter().position(|(c, _)| *c == code);
        match (at, value == DELETE) {
            (Some(i), true) => {
                list.remove(i);
            }
            (Some(i), false) => list[i].1 = value,
            (None, false) => list.push((code, value)),
            (None, true) => {}
        }
    }
}

struct Decoder<'a> {
    m: &'a Module,
    nall: u8,
}

impl Decoder<'_> {
    fn row(&self, f: &Field) -> Result<HashMap<String, Value>> {
        Ok(self.m.row_map(f, false)?)
    }

    fn attrs(&self, f: &Field, national: bool) -> Result<Vec<(u16, String)>> {
        let ucs2 = national && self.nall == 2;
        Ok(self
            .m
            .decode(f, ucs2)?
            .into_iter()
            .map(|row| {
                let code = row.first().and_then(Value::as_int).unwrap_or(0) as u16;
                let value = row.get(1).and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
                (code, value)
            })
            .collect())
    }

    fn spatial_ptrs(&self, f: &Field) -> Result<Vec<SpatialPtr>> {
        Ok(self
            .m
            .decode(f, false)?
            .into_iter()
            .map(|row| SpatialPtr {
                name: row.first().and_then(Value::as_bytes).map(name_of).unwrap_or((0, 0)),
                ornt: row.get(1).and_then(Value::as_int).unwrap_or(255) as u8,
                usag: row.get(2).and_then(Value::as_int).unwrap_or(255) as u8,
                mask: row.get(3).and_then(Value::as_int).unwrap_or(255) as u8,
            })
            .collect())
    }

    fn vector_ptrs(&self, f: &Field) -> Result<Vec<VectorPtr>> {
        Ok(self
            .m
            .decode(f, false)?
            .into_iter()
            .map(|row| VectorPtr {
                name: row.first().and_then(Value::as_bytes).map(name_of).unwrap_or((0, 0)),
                ornt: row.get(1).and_then(Value::as_int).unwrap_or(255) as u8,
                usag: row.get(2).and_then(Value::as_int).unwrap_or(255) as u8,
                topi: row.get(3).and_then(Value::as_int).unwrap_or(255) as u8,
                mask: row.get(4).and_then(Value::as_int).unwrap_or(255) as u8,
            })
            .collect())
    }

    fn feature_ptrs(&self, f: &Field) -> Result<Vec<FeaturePtr>> {
        Ok(self
            .m
            .decode(f, false)?
            .into_iter()
            .map(|row| FeaturePtr {
                lnam: row.first().and_then(Value::as_bytes).map(lnam_of).unwrap_or((0, 0, 0)),
                rind: row.get(1).and_then(Value::as_int).unwrap_or(0) as u8,
                comt: row.get(2).and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default(),
            })
            .collect())
    }

    fn coords(&self, f: &Field, three_d: bool) -> Result<Vec<[i32; 3]>> {
        Ok(self
            .m
            .decode(f, false)?
            .into_iter()
            .map(|row| {
                let g = |i: usize| row.get(i).and_then(Value::as_int).unwrap_or(0) as i32;
                [g(0), g(1), if three_d { g(2) } else { 0 }]
            })
            .collect())
    }
}

/// Update instruction of a record: 1 insert, 2 delete, 3 modify.
fn ruin(row: &HashMap<String, Value>) -> i64 {
    int(row, "RUIN").max(1)
}

impl Cell {
    /// Read a base cell (`.000`) and apply the update files beside it.
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let read = |p: &std::path::Path| {
            std::fs::read(p).map_err(|source| CellError::Io {
                path: p.display().to_string(),
                source,
            })
        };
        let mut cell = Self::parse_base(&read(path)?)?;
        for n in 1..=999u32 {
            let upd = path.with_extension(format!("{n:03}"));
            if !upd.exists() {
                break;
            }
            let bytes = read(&upd)?;
            cell.apply_update(&bytes)?;
        }
        Ok(cell)
    }

    /// Parse a base cell's bytes.
    pub fn parse_base(bytes: &[u8]) -> Result<Self> {
        let m = Module::parse(bytes)?;
        let mut cell = Cell::default();
        cell.read_records(&m, false)?;
        Ok(cell)
    }

    /// Apply one update file's bytes. An update for another edition, or one
    /// already contained in the base (a re-issue), is skipped.
    pub fn apply_update(&mut self, bytes: &[u8]) -> Result<()> {
        let m = Module::parse(bytes)?;
        let first = m.records.first().and_then(|r| r.field("DSID"));
        if let Some(dsid) = first {
            let row = m.row_map(dsid, false)?;
            let edtn = row.get("EDTN").and_then(Value::as_int).unwrap_or(0) as u32;
            let updn = row.get("UPDN").and_then(Value::as_int).unwrap_or(0) as u32;
            if (edtn != 0 && edtn != self.dataset.edition) || updn <= self.dataset.update {
                return Ok(());
            }
        }
        self.read_records(&m, true)?;
        self.updates_applied += 1;
        Ok(())
    }

    fn read_records(&mut self, m: &Module, is_update: bool) -> Result<()> {
        let mut d = Decoder { m, nall: self.dataset.nall };
        let mut feature_index: HashMap<u32, usize> = self
            .features
            .iter()
            .enumerate()
            .map(|(i, f)| (f.rcid, i))
            .collect();

        for rec in &m.records {
            if let Some(f) = rec.field("DSID") {
                let row = d.row(f)?;
                // An update file is named for itself (`.003`); the cell
                // keeps the base's name.
                if !is_update {
                    self.dataset.dsnm = row.get("DSNM").and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
                }
                self.dataset.edition = int(&row, "EDTN") as u32;
                self.dataset.update = int(&row, "UPDN") as u32;
                self.dataset.issue_date = row.get("ISDT").and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
                if let Some(s) = rec.field("DSSI") {
                    let r = d.row(s)?;
                    self.dataset.aall = int(&r, "AALL") as u8;
                    self.dataset.nall = int(&r, "NALL") as u8;
                    d.nall = self.dataset.nall;
                }
                continue;
            }
            if let Some(f) = rec.field("DSPM") {
                let row = d.row(f)?;
                self.dataset.comf = int(&row, "COMF").max(1) as f64;
                self.dataset.somf = int(&row, "SOMF").max(1) as f64;
                self.dataset.cscl = int(&row, "CSCL") as u32;
                continue;
            }
            if let Some(f) = rec.field("VRID") {
                let row = d.row(f)?;
                let name = (int(&row, "RCNM") as u8, int(&row, "RCID") as u32);
                let op = if is_update { ruin(&row) } else { 1 };
                match op {
                    2 => {
                        self.vectors.remove(&name);
                    }
                    3 => {
                        let Some(v) = self.vectors.get_mut(&name) else { continue };
                        v.rver = int(&row, "RVER") as u32;
                        if let Some(a) = rec.field("ATTV") {
                            merge_attrs(&mut v.attrs, d.attrs(a, false)?);
                        }
                        if let Some(p) = rec.field("VRPT") {
                            let items = d.vector_ptrs(p)?;
                            match rec.field("VRPC") {
                                Some(c) => {
                                    let r = d.row(c)?;
                                    edit_list(&mut v.ptrs, int(&r, "VPUI"), int(&r, "VPIX"), int(&r, "NVPT"), &items);
                                }
                                None => v.ptrs = items,
                            }
                        }
                        let (coords, three_d) = match (rec.field("SG2D"), rec.field("SG3D")) {
                            (Some(c), _) => (Some(d.coords(c, false)?), false),
                            (_, Some(c)) => (Some(d.coords(c, true)?), true),
                            _ => (None, v.three_d),
                        };
                        match (rec.field("SGCC"), coords) {
                            (Some(c), items) => {
                                let r = d.row(c)?;
                                let items = items.unwrap_or_default();
                                edit_list(&mut v.coords, int(&r, "CCUI"), int(&r, "CCIX"), int(&r, "CCNC"), &items);
                            }
                            (None, Some(items)) => {
                                v.coords = items;
                                v.three_d = three_d;
                            }
                            (None, None) => {}
                        }
                    }
                    _ => {
                        let mut v = Vector {
                            name,
                            rver: int(&row, "RVER") as u32,
                            ..Default::default()
                        };
                        if let Some(c) = rec.field("SG2D") {
                            v.coords = d.coords(c, false)?;
                        } else if let Some(c) = rec.field("SG3D") {
                            v.coords = d.coords(c, true)?;
                            v.three_d = true;
                        }
                        for p in rec.fields_named("VRPT") {
                            v.ptrs.extend(d.vector_ptrs(p)?);
                        }
                        if let Some(a) = rec.field("ATTV") {
                            v.attrs = d.attrs(a, false)?;
                        }
                        self.vectors.insert(name, v);
                    }
                }
                continue;
            }
            if let Some(f) = rec.field("FRID") {
                let row = d.row(f)?;
                let rcid = int(&row, "RCID") as u32;
                let op = if is_update { ruin(&row) } else { 1 };
                match op {
                    2 => {
                        if let Some(i) = feature_index.remove(&rcid) {
                            // Tombstone rather than shift every index.
                            self.features[i].prim = 0;
                            self.features[i].objl = 0;
                        }
                    }
                    3 => {
                        let Some(&i) = feature_index.get(&rcid) else { continue };
                        let fe = &mut self.features[i];
                        fe.rver = int(&row, "RVER") as u32;
                        if let Some(a) = rec.field("ATTF") {
                            merge_attrs(&mut fe.attf, d.attrs(a, false)?);
                        }
                        if let Some(a) = rec.field("NATF") {
                            merge_attrs(&mut fe.natf, d.attrs(a, true)?);
                        }
                        if let Some(p) = rec.field("FSPT") {
                            let items = d.spatial_ptrs(p)?;
                            match rec.field("FSPC") {
                                Some(c) => {
                                    let r = d.row(c)?;
                                    edit_list(&mut fe.fspt, int(&r, "FSUI"), int(&r, "FSIX"), int(&r, "NSPT"), &items);
                                }
                                None => fe.fspt = items,
                            }
                        } else if let Some(c) = rec.field("FSPC") {
                            // A deletion carries no pointers.
                            let r = d.row(c)?;
                            edit_list(&mut fe.fspt, int(&r, "FSUI"), int(&r, "FSIX"), int(&r, "NSPT"), &[]);
                        }
                        if let Some(p) = rec.field("FFPT") {
                            let items = d.feature_ptrs(p)?;
                            match rec.field("FFPC") {
                                Some(c) => {
                                    let r = d.row(c)?;
                                    edit_list(&mut fe.ffpt, int(&r, "FFUI"), int(&r, "FFIX"), int(&r, "NFPT"), &items);
                                }
                                None => fe.ffpt = items,
                            }
                        } else if let Some(c) = rec.field("FFPC") {
                            let r = d.row(c)?;
                            edit_list(&mut fe.ffpt, int(&r, "FFUI"), int(&r, "FFIX"), int(&r, "NFPT"), &[]);
                        }
                    }
                    _ => {
                        let mut fe = FeatureRec {
                            rcid,
                            prim: int(&row, "PRIM") as u8,
                            grup: int(&row, "GRUP") as u8,
                            objl: int(&row, "OBJL") as u16,
                            rver: int(&row, "RVER") as u32,
                            ..Default::default()
                        };
                        if let Some(fo) = rec.field("FOID") {
                            let r = d.row(fo)?;
                            fe.foid = (int(&r, "AGEN") as u16, int(&r, "FIDN") as u32, int(&r, "FIDS") as u16);
                        }
                        if let Some(a) = rec.field("ATTF") {
                            fe.attf = d.attrs(a, false)?;
                        }
                        if let Some(a) = rec.field("NATF") {
                            fe.natf = d.attrs(a, true)?;
                        }
                        for p in rec.fields_named("FSPT") {
                            fe.fspt.extend(d.spatial_ptrs(p)?);
                        }
                        for p in rec.fields_named("FFPT") {
                            fe.ffpt.extend(d.feature_ptrs(p)?);
                        }
                        feature_index.insert(rcid, self.features.len());
                        self.features.push(fe);
                    }
                }
            }
        }
        Ok(())
    }

    /// The live features: deleted ones (tombstoned by an update) left out.
    pub fn live_features(&self) -> impl Iterator<Item = &FeatureRec> {
        self.features.iter().filter(|f| f.objl != 0 || f.prim != 0)
    }

    /// A vector record's coordinates as `[lon, lat]` degrees (and depth in
    /// metres for soundings).
    pub fn lonlat(&self, c: [i32; 3]) -> [f64; 3] {
        let comf = if self.dataset.comf > 0.0 { self.dataset.comf } else { 10_000_000.0 };
        let somf = if self.dataset.somf > 0.0 { self.dataset.somf } else { 10.0 };
        [c[1] as f64 / comf, c[0] as f64 / comf, c[2] as f64 / somf]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(n: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/s57/US4MA1BD.{n}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn a_base_cell_reads_its_parameters_vectors_and_features() {
        let cell = Cell::parse_base(&fixture("000")).unwrap();
        assert_eq!(cell.dataset.dsnm, "US4MA1BD.000");
        assert!(cell.dataset.comf >= 1e6, "COMF {}", cell.dataset.comf);
        assert!(cell.dataset.cscl > 0);
        assert!(!cell.vectors.is_empty());
        assert!(cell.live_features().count() > 0);
        // Every feature's spatial pointers resolve to a vector record.
        for f in cell.live_features() {
            for p in &f.fspt {
                assert!(cell.vectors.contains_key(&p.name), "dangling {:?}", p.name);
            }
        }
        // Coordinates land off Massachusetts.
        let v = cell.vectors.values().find(|v| !v.coords.is_empty()).unwrap();
        let [lon, lat, _] = cell.lonlat(v.coords[0]);
        assert!((-72.0..-69.0).contains(&lon) && (40.0..43.5).contains(&lat), "{lon},{lat}");
    }

    #[test]
    fn updates_apply_in_sequence() {
        let path = std::path::PathBuf::from(format!(
            "{}/tests/fixtures/s57/US4MA1BD.000",
            env!("CARGO_MANIFEST_DIR")
        ));
        let base = Cell::parse_base(&fixture("000")).unwrap();
        let cell = Cell::open(&path).unwrap();
        assert!(cell.updates_applied >= 1);
        assert!(cell.dataset.update > base.dataset.update);
        for f in cell.live_features() {
            for p in &f.fspt {
                assert!(cell.vectors.contains_key(&p.name), "dangling {:?} after updates", p.name);
            }
        }
    }

    #[test]
    fn update_lists_edit_in_place() {
        let mut v = vec![1, 2, 3, 4];
        edit_list(&mut v, 1, 2, 2, &[9, 8]);
        assert_eq!(v, vec![1, 9, 8, 2, 3, 4]);
        edit_list(&mut v, 2, 1, 2, &[]);
        assert_eq!(v, vec![8, 2, 3, 4]);
        edit_list(&mut v, 3, 4, 1, &[7]);
        assert_eq!(v, vec![8, 2, 3, 7]);
        let mut a = vec![(1u16, "a".to_string()), (2, "b".into())];
        merge_attrs(&mut a, vec![(1, DELETE.into()), (2, "c".into()), (3, "d".into())]);
        assert_eq!(a, vec![(2, "c".to_string()), (3, "d".into())]);
    }
}
