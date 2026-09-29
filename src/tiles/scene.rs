//! Scene provenance — what Manx decided to draw, and where it came from.
//!
//! The S-52 oracle answers "which symbology instruction did this feature get".
//! This answers the next question down: "which polygon, from which chart, by
//! which code path, put ink on this pixel".
//!
//! There is no OpenCPN counterpart here and deliberately so. Its rasteriser is
//! not a linkable pure function the way `s52plib` is, and it does not need to
//! be: area geometry arrives from the SENC **already tessellated**, so Manx
//! is reproducing given triangles rather than computing them. That makes the
//! ground truth checkable by invariant instead of by comparison — an emitted
//! triangle that lies outside its own feature's polygon is wrong on its face,
//! with nothing to diff against.
//!
//! Enabled by `manx --dump-scene`; zero cost otherwise (the log is `None`).

use serde::Serialize;

/// How the geometry for one emitted area reached the vertex buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AreaSource {
    /// The triangles stored in the SENC, used as-is.
    SencTriangles,
    /// The triangles stored in the SENC, clipped to the tile.
    SencTrianglesClipped,
    /// Re-tessellated from the feature's rings with earcut, because the stored
    /// triangles emitted nothing for this tile. Correctness depends on ring
    /// reconstruction and hole ordering, so this path is worth watching.
    RingFallback,
}

/// One emitted area primitive, with enough provenance to trace a pixel back to
/// the feature and code path that produced it.
#[derive(Debug, Clone, Serialize)]
pub struct SceneArea {
    pub chart: String,
    /// Index of the feature within its chart, matching `manx --dump-ir` ids.
    pub feature: usize,
    pub class: String,
    pub source: AreaSource,
    pub priority: u8,
    pub color_index: u32,
    /// Chart's compilation scale denominator, so a coarse cell leaking over a
    /// finer one is visible in the report.
    pub chart_scale: u32,
    /// Whether this chart was treated as *background* for the tile: background
    /// geometry is stencil-masked away wherever a finer chart covers. A coarse
    /// chart drawing non-background geometry over a finer one is a quilt bug.
    pub is_background: bool,
    /// Triangles in global Mercator metres.
    pub tris: Vec<[[f64; 2]; 3]>,
    /// The feature's own extent in global Mercator metres, as the SENC declares
    /// it: `[min_x, min_y, max_x, max_y]`. Any emitted vertex outside this is a
    /// defect regardless of what any other renderer would do.
    pub extent: [f64; 4],
}

/// Where a stroked polyline came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LineSource {
    /// A line feature's own geometry (COALNE, DEPCNT, PIPSOL...).
    LineFeature,
    /// The LS() boundary of an area feature. Most unexplained ink on a chart is
    /// this: an area whose fill is invisible or absent still draws its edge.
    AreaBoundary,
}

/// One stroked feature, with the provenance to name the ink under a pixel.
#[derive(Debug, Clone, Serialize)]
pub struct SceneLine {
    pub chart: String,
    pub feature: usize,
    pub class: String,
    pub source: LineSource,
    pub priority: u8,
    pub is_background: bool,
    /// The S-52 style as written, e.g. `DASH,2,CHMGD` — enough to match ink
    /// seen in a capture to the instruction that produced it.
    pub style: String,
    /// Polylines in global Mercator metres.
    pub polylines: Vec<Vec<[f64; 2]>>,
}

/// An area feature the builder considered and did not draw, and why.
///
/// Half of "why is this pixel wrong" is "which feature is *missing*", and a
/// scene that only records what was emitted cannot answer that.
///
/// The extent is what makes a skip *queryable*: without it the log can say a
/// thousand LAKAREs were dropped but not that five of them were the lagoons
/// under the cursor. "What is missing here" is a different question from "what
/// is missing", and only the first one localises a bug.
#[derive(Debug, Clone, Serialize)]
pub struct SceneSkip {
    pub chart: String,
    pub feature: usize,
    pub class: String,
    pub reason: &'static str,
    /// The feature's extent in global Mercator metres, `[min_x, min_y, max_x,
    /// max_y]`. `None` for features that carry no area geometry at all.
    pub extent: Option<[f64; 4]>,
}

