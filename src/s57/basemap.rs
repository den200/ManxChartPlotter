//! The world basemap: the land of the whole world, drawn under the charts.
//!
//! Without it, everything outside the loaded cells is blank, and getting
//! from Danish charts to American ones means panning across an empty screen
//! with nothing to steer by. The land is Natural Earth's 1:50m set (public
//! domain), packed by `tools/basemap/make_basemap.py` into half a megabyte
//! of polygons that are built into the program.
//!
//! It becomes a chart like any other: a cell of LNDARE areas, turned into
//! SENC by the S-57 converter, entered in the catalogue at a compilation
//! scale of 1:50 000 000. The quilt then draws it only where no real chart
//! covers the view, and always beneath them. It is never used for routing
//! — the route planner builds its own catalogue from the chart folder — and
//! nobody should navigate by it.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::cell::{Cell, Dataset, FeatureRec, SpatialPtr, Vector, VectorPtr, RCNM_VC, RCNM_VE};
use crate::senc::{ChartInfo, SencReader};

/// The packed land polygons.
const LAND: &[u8] = include_bytes!("../../assets/basemap/land.bin");

/// The basemap's compilation scale, 1:N. Natural Earth's 1:50m set.
pub const SCALE: u32 = 50_000_000;

/// LNDARE's object class code.
const OBJL_LNDARE: u16 = 71;

/// The name the basemap goes by in the catalogue. Not a file: the chart
/// loader recognises it and takes the bytes from [`senc`].
pub fn path() -> PathBuf {
    PathBuf::from("manx:world-basemap")
}

/// Whether a catalogue entry is the basemap.
pub fn is_basemap(path: &Path) -> bool {
    // Or the id it had before the rename, in a catalogue cached then.
    path == Path::new("manx:world-basemap") || path == Path::new("navcore:world-basemap")
}

/// The basemap as SENC bytes, built on first use (a few tenths of a second)
/// and kept.
pub fn senc() -> &'static [u8] {
    static SENC: OnceLock<Vec<u8>> = OnceLock::new();
    SENC.get_or_init(|| super::senc_encode::encode(&land_cell()))
}

/// The basemap's catalogue entry.
pub fn chart_info() -> Option<ChartInfo> {
    let header = SencReader::parse_header_only(senc())
        .map_err(|e| log::warn!("world basemap: {e:?}"))
        .ok()?;
    ChartInfo::from_header(header, path())
}

/// The packed polygons as a cell: one LNDARE per polygon, one closed edge
/// per ring.
fn land_cell() -> Cell {
    let mut cell = Cell {
        dataset: Dataset {
            dsnm: "WORLD.000".into(),
            edition: 1,
            update: 0,
            issue_date: String::new(),
            comf: 10_000_000.0,
            somf: 10.0,
            cscl: SCALE,
            nall: 1,
            aall: 1,
        },
        ..Default::default()
    };
    let mut r = Reader { bytes: LAND, at: 0 };
    if r.take(8) != Some(b"NCLAND1\0".as_slice()) {
        log::warn!("world basemap: not a land file");
        return cell;
    }
    let mut next_id = 1u32;
    let polygons = r.u32().unwrap_or(0);
    for fid in 0..polygons {
        let Some(rings) = r.u32() else { break };
        let mut fspt = Vec::new();
        for ring in 0..rings {
            let Some(n) = r.u32() else { break };
            let mut pts = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let (Some(lon), Some(lat)) = (r.i32(), r.i32()) else { break };
                pts.push([lat, lon, 0]);
            }
            if pts.len() < 3 {
                continue;
            }
            // The ring closes on one node, which is its first point.
            let node = (RCNM_VC, next_id);
            let edge = (RCNM_VE, next_id + 1);
            next_id += 2;
            cell.vectors.insert(node, Vector { name: node, coords: vec![pts[0]], ..Default::default() });
            let ends = |topi| VectorPtr { name: node, ornt: 255, usag: 255, topi, mask: 255 };
            cell.vectors.insert(
                edge,
                Vector { name: edge, coords: pts[1..].to_vec(), ptrs: vec![ends(1), ends(2)], ..Default::default() },
            );
            fspt.push(SpatialPtr { name: edge, ornt: 1, usag: if ring == 0 { 1 } else { 2 }, mask: 255 });
        }
        if fspt.is_empty() {
            continue;
        }
        cell.features.push(FeatureRec {
            rcid: fid + 1,
            prim: 3,
            grup: 1,
            objl: OBJL_LNDARE,
            rver: 1,
            foid: (0, fid + 1, 0),
            fspt,
            ..Default::default()
        });
    }
    cell
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.bytes.get(self.at..self.at + n)?;
        self.at += n;
        Some(s)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The basemap is a world-wide chart of land at 1:50M, and it parses.
    #[test]
    fn the_basemap_is_a_world_chart_of_land() {
        let info = chart_info().expect("the basemap catalogues");
        assert_eq!(info.native_scale, SCALE);
        assert!(info.extent_wgs84.min_lon <= -179.0 && info.extent_wgs84.max_lon >= 179.0);
        assert!(info.extent_wgs84.min_lat < -80.0 && info.extent_wgs84.max_lat > 80.0);
        assert!(is_basemap(&info.path));
        let chart = crate::senc::ChartData::parse(senc().to_vec()).unwrap();
        assert!(chart.features.len() > 1000, "{} polygons", chart.features.len());
        assert!(chart.areas().all(|f| f.area_geometry.is_some()));
    }
}
