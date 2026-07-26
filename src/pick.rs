//! Identify the chart objects under a point — the query behind the info bubble.
//!
//! Everything shown comes out of the SENC. A feature carries its S-57 class and
//! its attributes, and [`crate::senc::s57_names`] turns those into the wording
//! OpenCPN's object query uses. Two attributes point outside the cell: `TXTDSC`
//! and `NTXTDS` name a `.TXT` beside the chart holding a caution or a note, and
//! those are read on demand.
//!
//! Picking is done against the geometry the SENC stores rather than against
//! anything the renderer built, so it does not depend on which tiles happen to
//! be resident, and it works the same at any zoom.

use std::path::Path;

use crate::senc::{
    s57_code_to_acronym,
    s57_names::{attribute_value_name, object_class_name},
    AttributeValue, ChartData, ChartInfo, Feature, FeatureType,
};

/// One object under the cursor.
#[derive(Debug, Clone)]
pub struct PickedObject {
    /// S-57 class acronym, e.g. `BOYCAR`.
    pub acronym: String,
    /// The class's full name, e.g. "Buoy, cardinal". Falls back to the acronym.
    pub title: String,
    /// Which cell it came from, and at what compilation scale.
    pub chart: String,
    pub chart_scale: u32,
    pub geometry: FeatureType,
    /// Attributes, in the order the standard lists them, already decoded.
    pub attributes: Vec<(String, String)>,
    /// Chart notes named by TXTDSC/NTXTDS.
    pub notes: Vec<String>,
    /// How far the cursor was from the object, in metres. Areas report 0 when
    /// the cursor is inside them.
    pub distance_m: f64,
}

/// Chart objects at a point, nearest and most specific first.
///
/// `tolerance_m` is how close a point or a line has to be to count; it should
/// be a handful of screen pixels converted to metres, so the tolerance follows
/// the zoom the way a mariner expects.
///
/// Ordering is by geometry before distance: a buoy sitting on a depth area is
/// what you meant to click, even though the depth area also contains the point.
pub fn pick_at(
    chart: &ChartData,
    info: &ChartInfo,
    point: [f64; 2],
    tolerance_m: f64,
) -> Vec<PickedObject> {
    let (ref_mx, ref_my) = crate::tiles::latlon_to_mercator(info.ref_lat, info.ref_lon);
    let mut out = Vec::new();

    for feature in &chart.features {
        let hit = match feature.feature_type {
            FeatureType::Point => point_hit(feature, point, tolerance_m),
            FeatureType::Multipoint => multipoint_hit(feature, ref_mx, ref_my, point, tolerance_m),
            FeatureType::Line => line_hit(feature, chart, ref_mx, ref_my, point, tolerance_m),
            FeatureType::Area => area_hit(feature, info, point),
        };
        let Some(distance_m) = hit else { continue };
        out.push(describe(feature, info, distance_m));
    }

    sort_picks(&mut out);
    out
}

/// Nearest and most specific first, and where two cells hold the same object,
/// the finer-scale one: a 1:4000 harbour plan says more about a berth than a
/// 1:180000 overview does.
pub fn sort_picks(picks: &mut [PickedObject]) {
    picks.sort_by(|a, b| {
        specificity(a.geometry, &a.acronym)
            .cmp(&specificity(b.geometry, &b.acronym))
            .then(a.distance_m.total_cmp(&b.distance_m))
            .then(a.chart_scale.cmp(&b.chart_scale))
    });
}

/// Lower sorts first: points, then lines, then areas, with the meta objects
/// last of all.
///
/// A click on a harbour lands inside M_NPUB, M_NSYS and M_QUAL as surely as it
/// lands on the berth, but those describe the *cell* — which publication it
/// came from, which buoyage system is in force — not the thing under the
/// cursor. They belong in the answer, at the bottom of it.
fn specificity(kind: FeatureType, acronym: &str) -> u8 {
    let meta = acronym.starts_with("M_") || acronym.starts_with("C_");
    let base = match kind {
        FeatureType::Point | FeatureType::Multipoint => 0,
        FeatureType::Line => 1,
        FeatureType::Area => 2,
    };
    if meta {
        base + 4
    } else {
        base
    }
}

fn point_hit(feature: &Feature, at: [f64; 2], tol: f64) -> Option<f64> {
    let pg = feature.point_geometry.as_ref()?;
    let (mx, my) = crate::tiles::latlon_to_mercator(pg.x, pg.y);
    let d = ((mx - at[0]).powi(2) + (my - at[1]).powi(2)).sqrt();
    (d <= tol).then_some(d)
}