/// A chart's declared coverage for a tile, in global Mercator metres.
///
/// Chart *selection* uses the rectangular extent from the SENC header, but a
/// cell's real coverage is its M_COVR polygon, which can be much smaller. Where
/// the two differ, a chart can paint ground it does not actually cover.
#[derive(Debug, Clone, Serialize)]
pub struct SceneCoverage {
    pub chart: String,
    pub chart_scale: u32,
    /// The header extent used for selection: `[min_x, min_y, max_x, max_y]`.
    pub extent: [f64; 4],
    /// M_COVR rings, if the cell declares any.
    pub polygons: Vec<Vec<[f64; 2]>>,
}

/// Collector handed to the tile builder while `--dump-scene` is active.
#[derive(Debug, Default)]
pub struct SceneLog {
    pub areas: Vec<SceneArea>,
    pub lines: Vec<SceneLine>,
    pub skipped: Vec<SceneSkip>,
    pub coverage: Vec<SceneCoverage>,
    /// Post-reorder foreground area buffer: `(priority, vertex count, bbox)`.
    pub packet: Vec<(u8, u32, [f64; 4])>,
    /// Chart and feature index of the area currently being emitted, so the
    /// builder's inner loops do not each need to thread it.
    current: Option<(String, usize, String, [f64; 4])>,
}

impl SceneLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Announce which feature the following `record_area` calls belong to.
    pub fn begin_feature(
        &mut self,
        chart: &str,
        feature: usize,
        class: &str,
        extent: [f64; 4],
    ) {
        self.current = Some((chart.to_string(), feature, class.to_string(), extent));
    }

    pub fn end_feature(&mut self) {
        self.current = None;
    }

    pub fn record_packet(&mut self, bboxes: Vec<(u8, u32, [f64; 4])>) {
        self.packet = bboxes;
    }

    pub fn record_coverage(
        &mut self,
        chart: &str,
        chart_scale: u32,
        extent: [f64; 4],
        polygons: Vec<Vec<[f64; 2]>>,
    ) {
        self.coverage.push(SceneCoverage {
            chart: chart.to_string(),
            chart_scale,
            extent,
            polygons,
        });
    }

    pub fn record_skip(
        &mut self,
        chart: &str,
        feature: usize,
        class: &str,
        reason: &'static str,
        extent: Option<[f64; 4]>,
    ) {
        self.skipped.push(SceneSkip {
            chart: chart.to_string(),
            feature,
            class: class.to_string(),
            reason,
            extent,
        });
    }

    /// Record one stroked feature. Polylines are in global Mercator metres.
    #[allow(clippy::too_many_arguments)]
    pub fn record_line(
        &mut self,
        chart: &str,
        feature: usize,
        class: &str,
        source: LineSource,
        priority: u8,
        is_background: bool,
        style: String,
        polylines: Vec<Vec<[f64; 2]>>,
    ) {
        if polylines.iter().all(|p| p.len() < 2) {
            return;
        }
        self.lines.push(SceneLine {
            chart: chart.to_string(),
            feature: feature,
            class: class.to_string(),
            source,
            priority,
            is_background,
            style,
            polylines,
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_area(
        &mut self,
        source: AreaSource,
        priority: u8,
        color_index: u32,
        chart_scale: u32,
        is_background: bool,
        tris: Vec<[[f64; 2]; 3]>,
    ) {
        if tris.is_empty() {
            return;
        }
        let Some((chart, feature, class, extent)) = self.current.clone() else {
            return;
        };
        self.areas.push(SceneArea {
            chart,
            feature,
            class,
            source,
            priority,
            color_index,
            chart_scale,
            is_background,
            tris,
            extent,
        });
    }
}
