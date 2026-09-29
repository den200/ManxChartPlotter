//! An S-57 cell written as SENC — the byte stream OpenCPN's `Osenc` builds
//! from S-57, and the one Manx's SENC reader already understands.
//!
//! Going through SENC rather than straight to `ChartData` keeps one reader
//! and one chart model for both chart sources, and lets a converted cell be
//! cached like a decrypted o-charts cell. The layout follows OpenCPN's
//! `Osenc.cpp` (`createSenc200`, `CreateSENCRecord200`), version 200, with
//! three deliberate simplifications that Manx's reader does not see:
//! areas are written as plain triangle lists (no strips or fans), edges are
//! written whole — their end nodes included — so lines and rings resolve
//! complete, and no level-of-detail thinning is applied.
//!
//! Frames: points are raw WGS84 (latitude first); everything else is Simple
//! Mercator metres relative to the centre of the cell's coverage, the same
//! projection as `tiles::latlon_to_mercator`.

use std::collections::HashMap;

use super::attr_types::{attr_type, AttrType};
use super::cell::{Cell, FeatureRec, Name, RCNM_VC, RCNM_VE, RCNM_VI};

/// The SENC version written. 200: edge references are 12 bytes and a
/// reversed edge is marked by a negative index, the form OpenCPN's open
/// `Osenc` writes.
const SENC_VERSION: u16 = 200;

const OBJL_M_COVR: u16 = 302;
const OBJL_SOUNDG: u16 = 129;
const ATTR_CATCOV: u16 = 18;

struct Writer {
    out: Vec<u8>,
}

impl Writer {
    fn record(&mut self, kind: u16, payload: &[u8]) {
        self.out.extend_from_slice(&kind.to_le_bytes());
        self.out.extend_from_slice(&((payload.len() + 6) as u32).to_le_bytes());
        self.out.extend_from_slice(payload);
    }
}

#[derive(Default)]
struct Buf(Vec<u8>);

