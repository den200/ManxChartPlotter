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
    /// The object read the way a mariner reads it, where the standard says how.
    ///
    /// A light carries eight attributes and means one thing: `Fl(1)G 3s 4M`.
    /// That is a light *list* entry, and it is what someone is looking for; the
    /// eight rows behind it are the chart's database. Composed by S-52's own
    /// LITDSN01, the same procedure that labels the light on the chart, so the
    /// bubble and the chart cannot disagree.
    pub summary: Option<String>,
    /// How many further copies of this object were folded into it — the same
    /// feature carried by other cells, or repeated within one. Zero for the
    /// common case; shown so the answer does not silently hide that the chart
    /// set says the same thing more than once.
    pub duplicates: usize,
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

/// Fold away objects the chart set says more than once.
///
/// A tap near a harbour light answers with fifty-odd objects, and most of them
/// are the same handful of things repeated: quilting means the light exists in
/// the 1:12000 plan, the 1:22000 approach and the 1:180000 overview, and a
/// shoreline is carried as many separate segments within one cell. Reading the
/// same light three times is not more information, it is less — the one entry
/// that differs gets lost among the copies.
///
/// Two objects are the same thing when they are the same class with the same
/// attributes. That is deliberately strict: a light and its sector partner
/// differ in `SECTR1`, two identical buoys a hundred metres apart differ in
/// nothing but position — and within a tap radius, two identical buoys are
/// very much more likely to be one buoy carried by two cells. What survives is
/// the nearest copy, and among equals the one from the finest chart, because a
/// harbour plan says more about a berth than an overview does.
pub fn dedupe_picks(picks: Vec<PickedObject>) -> Vec<PickedObject> {
    let mut out: Vec<PickedObject> = Vec::with_capacity(picks.len());
    for pick in picks {
        match out.iter_mut().find(|kept| same_object(kept, &pick)) {
            Some(kept) => {
                // Keep whichever copy is the better answer, but carry the
                // count and the finest scale across either way.
                kept.duplicates += 1;
                if pick.chart_scale > 0 && (kept.chart_scale == 0 || pick.chart_scale < kept.chart_scale) {
                    kept.chart = pick.chart;
                    kept.chart_scale = pick.chart_scale;
                }
                kept.distance_m = kept.distance_m.min(pick.distance_m);
                if kept.notes.is_empty() {
                    kept.notes = pick.notes;
                }
            }
            None => out.push(pick),
        }
    }
    out
}

fn same_object(a: &PickedObject, b: &PickedObject) -> bool {
    a.acronym == b.acronym
        && a.geometry == b.geometry
        && a.attributes.iter().filter(|(k, _)| !is_cell_bookkeeping(k)).eq(
            b.attributes.iter().filter(|(k, _)| !is_cell_bookkeeping(k)),
        )
}

/// Attributes that describe the *cell*, not the object in the water.
///
/// They differ legitimately between two cells carrying the same feature, so
/// comparing them defeats the whole point: before this, one daymark appeared
/// twice because the 1:22000 cell had drawn it with a `SCAMIN` and the 1:12000
/// cell had not.
///
/// `SCAMIN` is also the commonest attribute in a chart set and the least use to
/// anyone: it is the scale at which S-52 stops drawing the object. It is hidden
/// from the answer entirely. The source fields stay visible — provenance is
/// worth knowing — they simply do not decide identity.
pub fn is_cell_bookkeeping(name: &str) -> bool {
    matches!(name, "SCAMIN" | "SORDAT" | "SORIND" | "RECDAT" | "RECIND")
}

/// Attributes worth showing. Excludes the display-internal ones.
pub fn is_worth_showing(name: &str) -> bool {
    name != "SCAMIN"
}

/// Nearest and most specific first, and where two cells hold the same object,
/// the finer-scale one: a 1:4000 harbour plan says more about a berth than a
/// 1:180000 overview does.
pub fn sort_picks(picks: &mut [PickedObject]) {
    picks.sort_by(|a, b| {
        interest(&a.acronym)
            .cmp(&interest(&b.acronym))
            .then(is_overhead(&b.acronym).cmp(&is_overhead(&a.acronym)))
            .then(specificity(a.geometry, &a.acronym).cmp(&specificity(b.geometry, &b.acronym)))
            .then(a.distance_m.total_cmp(&b.distance_m))
            .then(a.chart_scale.cmp(&b.chart_scale))
    });
}

