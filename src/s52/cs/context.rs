//! Neighbourhood facts a conditional-symbology procedure needs beyond its own
//! feature.
//!
//! Most CS procedures are pure functions of one feature's attributes. UDWHAZ03
//! is not: to decide whether an obstruction, wreck or rock is an *isolated
//! danger* it has to look at the depth areas around it. A shallow rock sitting
//! inside shallow water is just part of that shallow water; the same rock
//! sitting in water deeper than the safety contour is a hazard that must be
//! promoted to DISPLAYBASE and flagged with ISODGR51.
//!
//! s52cnsy.cpp gets those neighbours through
//! `chart_context::pt2GetAssociatedObjects`. Manx precomputes the same
//! information where the chart's features are still in scope (the tile builder)
//! and passes it down as this struct.

/// Depth values of the DEPARE/DRGARE features that intersect a danger object.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CsContext {
    /// DRVAL1 of each intersecting depth *area*.
    pub area_drval1: Vec<f64>,
    /// DRVAL2 of each intersecting depth *line*.
    pub line_drval2: Vec<f64>,
    /// The chart's selected safety contour: the shallowest contour the chart
    /// actually has at or deeper than the mariner's setting (s52cnsy.cpp reads
    /// it from the chart context, not from the setting). `None` falls back to
    /// the setting itself.
    pub safety_contour: Option<f64>,
}

impl CsContext {
    /// No neighbourhood information available. UDWHAZ03 then behaves as
    /// OpenCPN does with a null chart context: it cannot confirm danger, so it
    /// does not raise one.
    pub const EMPTY: &'static CsContext = &CsContext {
        area_drval1: Vec::new(),
        line_drval2: Vec::new(),
        safety_contour: None,
    };

    pub fn is_empty(&self) -> bool {
        self.area_drval1.is_empty() && self.line_drval2.is_empty() && self.safety_contour.is_none()
    }

    /// The UDWHAZ03 test over the associated objects (s52cnsy.cpp `_UDWHAZ03`):
    /// a depth line shallower than the safety contour, or a depth area whose
    /// shallow limit is at or beyond it, both mean the object lies in water
    /// the mariner would otherwise treat as safe.
    pub fn indicates_danger(&self, safety_contour: f64, expsou: i32) -> bool {
        if self.line_drval2.iter().any(|d| *d < safety_contour) {
            return true;
        }
        expsou != 1 && self.area_drval1.iter().any(|d| *d >= safety_contour)
    }

    /// Stable hash for the resolve/CS caches — two features with the same
    /// attributes but different surroundings must not share a cache entry.
    pub fn hash_value(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for v in &self.area_drval1 {
            v.to_bits().hash(&mut h);
        }
        0xffu8.hash(&mut h);
        for v in &self.line_drval2 {
            v.to_bits().hash(&mut h);
        }
        self.safety_contour.map(f64::to_bits).hash(&mut h);
        h.finish()
    }
}

/// One depth area, tessellated, in global Mercator metres.
struct DepthArea {
    min: [f64; 2],
    max: [f64; 2],
    /// Flattened triangle list: every 3 points is one triangle.
    tris: Vec<[f64; 2]>,
    drval1: f64,
}

/// Per-chart index of DEPARE/DRGARE areas, used to answer "what depth area is
/// this danger sitting in?" — Manx's stand-in for OpenCPN's
/// `s57chart::GetAssociatedObjects`.
///
/// Depth *lines* are not indexed: the S-57 model allows DEPARE as a line
/// primitive and UDWHAZ03 tests those with DRVAL2, but the o-charts SENC
/// encodes depth areas exclusively as areas ([`CsContext::line_drval2`] is the
/// extension point if that ever changes).
pub struct DepthAreaIndex {
    areas: Vec<DepthArea>,
}

impl DepthAreaIndex {
    /// Build from one chart's features. `ref_lat`/`ref_lon` are the chart's
    /// SENC reference point — area triangles are stored relative to it.
    pub fn build(features: &[crate::senc::Feature], ref_lat: f64, ref_lon: f64) -> Self {
        Self::build_near(features, ref_lat, ref_lon, None)
    }