impl Buf {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn i32(&mut self, v: i32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn f32(&mut self, v: f32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn f64(&mut self, v: f64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
    fn cstr(&mut self, s: &str) -> &mut Self {
        self.0.extend_from_slice(s.as_bytes());
        self.0.push(0);
        self
    }
}

/// A bounding box in degrees.
#[derive(Clone, Copy)]
struct Bounds {
    s: f64,
    n: f64,
    w: f64,
    e: f64,
}

impl Bounds {
    fn empty() -> Self {
        Self {
            s: f64::MAX,
            n: f64::MIN,
            w: f64::MAX,
            e: f64::MIN,
        }
    }
    fn add(&mut self, lon: f64, lat: f64) {
        self.s = self.s.min(lat);
        self.n = self.n.max(lat);
        self.w = self.w.min(lon);
        self.e = self.e.max(lon);
    }
    fn is_empty(&self) -> bool {
        self.s > self.n
    }
    fn write(&self, b: &mut Buf) {
        b.f64(self.s).f64(self.n).f64(self.w).f64(self.e);
    }
}

/// An edge laid out: its full polyline (end nodes included), in lon/lat,
/// and the RCIDs of its beginning and end nodes.
struct EdgeGeom {
    pts: Vec<[f64; 2]>,
    begin: u32,
    end: u32,
}

/// One edge as a feature uses it.
struct UsedEdge {
    rcid: u32,
    reversed: bool,
    /// 1 exterior, 2 interior, 3 exterior truncated by the data limit.
    usag: u8,
}

struct Encoder<'a> {
    cell: &'a Cell,
    ref_lat: f64,
    ref_lon: f64,
    ref_m: (f64, f64),
    edges: HashMap<u32, EdgeGeom>,
}

impl Encoder<'_> {
    fn lonlat(&self, name: Name) -> Option<Vec<[f64; 3]>> {
        let v = self.cell.vectors.get(&name)?;
        Some(v.coords.iter().map(|c| self.cell.lonlat(*c)).collect())
    }

    /// Simple Mercator metres relative to the cell's reference point.
    fn sm(&self, lon: f64, lat: f64) -> [f64; 2] {
        // Across the antimeridian, keep the longitude on the reference's side.
        let mut lon = lon;
        if lon * self.ref_lon < 0.0 && (lon - self.ref_lon).abs() > 180.0 {
            lon += 360.0 * self.ref_lon.signum();
        }
        let (x, y) = crate::tiles::latlon_to_mercator(lat, lon);
        [x - self.ref_m.0, y - self.ref_m.1]
    }

    fn used_edges(&self, f: &FeatureRec) -> Vec<UsedEdge> {
        f.fspt
            .iter()
            .filter(|p| p.name.0 == RCNM_VE && self.edges.contains_key(&p.name.1))
            .map(|p| UsedEdge {
                rcid: p.name.1,
                reversed: p.ornt == 2,
                usag: p.usag,
            })
            .collect()
    }

    /// The oriented polyline of one used edge.
    fn oriented(&self, e: &UsedEdge) -> (Vec<[f64; 2]>, u32, u32) {
        let g = &self.edges[&e.rcid];
        if e.reversed {
            let mut p = g.pts.clone();
            p.reverse();
            (p, g.end, g.begin)
        } else {
            (g.pts.clone(), g.begin, g.end)
        }
    }

    fn edge_refs(&self, used: &[UsedEdge], b: &mut Buf) {
        for e in used {
            let g = &self.edges[&e.rcid];
            // OpenCPN's order: forward (begin, +edge, end); reversed
            // (end, -edge, begin).
            if e.reversed {
                b.u32(g.end).i32(-(e.rcid as i32)).u32(g.begin);
            } else {
                b.u32(g.begin).i32(e.rcid as i32).u32(g.end);
            }
        }
    }

    /// Chain a feature's edges into closed rings, each tagged exterior or
    /// interior. Edges come in any order; a ring follows shared nodes.
    fn rings(&self, used: &[UsedEdge]) -> Vec<(Vec<[f64; 2]>, bool)> {
        let mut parts: Vec<(Vec<[f64; 2]>, u32, u32, bool)> = used
            .iter()
            .map(|e| {
                let (p, a, b) = self.oriented(e);
                (p, a, b, e.usag != 2)
            })
            .collect();
        let mut rings = Vec::new();
        while let Some((mut ring, first, mut last, exterior)) = parts.pop() {
            let mut guard = parts.len() + 1;
            while last != first && guard > 0 {
                guard -= 1;
                let next = parts.iter().position(|p| p.1 == last).map(|i| (i, false)).or_else(|| {
                    parts.iter().position(|p| p.2 == last).map(|i| (i, true))
                });
                let Some((i, flip)) = next else { break };
                let (mut p, a, b, _) = parts.remove(i);
                let end = if flip {
                    p.reverse();
                    a
                } else {
                    b
                };
                ring.extend(p.into_iter().skip(1));
                last = end;
            }
            if ring.len() >= 3 {
                if ring.first() != ring.last() {
                    let f = ring[0];
                    ring.push(f);
                }
                rings.push((ring, exterior));
            }
        }
        rings
    }
}

fn point_in_ring(p: [f64; 2], ring: &[[f64; 2]]) -> bool {
    let mut inside = false;
    let mut j = ring.len() - 1;
    for i in 0..ring.len() {
        let (a, b) = (ring[i], ring[j]);
        if (a[1] > p[1]) != (b[1] > p[1])
            && p[0] < (b[0] - a[0]) * (p[1] - a[1]) / (b[1] - a[1]) + a[0]
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn ring_area(r: &[[f64; 2]]) -> f64 {
    r.windows(2).map(|w| w[0][0] * w[1][1] - w[1][0] * w[0][1]).sum::<f64>() / 2.0
}

/// Encode a cell as SENC bytes.
pub fn encode(cell: &Cell) -> Vec<u8> {
    encode_with_ids(cell).0
}

/// [`encode`], also returning the S-57 record id (FRID RCID) of each feature
/// written, in order — so a feature of the parsed chart can be traced back
/// to its record, as the conformance dump does.
pub fn encode_with_ids(cell: &Cell) -> (Vec<u8>, Vec<u32>) {
    let mut ids = Vec::new();
    // The cell's extent: its M_COVR coverage (CATCOV = 1), as OpenCPN takes
    // it; or, failing that, everything it contains.
    let mut extent = Bounds::empty();
    let mut probe = Encoder {
        cell,
        ref_lat: 0.0,
        ref_lon: 0.0,
        ref_m: (0.0, 0.0),
        edges: HashMap::new(),
    };
    probe.edges = edge_geometry(cell);
    for f in cell.live_features().filter(|f| f.objl == OBJL_M_COVR) {
        let catcov = f.attf.iter().find(|(c, _)| *c == ATTR_CATCOV).map(|(_, v)| v.trim());
        if catcov.is_some_and(|v| v != "1") {
            continue;
        }
        for e in probe.used_edges(f) {
            for p in probe.oriented(&e).0 {
                extent.add(p[0], p[1]);
            }
        }
    }
    if extent.is_empty() {
        for v in cell.vectors.values() {
            for c in &v.coords {
                let [lon, lat, _] = cell.lonlat(*c);
                extent.add(lon, lat);
            }
        }
    }
    if extent.is_empty() {
        extent = Bounds { s: 0.0, n: 0.0, w: 0.0, e: 0.0 };
    }
    let ref_lat = (extent.s + extent.n) / 2.0;
    let ref_lon = (extent.w + extent.e) / 2.0;
    let enc = Encoder {
        ref_lat,
        ref_lon,
        ref_m: crate::tiles::latlon_to_mercator(ref_lat, ref_lon),
        ..probe
    };
    let _ = enc.ref_lat;

    let mut w = Writer { out: Vec::new() };
    // Header, in Osenc's order.
    w.record(1, &Buf::default().u16(SENC_VERSION).take());
    let name = cell.dataset.dsnm.trim_end_matches(".000").to_string();
    w.record(2, &Buf::default().cstr(&name).take());
    w.record(3, &Buf::default().cstr(&cell.dataset.issue_date).take());
    w.record(4, &Buf::default().u16(cell.dataset.edition as u16).take());
    w.record(6, &Buf::default().u16(cell.dataset.update as u16).take());
    w.record(7, &Buf::default().u32(cell.dataset.cscl).take());
    let mut ext = Buf::default();
    ext.f64(extent.s).f64(extent.w); // SW
    ext.f64(extent.n).f64(extent.w); // NW
    ext.f64(extent.n).f64(extent.e); // NE
    ext.f64(extent.s).f64(extent.e); // SE
    w.record(100, &ext.0);

    for (fid, f) in cell.live_features().enumerate() {
        let Some(geometry) = enc.geometry(f) else { continue };
        let prim = match geometry.0 {
            80 => 1,
            81 => 2,
            82 => 3,
            _ => 4,
        };
        w.record(64, &Buf::default().u16(f.objl).u16(fid as u16).u8(prim).take());
        ids.push(f.rcid);
        for (code, value) in f.attf.iter().chain(f.natf.iter()) {
            if let Some(payload) = attribute(*code, value) {
                w.record(65, &payload);
            }
        }
        w.record(geometry.0, &geometry.1);
    }

    // The vector tables every line and area refers to.
    let mut edges = Buf::default();
    edges.u32(enc.edges.len() as u32);
    let mut edge_ids: Vec<&u32> = enc.edges.keys().collect();
    edge_ids.sort();
    for id in edge_ids {
        let g = &enc.edges[id];
        edges.u32(*id).u32(g.pts.len() as u32);
        for p in &g.pts {
            let [x, y] = enc.sm(p[0], p[1]);
            edges.f32(x as f32).f32(y as f32);
        }
    }
    w.record(96, &edges.0);
    let mut nodes = Buf::default();
    let mut vc: Vec<&Name> = cell.vectors.keys().filter(|n| n.0 == RCNM_VC).collect();
    vc.sort();
    nodes.u32(vc.len() as u32);
    for n in vc {
        let Some(c) = cell.vectors[n].coords.first() else {
            nodes.u32(n.1).f32(0.0).f32(0.0);
            continue;
        };
        let [lon, lat, _] = cell.lonlat(*c);
        let [x, y] = enc.sm(lon, lat);
        nodes.u32(n.1).f32(x as f32).f32(y as f32);
    }
    w.record(97, &nodes.0);
    w.out.extend_from_slice(&[0; 6]);
    (w.out, ids)
}

/// Every edge laid out once: beginning node, interior points, end node.
fn edge_geometry(cell: &Cell) -> HashMap<u32, EdgeGeom> {
    let node = |name: Name| -> Option<[f64; 2]> {
        let v = cell.vectors.get(&name)?;
        let [lon, lat, _] = cell.lonlat(*v.coords.first()?);
        Some([lon, lat])
    };
    let mut out = HashMap::new();
    for v in cell.vectors.values().filter(|v| v.name.0 == RCNM_VE) {
        // TOPI names the ends; without it, the first pointer begins.
        let nodes: Vec<_> = v.ptrs.iter().filter(|p| p.name.0 == RCNM_VC).collect();
        let begin = nodes
            .iter()
            .find(|p| p.topi == 1)
            .or(nodes.first())
            .map(|p| p.name);
        let end = nodes
            .iter()
            .find(|p| p.topi == 2)
            .or(nodes.get(1))
            .or(nodes.first())
            .map(|p| p.name);
        let (Some(begin), Some(end)) = (begin, end) else { continue };
        let (Some(a), Some(b)) = (node(begin), node(end)) else { continue };
        let mut pts = vec![a];
        pts.extend(v.coords.iter().map(|c| {
            let [lon, lat, _] = cell.lonlat(*c);
            [lon, lat]
        }));
        pts.push(b);
        out.insert(
            v.name.1,
            EdgeGeom {
                pts,
                begin: begin.1,
                end: end.1,
            },
        );
    }
    out
}

/// One attribute record's payload, typed as the S-57 catalogue types it.
/// An empty value writes nothing — OpenCPN drops those too.
fn attribute(code: u16, value: &str) -> Option<Vec<u8>> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    let mut b = Buf::default();
    b.u16(code);
    match attr_type(code) {
        AttrType::Int if v.parse::<i32>().is_ok() => {
            b.u8(0).i32(v.parse().unwrap());
        }
        AttrType::Float if v.parse::<f64>().is_ok() => {
            b.u8(2).f64(v.parse().unwrap());
        }
        _ => {
            b.u8(4).cstr(value);
        }
    }
    Some(b.0)
}

impl Encoder<'_> {
    /// The feature's geometry record: (record type, payload). `None` for a
    /// feature with nothing to draw (collection objects, broken pointers).
    fn geometry(&self, f: &FeatureRec) -> Option<(u16, Vec<u8>)> {
        match f.prim {
            1 => {
                let nodes: Vec<Name> = f
                    .fspt
                    .iter()
                    .map(|p| p.name)
                    .filter(|n| n.0 == RCNM_VI || n.0 == RCNM_VC)
                    .collect();
                let three_d = nodes
                    .iter()
                    .any(|n| self.cell.vectors.get(n).is_some_and(|v| v.three_d));
                if f.objl == OBJL_SOUNDG || three_d {
                    // Soundings: a multipoint, depths in metres.
                    let pts: Vec<[f64; 3]> = nodes.iter().filter_map(|n| self.lonlat(*n)).flatten().collect();
                    if pts.is_empty() {
                        return None;
                    }
                    let mut bounds = Bounds::empty();
                    for p in &pts {
                        bounds.add(p[0], p[1]);
                    }
                    let mut b = Buf::default();
                    bounds.write(&mut b);
                    b.u32(pts.len() as u32);
                    for p in &pts {
                        let [x, y] = self.sm(p[0], p[1]);
                        b.f32(x as f32).f32(y as f32).f32(p[2] as f32);
                    }
                    return Some((83, b.0));
                }
                let [lon, lat, _] = *self.lonlat(*nodes.first()?)?.first()?;
                // Raw degrees, latitude first — the one frame exception.
                Some((80, Buf::default().f64(lat).f64(lon).take()))
            }
            2 => {
                let used = self.used_edges(f);
                if used.is_empty() {
                    return None;
                }
                let mut bounds = Bounds::empty();
                for e in &used {
                    for p in &self.edges[&e.rcid].pts {
                        bounds.add(p[0], p[1]);
                    }
                }
                let mut b = Buf::default();
                bounds.write(&mut b);
                b.u32(used.len() as u32);
                self.edge_refs(&used, &mut b);
                Some((81, b.0))
            }
            3 => self.area(f),
            _ => None,
        }
    }

    fn area(&self, f: &FeatureRec) -> Option<(u16, Vec<u8>)> {
        let used = self.used_edges(f);
        if used.is_empty() {
            return None;
        }
        let rings = self.rings(&used);
        if rings.is_empty() {
            return None;
        }
        let mut bounds = Bounds::empty();
        for (r, _) in &rings {
            for p in r {
                bounds.add(p[0], p[1]);
            }
        }
        // Rings in plane metres, for the triangulation.
        let plane: Vec<(Vec<[f64; 2]>, bool)> = rings
            .iter()
            .map(|(r, ext)| (r.iter().map(|p| self.sm(p[0], p[1])).collect(), *ext))
            .collect();
        // Exterior rings; if none is marked, the largest ring is.
        let mut exterior: Vec<usize> = (0..plane.len()).filter(|&i| plane[i].1).collect();
        if exterior.is_empty() {
            let biggest = (0..plane.len())
                .max_by(|&a, &b| ring_area(&plane[a].0).abs().total_cmp(&ring_area(&plane[b].0).abs()))?;
            exterior.push(biggest);
        }
        let mut triangles: Vec<[f64; 2]> = Vec::new();
        for &e in &exterior {
            let outer = &plane[e].0;
            let holes: Vec<&Vec<[f64; 2]>> = (0..plane.len())
                .filter(|&i| !exterior.contains(&i))
                .map(|i| &plane[i].0)
                .filter(|h| h.first().is_some_and(|p| point_in_ring(*p, outer)))
                .collect();
            let mut flat: Vec<f64> = Vec::new();
            let mut hole_idx = Vec::new();
            let push_ring = |flat: &mut Vec<f64>, r: &[[f64; 2]]| {
                // earcut wants the ring open.
                let n = if r.len() > 1 && r.first() == r.last() { r.len() - 1 } else { r.len() };
                for p in &r[..n] {
                    flat.push(p[0]);
                    flat.push(p[1]);
                }
            };
            push_ring(&mut flat, outer);
            for h in &holes {
                hole_idx.push(flat.len() / 2);
                push_ring(&mut flat, h);
            }
            let Ok(idx) = earcutr::earcut(&flat, &hole_idx, 2) else { continue };
            for i in idx {
                triangles.push([flat[2 * i], flat[2 * i + 1]]);
            }
        }
        if triangles.is_empty() {
            return None;
        }

        let mut b = Buf::default();
        bounds.write(&mut b);
        b.u32(rings.len() as u32); // contours
        b.u32(1); // one TriPrim: a triangle list
        b.u32(used.len() as u32);
        for (r, _) in &rings {
            b.u32(r.len() as u32);
        }
        b.u8(0x04).u32(triangles.len() as u32);
        // The primitive's own bounds, longitude first as Osenc writes them.
        b.f64(bounds.w).f64(bounds.e).f64(bounds.s).f64(bounds.n);
        for p in &triangles {
            b.f32(p[0] as f32).f32(p[1] as f32);
        }
        self.edge_refs(&used, &mut b);
        Some((82, b.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::senc::{ChartData, FeatureType};

    fn fixture() -> ChartData {
        let path = std::path::PathBuf::from(format!(
            "{}/tests/fixtures/s57/US4MA1BD.000",
            env!("CARGO_MANIFEST_DIR")
        ));
        let cell = Cell::open(&path).unwrap();
        ChartData::parse(encode(&cell)).expect("the SENC reader accepts it")
    }

    /// The encoded cell reads back through the ordinary SENC reader, with
    /// its scale, a reference point inside its coverage, and every kind of
    /// geometry the chart needs.
    #[test]
    fn an_s57_cell_reads_back_as_senc() {
        let chart = fixture();
        assert!(chart.header.native_scale > 0);
        assert!((40.0..44.0).contains(&chart.header.ref_lat), "{}", chart.header.ref_lat);
        assert!((-72.0..-69.0).contains(&chart.header.ref_lon), "{}", chart.header.ref_lon);
        let has = |t: FeatureType| chart.features.iter().any(|f| f.feature_type == t);
        assert!(has(FeatureType::Area), "areas");
        assert!(has(FeatureType::Line) || has(FeatureType::Point), "lines or points");
        // Every area carries triangles.
        for f in chart.features.iter().filter(|f| f.feature_type == FeatureType::Area) {
            let g = f.area_geometry.as_ref().unwrap();
            assert!(g.triangles.iter().any(|t| t.triangle_count() > 0));
        }
    }
}