/// What a sailor tapping the chart most likely means, lower first.
///
/// A tap in a harbour mouth lands on a light, inside a caution area, on a
/// depth contour and in a depth area all at once. Sorted by geometry alone
/// the contour (a line) came before the caution area, and where no point
/// was hit the answer to "what is that?" opened with "Depth contour". So,
/// before geometry:
///
/// 0. what a mast must pass under — bridges, overhead cables — and then
///    aids to navigation: lights, buoys, beacons and what they carry;
/// 1. dangers — wrecks, rocks, obstructions;
/// 2. everything else charted on purpose: areas with rules, submarine
///    cables, berths, names;
/// 3. the seabed and the land themselves — depth areas and contours,
///    soundings, coastline;
/// 4. the cell's own description (`M_`, `C_`).
pub fn interest(acronym: &str) -> u8 {
    if acronym.starts_with("M_") || acronym.starts_with("C_") {
        return 4;
    }
    match acronym {
        // Whether the mast goes under is the first thing to know about a
        // bridge or a cable, and a tap on one usually lands nearer to one
        // of its lights than to anything else.
        a if is_overhead(a) => 0,
        "LIGHTS" | "LITFLT" | "LITVES" | "TOPMAR" | "DAYMAR" | "FOGSIG" | "RTPBCN" | "RDOSTA"
        | "RADRFL" | "PILBOP" => 0,
        a if a.starts_with("BOY") || a.starts_with("BCN") => 0,
        "WRECKS" | "UWTROC" | "OBSTRN" => 1,
        // Magnetic variation covers whole sea areas: true everywhere, the
        // answer to a tap nowhere.
        "DEPARE" | "DEPCNT" | "DRGARE" | "SOUNDG" | "LNDARE" | "COALNE" | "UNSARE" | "SBDARE"
        | "MAGVAR" => 3,
        _ => 2,
    }
}