fn multipoint_hit(feature: &Feature, ref_mx: f64, ref_my: f64, at: [f64; 2], tol: f64) -> Option<f64> {
    let mp = feature.multipoint_geometry.as_ref()?;
    let mut best = f64::MAX;
    for p in &mp.points {
        // Multipoint vertices are SM metres relative to the cell's reference
        // point, like every other stored geometry — not lat/lon.
        let (mx, my) = (ref_mx + p[0] as f64, ref_my + p[1] as f64);
        let d = ((mx - at[0]).powi(2) + (my - at[1]).powi(2)).sqrt();
        best = best.min(d);
    }
    (best <= tol).then_some(best)
}

/// How much more generous the grab is for a line than for a point.
///
/// A line is a hairline; a fingertip is nine millimetres. Aiming at a depth
/// contour with the same tolerance that finds a buoy is a game of darts. The
/// answer is a list ranked by distance rather than a single hit, so a fat grab
/// costs nothing: the line joins the list, and a point that is genuinely under
/// the finger still sorts above it.
const LINE_GRAB: f64 = 4.0;

fn line_hit(
    feature: &Feature,
    chart: &ChartData,
    ref_mx: f64,
    ref_my: f64,
    at: [f64; 2],
    tol: f64,
) -> Option<f64> {
    let tol = tol * LINE_GRAB;
    let geom = feature.line_geometry.as_ref()?;
    let mut best = f64::MAX;
    for ring in geom.resolve(&chart.edge_table) {
        for w in ring.windows(2) {
            let a = [ref_mx + w[0][0] as f64, ref_my + w[0][1] as f64];
            let b = [ref_mx + w[1][0] as f64, ref_my + w[1][1] as f64];
            best = best.min(distance_to_segment(at, a, b));
        }
    }
    (best <= tol).then_some(best)
}

fn area_hit(feature: &Feature, info: &ChartInfo, at: [f64; 2]) -> Option<f64> {
    let geom = feature.area_geometry.as_ref()?;
    let mut inside = false;
    geom.for_each_triangle_global(info.ref_lat, info.ref_lon, |tri| {
        if !inside && point_in_triangle(at, tri) {
            inside = true;
        }
    });
    inside.then_some(0.0)
}

fn distance_to_segment(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (vx, vy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = vx * vx + vy * vy;
    let t = if len2 <= f64::EPSILON {
        0.0
    } else {
        (((p[0] - a[0]) * vx + (p[1] - a[1]) * vy) / len2).clamp(0.0, 1.0)
    };
    let (cx, cy) = (a[0] + t * vx, a[1] + t * vy);
    ((p[0] - cx).powi(2) + (p[1] - cy).powi(2)).sqrt()
}

fn point_in_triangle(p: [f64; 2], t: [[f64; 2]; 3]) -> bool {
    let side = |a: [f64; 2], b: [f64; 2]| {
        (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])
    };
    let (d0, d1, d2) = (side(t[0], t[1]), side(t[1], t[2]), side(t[2], t[0]));
    let neg = d0 < 0.0 || d1 < 0.0 || d2 < 0.0;
    let pos = d0 > 0.0 || d1 > 0.0 || d2 > 0.0;
    !(neg && pos)
}

fn describe(feature: &Feature, info: &ChartInfo, distance_m: f64) -> PickedObject {
    let acronym = s57_code_to_acronym(feature.type_code).to_string();
    let title = object_class_name(&acronym).unwrap_or(&acronym).to_string();

    let mut attributes = Vec::new();
    let mut notes = Vec::new();
    for (name, value) in feature.attributes.iter() {
        // Housekeeping, not chart content: `catgeo` (code 50000) records which
        // geometric primitive the object was encoded as, which the reader
        // already knows and a mariner has no use for.
        if name == "catgeo" {
            continue;
        }
        // The two attributes that point at a file rather than carrying a value.
        if matches!(name, "TXTDSC" | "NTXTDS") {
            if let AttributeValue::String(file) = value {
                if let Some(text) = read_note(&info.path, file) {
                    notes.push(text);
                    continue;
                }
            }
        }
        attributes.push((name.to_string(), render_value(name, value)));
    }

    PickedObject {
        acronym,
        title,
        // These cells leave the cell name blank in the header, so fall back to
        // the file it came from — the mariner needs to know which chart is
        // talking, and "" does not tell them.
        chart: if info.name.is_empty() {
            info.path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            info.name.clone()
        },
        chart_scale: info.native_scale,
        geometry: feature.feature_type,
        attributes,
        notes,
        distance_m,
    }
}