    /// As [`build`](Self::build), keeping only the areas that reach into
    /// `near` (global Mercator `[min, max]`). The tile builder asks about one
    /// tile's dangers at a time; copying out every depth area in the chart
    /// for each tile would cost far more than the question.
    pub fn build_near(
        features: &[crate::senc::Feature],
        ref_lat: f64,
        ref_lon: f64,
        near: Option<([f64; 2], [f64; 2])>,
    ) -> Self {
        use crate::senc::ObjectClass;

        let (ref_mx, ref_my) = crate::tiles::latlon_to_mercator(ref_lat, ref_lon);
        let mut areas = Vec::new();

        for feature in features {
            if !matches!(
                feature.object_class,
                ObjectClass::DepthArea | ObjectClass::DredgedArea
            ) {
                continue;
            }
            let Some(geom) = &feature.area_geometry else {
                continue;
            };
            // A depth area with no DRVAL1 cannot make anything safe, and
            // GetDoubleAttr would leave OpenCPN's local at 0.0 — same effect.
            let drval1 = feature.drval1().unwrap_or(0.0);

            if let Some((lo, hi)) = near {
                // A pass over the vertices with no allocation, to skip areas
                // that cannot contain anything asked about.
                let mut min = [f64::MAX; 2];
                let mut max = [f64::MIN; 2];
                for p in geom.triangles.iter().flat_map(|t| t.vertices.iter()) {
                    let (x, y) = (p[0] as f64 + ref_mx, p[1] as f64 + ref_my);
                    min = [min[0].min(x), min[1].min(y)];
                    max = [max[0].max(x), max[1].max(y)];
                }
                if max[0] < lo[0] || min[0] > hi[0] || max[1] < lo[1] || min[1] > hi[1] {
                    continue;
                }
            }

            let mut tris: Vec<[f64; 2]> = Vec::new();
            for prim in &geom.triangles {
                let v: Vec<[f64; 2]> = prim
                    .vertices
                    .iter()
                    .map(|p| [p[0] as f64 + ref_mx, p[1] as f64 + ref_my])
                    .collect();
                match prim.prim_type {
                    crate::senc::TriPrimType::Triangles => {
                        for c in v.chunks_exact(3) {
                            tris.extend_from_slice(c);
                        }
                    }
                    crate::senc::TriPrimType::TriangleStrip => {
                        for i in 2..v.len() {
                            tris.extend_from_slice(&[v[i - 2], v[i - 1], v[i]]);
                        }
                    }
                    crate::senc::TriPrimType::TriangleFan => {
                        for i in 2..v.len() {
                            tris.extend_from_slice(&[v[0], v[i - 1], v[i]]);
                        }
                    }
                }
            }
            if tris.is_empty() {
                continue;
            }

            let mut min = [f64::MAX; 2];
            let mut max = [f64::MIN; 2];
            for p in &tris {
                min[0] = min[0].min(p[0]);
                min[1] = min[1].min(p[1]);
                max[0] = max[0].max(p[0]);
                max[1] = max[1].max(p[1]);
            }
            areas.push(DepthArea {
                min,
                max,
                tris,
                drval1,
            });
        }

        Self { areas }
    }

    pub fn is_empty(&self) -> bool {
        self.areas.is_empty()
    }

    /// The depth areas containing `point` (global Mercator metres).
    pub fn context_at(&self, point: [f64; 2]) -> CsContext {
        let mut area_drval1 = Vec::new();
        for area in &self.areas {
            if point[0] < area.min[0]
                || point[0] > area.max[0]
                || point[1] < area.min[1]
                || point[1] > area.max[1]
            {
                continue;
            }
            if area
                .tris
                .chunks_exact(3)
                .any(|t| point_in_triangle(point, t[0], t[1], t[2]))
            {
                area_drval1.push(area.drval1);
            }
        }
        CsContext {
            area_drval1,
            line_drval2: Vec::new(),
            ..Default::default()
        }
    }

    /// The depth areas around a feature, located by its own geometry.
    pub fn context_for(&self, feature: &crate::senc::Feature) -> CsContext {
        let Some(point) = feature_position(feature) else {
            return CsContext::default();
        };
        self.context_at(point)
    }
}

/// Where to probe for a feature's surroundings: its own point, or the centre of
/// its extent for line and area dangers.
fn feature_position(feature: &crate::senc::Feature) -> Option<[f64; 2]> {
    // OSENC point geometry stores x = latitude, y = longitude.
    if let Some(pg) = &feature.point_geometry {
        let (mx, my) = crate::tiles::latlon_to_mercator(pg.x, pg.y);
        return Some([mx, my]);
    }
    if let Some(ag) = &feature.area_geometry {
        let (mx, my) = crate::tiles::latlon_to_mercator(
            (ag.extent.min_y + ag.extent.max_y) / 2.0,
            (ag.extent.min_x + ag.extent.max_x) / 2.0,
        );
        return Some([mx, my]);
    }
    None
}

