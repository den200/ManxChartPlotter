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
//! `chart_context::pt2GetAssociatedObjects`. navcore precomputes the same
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
/// this danger sitting in?" — navcore's stand-in for OpenCPN's
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
}