/// Something a mast has to pass under.
pub fn is_overhead(acronym: &str) -> bool {
    matches!(acronym, "BRIDGE" | "CBLOHD" | "PIPOHD" | "CONVYR")
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
pub(crate) const LINE_GRAB: f64 = 4.0;

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

/// A one-line reading, for the classes the standard composes one for.
///
/// Only lights today. Adding a class here means finding the S-52 procedure
/// that already writes its label, not inventing a phrasing: the point is that
/// the bubble says exactly what the chart says.
fn summarise(acronym: &str, feature: &Feature, attrs: &[(String, String)]) -> Option<String> {
    if acronym != "LIGHTS" {
        return None;
    }
    let get = |k: &str| {
        attrs
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    };
    let mut text = crate::s52::litdsn01(feature)?;

    // For a *sectored* light LITDSN01 deliberately leaves out the colour and
    // the range, because one label cannot state a colour that changes with
    // bearing — the chart draws coloured arcs instead. A written answer has no
    // arcs, so it has to say them, or "Fl 3s 4m" hides that this is the green
    // sector of a light that is also red somewhere else.
    if let (Some(from), Some(to)) = (
        feature.attribute_float("SECTR1"),
        feature.attribute_float("SECTR2"),
    ) {
        if let Some(colour) = get("COLOUR") {
            text.push_str(&format!(", {colour}"));
        }
        text.push_str(&format!(" {from:.0}°–{to:.0}°"));
        if let Some(range) = get("VALNMR") {
            text.push_str(&format!(", {range} M"));
        }
    }
    (!text.trim().is_empty()).then_some(text)
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
        summary: summarise(&acronym, feature, &attributes),
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
        duplicates: 0,
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

    fn obj(acronym: &str, chart: &str, scale: u32, attrs: &[(&str, &str)]) -> PickedObject {
        PickedObject {
            acronym: acronym.into(),
            title: acronym.into(),
            chart: chart.into(),
            chart_scale: scale,
            geometry: FeatureType::Point,
            attributes: attrs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            notes: Vec::new(),
            distance_m: 10.0,
            summary: None,
            duplicates: 0,
        }
    }

    #[test]
    fn one_light_carried_by_three_cells_answers_once() {
        // Measured from the real chart set: a tap by Kalvebod light returned
        // the same light three times, once per cell that carries it.
        let light = &[("COLOUR", "green"), ("SIGPER", "3")];
        let picks = vec![
            obj("LIGHTS", "harbour-plan", 12000, light),
            obj("LIGHTS", "approach", 22000, light),
            obj("LIGHTS", "overview", 180000, light),
        ];
        let out = dedupe_picks(picks);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].duplicates, 2);
        // The finest chart wins: a harbour plan says more about a light than an
        // overview does.
        assert_eq!(out[0].chart, "harbour-plan");
        assert_eq!(out[0].chart_scale, 12000);
    }

    #[test]
    fn the_finest_chart_wins_whatever_order_they_arrive_in() {
        let light = &[("COLOUR", "green")];
        let out = dedupe_picks(vec![
            obj("LIGHTS", "overview", 180000, light),
            obj("LIGHTS", "harbour-plan", 12000, light),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chart, "harbour-plan");
    }

    #[test]
    fn scamin_does_not_make_two_objects_out_of_one() {
        // The bug this pins: one cell had drawn the daymark with a SCAMIN and
        // another had not, so the same daymark answered twice.
        let out = dedupe_picks(vec![
            obj("DAYMAR", "plan", 12000, &[("COLOUR", "green")]),
            obj("DAYMAR", "approach", 22000, &[("COLOUR", "green"), ("SCAMIN", "89999")]),
        ]);
        assert_eq!(out.len(), 1, "SCAMIN describes the cell, not the daymark");
        assert_eq!(out[0].duplicates, 1);
    }

    #[test]
    fn objects_that_really_differ_are_kept_apart() {
        // A light and its sector partner differ in one attribute and are two
        // different lights. Folding them would lose a sector.
        let out = dedupe_picks(vec![
            obj("LIGHTS", "plan", 12000, &[("SECTR1", "165")]),
            obj("LIGHTS", "plan", 12000, &[("SECTR1", "305")]),
        ]);
        assert_eq!(out.len(), 2);

        // Same attributes, different class.
        let out = dedupe_picks(vec![
            obj("LIGHTS", "plan", 12000, &[("COLOUR", "green")]),
            obj("DAYMAR", "plan", 12000, &[("COLOUR", "green")]),
        ]);
        assert_eq!(out.len(), 2);

        // Same class and attributes, different geometry: a point islet and an
        // area of land are not the same object.
        let mut area = obj("LNDARE", "plan", 12000, &[]);
        area.geometry = FeatureType::Area;
        let out = dedupe_picks(vec![obj("LNDARE", "plan", 12000, &[]), area]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn the_nearest_copy_sets_the_distance() {
        let light = &[("COLOUR", "green")];
        let mut far = obj("LIGHTS", "approach", 22000, light);
        far.distance_m = 40.0;
        let mut near = obj("LIGHTS", "plan", 12000, light);
        near.distance_m = 4.0;
        let out = dedupe_picks(vec![far, near]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].distance_m, 4.0);
    }

    fn light(attrs: &[(&str, AttributeValue)]) -> Feature {
        let mut attributes = crate::senc::Attributes::new();
        for (k, v) in attrs {
            attributes.insert(k, v.clone());
        }
        Feature {
            type_code: 0,
            object_class: crate::senc::ObjectClass::Light,
            feature_type: FeatureType::Point,
            attributes,
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        }
    }

    #[test]
    fn a_sectored_light_reads_as_a_light_list_entry() {
        // Kalvebod S, from the real chart set. LITDSN01 alone gives
        // "Fl 3s 4m": correct for a chart *label*, because a sectored light's
        // colour changes with bearing and the arcs carry it. In a written
        // answer that hides that this is the green sector of a light which is
        // also red elsewhere, so the colour and range are said outright.
        let decoded = [
            ("COLOUR".to_string(), "green".to_string()),
            ("VALNMR".to_string(), "4".to_string()),
        ];
        let f = light(&[
            ("LITCHR", AttributeValue::Integer(2)), // flashing
            ("SIGGRP", AttributeValue::String("(1)".into())),
            ("COLOUR", AttributeValue::String("4".into())), // green
            ("SIGPER", AttributeValue::Float(3.0)),
            ("HEIGHT", AttributeValue::Float(3.5)),
            ("VALNMR", AttributeValue::Float(4.0)),
            ("SECTR1", AttributeValue::Float(165.0)),
            ("SECTR2", AttributeValue::Float(305.0)),
        ]);
        let summary = summarise("LIGHTS", &f, &decoded).expect("a reading");
        assert!(summary.starts_with("Fl"), "{summary}");
        assert!(summary.contains("green"), "the sector's colour: {summary}");
        assert!(summary.contains("165°–305°"), "the sector: {summary}");
        assert!(summary.contains("4 M"), "the range: {summary}");
    }

    /// A tap on Kalvebodbroen lands 9 m from one of its lights and inside
    /// the bridge's area. The bridge is the answer: its clearance decides
    /// whether the boat goes through at all.
    #[test]
    fn a_bridge_comes_before_the_lights_it_carries() {
        let mut bridge = obj("BRIDGE", "plan", 12000, &[("VERCLR", "16")]);
        bridge.geometry = FeatureType::Area;
        bridge.distance_m = 0.0;
        let mut picks = vec![
            obj("CTNARE", "plan", 12000, &[]),
            obj("LIGHTS", "plan", 12000, &[("COLOUR", "red")]),
            bridge,
        ];
        sort_picks(&mut picks);
        let order: Vec<&str> = picks.iter().map(|p| p.acronym.as_str()).collect();
        assert_eq!(order, ["BRIDGE", "LIGHTS", "CTNARE"]);
        assert!(is_overhead("CBLOHD") && !is_overhead("CBLSUB"));
    }

    /// What is tapped for comes before what merely lies there: a light
    /// before a wreck, a wreck before the caution area round it, and all of
    /// them before the depth contour and the depth area — whatever their
    /// geometry — with the cell's description last.
    #[test]
    fn a_tap_is_answered_with_what_a_sailor_means_first() {
        let order = ["LIGHTS", "BOYLAT", "BCNCAR", "WRECKS", "UWTROC", "CTNARE", "RESARE", "CBLSUB", "DEPCNT", "DEPARE", "SOUNDG", "M_QUAL"];
        let ranks: Vec<u8> = order.iter().map(|a| interest(a)).collect();
        assert!(ranks.windows(2).all(|w| w[0] <= w[1]), "{ranks:?}");
        assert!(interest("BOYLAT") < interest("WRECKS"));
        assert!(interest("OBSTRN") < interest("CTNARE"));
        assert!(interest("CTNARE") < interest("DEPCNT"), "an area with a rule before a contour line");
        assert!(interest("DEPARE") < interest("M_NPUB"));
        // Seen on the NOAA Boston cell: a tap on open water was answered
        // "Magnetic variation", ahead of the sea area's name.
        assert!(interest("SEAARE") < interest("MAGVAR"));
    }

    #[test]
    fn only_the_classes_the_standard_composes_for_get_a_summary() {
        // Inventing a phrasing for other classes would make the bubble and the
        // chart disagree, which is the one thing this must not do.
        let f = light(&[("CATROD", AttributeValue::Integer(1))]);
        assert_eq!(summarise("ROADWY", &f, &[]), None);
        assert_eq!(summarise("DAYMAR", &f, &[]), None);
    }

    #[test]
    fn scamin_is_hidden_but_provenance_is_not() {
        assert!(!is_worth_showing("SCAMIN"));
        assert!(is_worth_showing("COLOUR"));
        // Source fields do not decide identity — two cells date the same
        // survey differently — but they stay visible.
        assert!(is_cell_bookkeeping("SORDAT"));
        assert!(is_worth_showing("SORDAT"));
        assert!(!is_cell_bookkeeping("COLOUR"));
    }

    fn feature(acronym: &str, feature_type: FeatureType) -> Feature {
        Feature {
            type_code: crate::senc::s57_acronym_to_code(acronym).unwrap(),
            object_class: crate::senc::ObjectClass::Other,
            feature_type,
            attributes: crate::senc::Attributes::new(),
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        }
    }

    /// A small cell referenced at 56°N 11°E, all positions in metres from
    /// there: a buoy on the reference point, soundings 30 m and 500 m east,
    /// a depth contour running north–south 45 m west, and a depth area
    /// 400 m square around it all.
    fn harbour() -> (ChartData, ChartInfo, [f64; 2]) {
        use crate::senc::{
            AreaGeometry, BBox, CellExtent, EdgeRef, EdgeTable, LineGeometry, TriPrim, TriPrimType,
        };
        let (ref_lat, ref_lon) = (56.0, 11.0);
        let (rx, ry) = crate::tiles::latlon_to_mercator(ref_lat, ref_lon);
        let bbox = BBox { min_x: 10.99, max_x: 11.01, min_y: 55.99, max_y: 56.01 };

        // The point and multipoint geometry types cannot be named outside
        // `senc`, so they are borrowed from a real cell and moved here.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/s57/US4MA1BD.000");
        let cell = crate::s57::cell::Cell::open(std::path::Path::new(path)).unwrap();
        let fixture = ChartData::parse(crate::s57::senc_encode::encode(&cell)).unwrap();
        let like = |t: FeatureType| fixture.features.iter().find(|f| f.feature_type == t).cloned();

        let mut buoy = like(FeatureType::Point).expect("a point feature in the fixture");
        buoy.type_code = crate::senc::s57_acronym_to_code("BOYLAT").unwrap();
        buoy.attributes = crate::senc::Attributes::new();
        let pg = buoy.point_geometry.as_mut().unwrap();
        (pg.x, pg.y) = (ref_lat, ref_lon);
        let mut soundings = like(FeatureType::Multipoint).expect("soundings in the fixture");
        soundings.type_code = crate::senc::s57_acronym_to_code("SOUNDG").unwrap();
        soundings.multipoint_geometry.as_mut().unwrap().points = vec![[500.0, 0.0, 3.0], [30.0, 0.0, 4.0]];
        let mut edge_table = EdgeTable::new();
        edge_table.edges.insert(1, vec![[-45.0, -500.0], [-45.0, 500.0]]);
        let mut contour = feature("DEPCNT", FeatureType::Line);
        contour.line_geometry = Some(LineGeometry {
            extent: bbox,
            edge_refs: vec![EdgeRef { start_node: 1, edge_index: 1, end_node: 2, reversed: Some(false) }],
        });
        let mut depth = feature("DEPARE", FeatureType::Area);
        let (lo, hi) = (-200.0, 200.0);
        depth.area_geometry = Some(AreaGeometry {
            extent: bbox,
            triangles: vec![TriPrim {
                prim_type: TriPrimType::Triangles,
                vertices: vec![[lo, lo], [hi, lo], [hi, hi], [lo, lo], [hi, hi], [lo, hi]],
            }],
            edge_refs: Vec::new(),
        });

        let chart = ChartData {
            header: crate::senc::SencHeader::default(),
            // Areas first, as a cell stores them: the ranking must not
            // depend on the order the features arrive in.
            features: vec![depth, contour, soundings, buoy],
            edge_table,
        };
        let info = ChartInfo {
            id: 1,
            path: "/charts/TEST.oesu".into(),
            name: "TEST".into(),
            native_scale: 12_000,
            extent_wgs84: CellExtent { min_lat: 55.99, max_lat: 56.01, min_lon: 10.99, max_lon: 11.01 },
            extent_mercator: crate::tiles::TileBounds::new(rx - 1e3, rx + 1e3, ry - 1e3, ry + 1e3),
            ref_lat,
            ref_lon,
        };
        (chart, info, [rx, ry])
    }

    fn tap(at: [f64; 2], dx: f64, dy: f64, tol: f64) -> Vec<(String, f64)> {
        let (chart, info, _) = harbour();
        pick_at(&chart, &info, [at[0] + dx, at[1] + dy], tol)
            .into_iter()
            .map(|p| (p.acronym, p.distance_m))
            .collect()
    }

    /// The tolerance is the reach: a point just inside it is found, just
    /// outside it is not. A buoy on a depth area is listed first even though
    /// the cursor is *inside* the area (distance 0) and only near the buoy.
    #[test]
    fn a_tap_finds_points_within_the_tolerance_and_lists_them_first() {
        let (_, _, at) = harbour();
        let names = |v: &[(String, f64)]| v.iter().map(|(a, _)| a.clone()).collect::<Vec<_>>();
        let hits = tap(at, 0.0, 9.99, 10.0);
        assert_eq!(names(&hits), ["BOYLAT", "DEPARE"]);
        assert!((hits[0].1 - 9.99).abs() < 1e-6, "{hits:?}");
        assert_eq!(hits[1].1, 0.0);
        assert_eq!(names(&tap(at, 0.0, 10.01, 10.0)), ["DEPARE"]);
        // A sounding is found by its own position, not the multipoint's
        // first one.
        let hits = tap(at, 28.0, 0.0, 5.0);
        assert_eq!(names(&hits), ["SOUNDG", "DEPARE"]);
        assert!((hits[0].1 - 2.0).abs() < 1e-3, "{hits:?}");
        // Outside the area and away from everything: nothing.
        assert!(tap(at, 300.0, 0.0, 10.0).is_empty());
    }

    /// A line is grabbed from four times as far as a point, because a
    /// hairline is hard to hit with a finger — but not from further.
    #[test]
    fn a_line_is_grabbed_from_further_than_a_point() {
        let (_, _, at) = harbour();
        // 20 m from the contour, with a 10 m tolerance: within 4 × 10.
        let hits = tap(at, -25.0, 0.0, 10.0);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].0, "DEPCNT");
        assert!((hits[0].1 - 20.0).abs() < 1e-3, "{hits:?}");
        assert_eq!(hits[1].0, "DEPARE");
        // With a 4 m tolerance the grab is 16 m, short of it.
        let hits = tap(at, -25.0, 0.0, 4.0);
        assert_eq!(hits.iter().map(|h| h.0.as_str()).collect::<Vec<_>>(), ["DEPARE"]);
    }

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