/// An attribute value as a mariner should read it.
///
/// Enumerated values become their meaning; a comma-separated list of them —
/// S-57 allows several, a buoy can be red *and* white — becomes a list of
/// meanings. Anything else is passed through.
fn render_value(name: &str, value: &AttributeValue) -> String {
    match value {
        AttributeValue::Integer(v) => (*v)
            .try_into()
            .ok()
            .and_then(|v: u32| attribute_value_name(name, v))
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string()),
        AttributeValue::Float(v) => {
            if (v.fract()).abs() < 1e-9 {
                format!("{:.0}", v)
            } else {
                format!("{}", v)
            }
        }
        AttributeValue::String(s) => {
            let parts: Vec<&str> = s.split(',').map(str::trim).filter(|p| !p.is_empty()).collect();
            let decoded: Vec<String> = parts
                .iter()
                .map(|p| {
                    p.parse::<u32>()
                        .ok()
                        .and_then(|v| attribute_value_name(name, v))
                        .map(str::to_string)
                        .unwrap_or_else(|| (*p).to_string())
                })
                .collect();
            if decoded.is_empty() {
                s.clone()
            } else {
                decoded.join(", ")
            }
        }
    }
}

/// A chart note, read from the `.TXT` beside the cell.
///
/// The cells ship these in a directory named after the cell — `OC-45-AAVIN5/
/// DKNO7001.TXT` — holding the cautions and regulatory text a paper chart
/// prints in its margin.
fn read_note(chart_path: &Path, file: &str) -> Option<String> {
    let stem = chart_path.file_stem()?;
    let dir = chart_path.parent()?.join(stem);
    let text = std::fs::read(dir.join(file))
        .or_else(|_| std::fs::read(dir.join(file.to_uppercase())))
        .ok()?;
    let text = String::from_utf8_lossy(&text).trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_to_segment_handles_the_ends_and_the_middle() {
        let a = [0.0, 0.0];
        let b = [10.0, 0.0];
        assert!((distance_to_segment([5.0, 3.0], a, b) - 3.0).abs() < 1e-9);
        assert!((distance_to_segment([-4.0, 0.0], a, b) - 4.0).abs() < 1e-9);
        assert!((distance_to_segment([14.0, 0.0], a, b) - 4.0).abs() < 1e-9);
        // A degenerate segment is a point, not a division by zero.
        assert!((distance_to_segment([3.0, 4.0], a, a) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn point_in_triangle_includes_the_edge() {
        let t = [[0.0, 0.0], [10.0, 0.0], [0.0, 10.0]];
        assert!(point_in_triangle([1.0, 1.0], t));
        assert!(point_in_triangle([0.0, 5.0], t));
        assert!(!point_in_triangle([6.0, 6.0], t));
    }

    #[test]
    fn points_outrank_lines_which_outrank_areas_and_meta_comes_last() {
        assert!(specificity(FeatureType::Point, "BOYCAR") < specificity(FeatureType::Line, "DEPCNT"));
        assert!(specificity(FeatureType::Line, "DEPCNT") < specificity(FeatureType::Area, "DEPARE"));
        // A meta object sorts below every real chart object, whatever its
        // geometry: a cell-wide note is not what the cursor is pointing at.
        assert!(specificity(FeatureType::Area, "DEPARE") < specificity(FeatureType::Point, "M_NPUB"));
    }

    #[test]
    fn enumerated_values_are_decoded_singly_and_in_lists() {
        assert_eq!(
            render_value("CATCAM", &AttributeValue::Integer(1)),
            "north cardinal mark"
        );
        // COLOUR is routinely a list: red and white bands.
        assert_eq!(
            render_value("COLOUR", &AttributeValue::String("3,1".into())),
            "red, white"
        );
        // A value the standard does not enumerate passes through.
        assert_eq!(render_value("DRVAL1", &AttributeValue::Float(3.5)), "3.5");
        assert_eq!(render_value("DRVAL1", &AttributeValue::Float(6.0)), "6");
        assert_eq!(
            render_value("OBJNAM", &AttributeValue::String("Nivå Havn".into())),
            "Nivå Havn"
        );
    }
}