fn point_in_triangle(p: [f64; 2], a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> bool {
    let sign = |p1: [f64; 2], p2: [f64; 2], p3: [f64; 2]| {
        (p1[0] - p3[0]) * (p2[1] - p3[1]) - (p2[0] - p3[0]) * (p1[1] - p3[1])
    };
    let d1 = sign(p, a, b);
    let d2 = sign(p, b, c);
    let d3 = sign(p, c, a);
    let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(has_neg && has_pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_in_triangle_basic() {
        let a = [0.0, 0.0];
        let b = [10.0, 0.0];
        let c = [0.0, 10.0];
        assert!(point_in_triangle([1.0, 1.0], a, b, c));
        assert!(!point_in_triangle([9.0, 9.0], a, b, c));
    }

    #[test]
    fn empty_context_never_raises_danger() {
        assert!(!CsContext::EMPTY.indicates_danger(10.0, 0));
    }

    #[test]
    fn area_deeper_than_safety_contour_is_danger() {
        let ctx = CsContext {
            area_drval1: vec![10.0],
            line_drval2: vec![],
            ..Default::default()
        };
        assert!(ctx.indicates_danger(10.0, 0));
    }

    #[test]
    fn shallow_surroundings_are_not_a_danger() {
        // A rock inside a 0-2 m area is not an isolated danger.
        let ctx = CsContext {
            area_drval1: vec![0.0],
            line_drval2: vec![],
            ..Default::default()
        };
        assert!(!ctx.indicates_danger(10.0, 0));
    }

    #[test]
    fn expsou_one_suppresses_the_area_test() {
        let ctx = CsContext {
            area_drval1: vec![20.0],
            line_drval2: vec![],
            ..Default::default()
        };
        assert!(!ctx.indicates_danger(10.0, 1));
    }

    #[test]
    fn shallow_depth_line_is_a_danger_even_with_expsou() {
        let ctx = CsContext {
            area_drval1: vec![],
            line_drval2: vec![5.0],
            ..Default::default()
        };
        assert!(ctx.indicates_danger(10.0, 1));
    }

    /// The two halves of `_UDWHAZ03` have opposite boundaries: an area is
    /// "safe water" from `drval1 >= safety_contour`, a line is a danger only
    /// when `drval2 < safety_contour`.
    #[test]
    fn udwhaz03_boundaries_at_the_safety_contour() {
        let area = |d: f64| CsContext {
            area_drval1: vec![d],
            ..Default::default()
        };
        assert!(area(10.0).indicates_danger(10.0, 0));
        assert!(!area(9.9).indicates_danger(10.0, 0));

        let line = |d: f64| CsContext {
            line_drval2: vec![d],
            ..Default::default()
        };
        assert!(line(9.9).indicates_danger(10.0, 0));
        assert!(!line(10.0).indicates_danger(10.0, 0));
    }

    #[test]
    fn any_safe_area_among_several_raises_the_danger() {
        // An object straddling a 0-5 m and a 10-20 m area lies partly in safe
        // water, as s52cnsy.cpp's loop over associated objects concludes.
        let ctx = CsContext {
            area_drval1: vec![0.0, 10.0],
            ..Default::default()
        };
        assert!(ctx.indicates_danger(10.0, 0));
    }

    /// The resolve and CS caches key on this hash, so surroundings that
    /// symbolise differently must hash differently — including the same
    /// value moved between the area and line lists.
    #[test]
    fn different_surroundings_hash_differently() {
        let a = CsContext {
            area_drval1: vec![10.0],
            ..Default::default()
        };
        let b = CsContext {
            area_drval1: vec![0.0],
            ..Default::default()
        };
        let c = CsContext {
            line_drval2: vec![10.0],
            ..Default::default()
        };
        let d = CsContext {
            area_drval1: vec![10.0],
            safety_contour: Some(10.0),
            ..Default::default()
        };
        let hashes = [a.hash_value(), b.hash_value(), c.hash_value(), d.hash_value()];
        for i in 0..hashes.len() {
            for j in i + 1..hashes.len() {
                assert_ne!(hashes[i], hashes[j], "contexts {i} and {j} collide");
            }
        }
        assert_eq!(a.hash_value(), a.clone().hash_value());
        assert!(CsContext::EMPTY.is_empty());
        assert!(!a.is_empty());
    }

    const REF_LAT: f64 = 55.0;
    const REF_LON: f64 = 10.0;

    /// A depth area covering +-1 km around the chart reference point, as
    /// a two-triangle fan in reference-relative Mercator metres.
    fn square_area(class: crate::senc::ObjectClass, drval1: Option<f64>, prim: crate::senc::TriPrimType) -> crate::senc::Feature {
        use crate::senc::{AttributeValue, Attributes, AreaGeometry, BBox, Feature, FeatureType, TriPrim, TriPrimType};
        let vertices = match prim {
            // Fan and Triangles listed so both cover the same square.
            TriPrimType::TriangleFan => vec![[-1000.0, -1000.0], [1000.0, -1000.0], [1000.0, 1000.0], [-1000.0, 1000.0]],
            TriPrimType::TriangleStrip => vec![[-1000.0, -1000.0], [1000.0, -1000.0], [-1000.0, 1000.0], [1000.0, 1000.0]],
            TriPrimType::Triangles => vec![
                [-1000.0, -1000.0], [1000.0, -1000.0], [1000.0, 1000.0],
                [-1000.0, -1000.0], [1000.0, 1000.0], [-1000.0, 1000.0],
            ],
        };
        let mut attributes = Attributes::new();
        if let Some(d) = drval1 {
            attributes.insert("DRVAL1", AttributeValue::Float(d));
        }
        Feature {
            type_code: 0,
            object_class: class,
            feature_type: FeatureType::Area,
            attributes,
            area_geometry: Some(AreaGeometry {
                extent: BBox::default(),
                triangles: vec![TriPrim { prim_type: prim, vertices }],
                edge_refs: Vec::new(),
            }),
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        }
    }

    fn reference_point(dx: f64, dy: f64) -> [f64; 2] {
        let (x, y) = crate::tiles::latlon_to_mercator(REF_LAT, REF_LON);
        [x + dx, y + dy]
    }

    #[test]
    fn depth_area_index_reports_the_areas_around_a_point() {
        use crate::senc::{ObjectClass, TriPrimType};
        let features = vec![
            square_area(ObjectClass::DepthArea, Some(10.0), TriPrimType::TriangleFan),
            square_area(ObjectClass::DredgedArea, Some(15.0), TriPrimType::TriangleStrip),
            square_area(ObjectClass::DepthArea, None, TriPrimType::Triangles),
            // Land is not a depth area and must not count as water.
            square_area(ObjectClass::LandArea, Some(30.0), TriPrimType::TriangleFan),
        ];
        let index = DepthAreaIndex::build(&features, REF_LAT, REF_LON);

        let mut inside = index.context_at(reference_point(500.0, -500.0)).area_drval1;
        inside.sort_by(f64::total_cmp);
        // A depth area without DRVAL1 counts as 0 m, like OpenCPN's default.
        assert_eq!(inside, [0.0, 10.0, 15.0]);

        assert!(index.context_at(reference_point(1500.0, 0.0)).area_drval1.is_empty());
        assert!(index.context_at(reference_point(0.0, -1500.0)).area_drval1.is_empty());
    }

    #[test]
    fn build_near_skips_areas_outside_the_tile() {
        use crate::senc::{ObjectClass, TriPrimType};
        let features = vec![square_area(ObjectClass::DepthArea, Some(10.0), TriPrimType::TriangleFan)];
        let far = (reference_point(5000.0, 5000.0), reference_point(6000.0, 6000.0));
        assert!(DepthAreaIndex::build_near(&features, REF_LAT, REF_LON, Some(far)).is_empty());

        let near = (reference_point(900.0, 900.0), reference_point(2000.0, 2000.0));
        let index = DepthAreaIndex::build_near(&features, REF_LAT, REF_LON, Some(near));
        assert_eq!(index.context_at(reference_point(0.0, 0.0)).area_drval1, [10.0]);
    }

    #[test]
    fn context_for_probes_the_centre_of_an_area_feature() {
        use crate::senc::{ObjectClass, TriPrimType};
        let features = vec![square_area(ObjectClass::DepthArea, Some(10.0), TriPrimType::TriangleFan)];
        let index = DepthAreaIndex::build(&features, REF_LAT, REF_LON);

        // An area obstruction whose extent is centred on the reference point.
        let mut obstruction = square_area(ObjectClass::Obstruction, None, TriPrimType::TriangleFan);
        if let Some(ag) = obstruction.area_geometry.as_mut() {
            ag.extent = crate::senc::BBox {
                min_x: REF_LON - 0.001,
                max_x: REF_LON + 0.001,
                min_y: REF_LAT - 0.001,
                max_y: REF_LAT + 0.001,
            };
        }
        assert_eq!(index.context_for(&obstruction).area_drval1, [10.0]);

        // A feature with no geometry has no surroundings.
        let mut bare = obstruction.clone();
        bare.area_geometry = None;
        assert!(index.context_for(&bare).is_empty());
    }
}
