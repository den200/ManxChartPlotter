//! Mariner settings for S-52 display control.

use super::DisplayCategory;

/// Depth unit display modes for soundings
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum DepthUnit {
    #[default]
    Meters = 0,
    Feet = 1,
    Fathoms = 2,
}

/// Depth shade mode per S52_MAR_TWO_SHADES
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum DepthShadeMode {
    /// Two shades: safe (DEPMD) and unsafe (DEPVS)
    TwoShades,
    /// Four shades: DEPVS, DEPMS, DEPMD, DEPDW (default per spec)
    #[default]
    FourShades,
}

/// User-configurable display settings.
/// Controls which features are visible based on S-52 display categories.
#[derive(Debug, Clone, PartialEq)]
pub struct MarinerSettings {
    /// Show Displaybase features (always true for safety - ECDIS requirement)
    pub show_displaybase: bool,
    /// Show Standard features (default: true)
    pub show_standard: bool,
    /// Show Other features.
    ///
    /// Default **true**. OpenCPN's compiled default is STANDARD, but every
    /// reference capture we target is taken with "ENC display: All", and the
    /// difference is not cosmetic: LAKARE (the coastal lagoons behind
    /// Vallensbæk beach), BUISGL, PRDARE and the harbour detail are all
    /// category OTHER. Measured coastline agreement against the four reference
    /// views, STANDARD → ALL: 0.320→0.538, 0.360→0.539, 0.478→0.532,
    /// 0.212→0.432. Use `NAVCORE_DISPLAY_CAT=standard` for the ECDIS default.
    pub show_other: bool,
    /// Safety depth in meters (features shallower are highlighted)
    pub safety_depth: f32,
    /// Safety contour depth in meters (emphasized on chart)
    pub safety_contour: f32,
    /// Shallow contour depth in meters (OpenCPN S52_MAR_SHALLOW_CONTOUR, default 2m)
    pub shallow_contour: f32,
    /// Deep contour depth in meters (OpenCPN S52_MAR_DEEP_CONTOUR, default 30m)
    pub deep_contour: f32,
    /// Show text labels
    pub show_text: bool,
    /// Show soundings
    pub show_soundings: bool,
    /// Depth unit for display (meters, feet, or fathoms)
    pub depth_unit: DepthUnit,
    /// Depth shade mode: 2 or 4 shades (S52_MAR_TWO_SHADES)
    pub depth_shade_mode: DepthShadeMode,
    /// Use symbolized area boundaries (LC patterns) vs plain (LS lines).
    /// Corresponds to S52_MAR_SYMBOLIZED_BND. Default: false — matches OpenCPN's
    /// `m_nBoundaryStyle = PLAIN_BOUNDARIES` (s52plib.cpp:308). With symbolized
    /// boundaries, area classes like CTNARE resolve to a stamped LC() boundary
    /// (e.g. magenta CTNARE51 caution symbols along the whole coast); PLAIN
    /// resolves them to a single dashed LS() line, as OpenCPN shows by default.
    pub symbolized_boundaries: bool,
    /// Suppress non-critical text labels (OpenCPN m_bShowS57ImportantTextOnly).
    /// When true, text with display-group (dis) >= 20 is filtered out.
    pub show_important_text_only: bool,
    /// Draw the chart coverage (M_COVR) outline rectangle. OpenCPN does not draw
    /// this in STANDARD display; gate it behind a separate setting rather than
    /// bypassing the category filter.
    pub show_chart_boundaries: bool,
    /// Apply SUPER_SCAMIN: synthesise a SCAMIN from the cell's compilation
    /// scale for features that declare none. OpenCPN reads this from config
    /// with a default of **0** (navutil.cpp: `Read("bUseSUPER_SCAMIN", &v, 0)`),
    /// so it is off unless the user asks for it. navcore applied it
    /// unconditionally, which hid pier structures, mooring facilities and the
    /// dredged basins inside harbours.
    pub use_super_scamin: bool,
    /// Draw S-57 meta objects (M_NSYS, M_COVR, M_NPUB...). OpenCPN filters every
    /// `M_*` class outside the OTHER display category unless "show meta objects"
    /// is on, and its config default is off (navutil.cpp `Read("bShowMeta",
    /// &v, 0)`). navcore drew them, which put survey-system boundaries over the
    /// chart at every zoom.
    pub show_meta_objects: bool,
    /// Show M_QUAL (zones of confidence / quality of survey) regardless of its
    /// OTHER display category — OpenCPN's separate "Quality of data" switch
    /// (`s52plib::SetQualityOfData`, which forces M_QUAL visible rather than
    /// moving it between categories).
    pub show_quality_of_data: bool,
    /// Prefer the Simplified point-symbol table over Paper Chart.
    /// Corresponds to OpenCPN's "simplified symbols" toggle.
    pub simplified_points: bool,
    /// Shade a band inside each depth area's boundary, so nested depth areas
    /// read as terraced steps rather than flat bands.
    ///
    /// **Not S-52.** The presentation library specifies exact fill colours for
    /// depth areas, and this darkens them near their edges, so it is a display
    /// option rather than part of the portrayal — default off, and the
    /// conformance harness and reference captures run without it. It invents no
    /// depth data: the shading follows the real polygon boundaries, which are
    /// the surveyed contours, and says nothing about the seabed between them.
    pub depth_relief: bool,
}

impl Default for MarinerSettings {
    fn default() -> Self {
        // Defaults per S52-RENDERING-SPEC.md Appendix B
        Self {
            show_displaybase: true,  // Always on (safety)
            show_standard: true,     // Default on
            show_other: true,        // "ENC display: All" — see the field docs
            // s52plib's own defaults (`_MARparamVal` in s52utils.cpp), which is
            // what the reference captures run. They are not the IMO PS values
            // the spec appendix lists, and the difference is visible: with
            // safety contour 10 m a channel dredged to 3.7 m paints DEPMS,
            // where the reference paints DEPMD.
            safety_depth: 3.0,       // S52_MAR_SAFETY_DEPTH
            safety_contour: 3.0,     // S52_MAR_SAFETY_CONTOUR
            shallow_contour: 2.0,    // S52_MAR_SHALLOW_CONTOUR
            deep_contour: 6.0,       // S52_MAR_DEEP_CONTOUR
            show_text: true,
            show_soundings: true,
            depth_unit: DepthUnit::default(), // Meters
            depth_shade_mode: DepthShadeMode::default(), // FourShades per spec
            symbolized_boundaries: false, // PLAIN boundaries by default (matches OpenCPN)
            // Default ON: until navcore has proper LOD / per-feature SCAMIN for
            // labels, leaving every dis>=20 label visible at overview zooms
            // produces an unreadable text stampede. OpenCPN's default is off,
            // but its labels are also culled by a proper chart-scale filter we
            // don't yet match.
            show_important_text_only: true,
            show_chart_boundaries: false,
            // Simplified point symbols (the standard ECDIS look). OpenCPN's COMPILED
            // default is PAPER_CHART (s52plib.cpp:307), but the reference screenshots we
            // target run Simplified — Paper renders DAYMAR/topmarks as detailed paper
            // symbols (e.g. TOPSHP22 "red boarded square board") that show as prominent
            // red squares absent from the reference. Simplified maps these to DAYSQR01/
            // DAYTRI01 (small outline daymarks), matching OpenCPN's on-screen output.
            simplified_points: true,
            // Off, as in OpenCPN. The pale dotted survey-quality overlay is
            // supplementary information, not part of the standard display.
            show_quality_of_data: false,
            use_super_scamin: false,
            show_meta_objects: false,
            depth_relief: false,
        }
    }
}

impl MarinerSettings {
    /// Defaults with the `NAVCORE_*` mariner-setting overrides applied.
    ///
    /// One function for every entry point — the renderer, `--dump-scene` and
    /// `--dump-ir` — because a harness that resolves features under different
    /// settings than the renderer uses reports divergences that do not exist,
    /// and hides the ones that do.
    ///
    /// - `NAVCORE_DISPLAY_CAT=base|standard|all` — OpenCPN's "ENC display"
    /// - `NAVCORE_QUALITY_OF_DATA=1` — the M_QUAL survey-quality overlay
    /// - `NAVCORE_SHOW_META=1` — draw `M_*` meta objects
    /// - `NAVCORE_SUPER_SCAMIN=1` — synthesise SCAMIN from the cell scale
    /// - `NAVCORE_SAFETY_DEPTH=<m>` — soundings at or above this print bold
    /// - `NAVCORE_SAFETY_CONTOUR=<m>` — the safe/unsafe water boundary
    /// - `NAVCORE_SHALLOW_CONTOUR`, `NAVCORE_DEEP_CONTOUR` — the other two shades
    /// - `NAVCORE_DEPTH_SHADES=2|4` — two-shade display (safe/unsafe only)
    /// - `NAVCORE_DEPTH_UNIT=m|ft|fm`
    /// - `NAVCORE_DEPTH_RELIEF=1` — shade depth-area edges (not S-52)
    /// - `NAVCORE_SYMBOLS=simplified|paper` — OpenCPN's symbol-style switch
    pub fn from_env() -> Self {
        let mut s = Self::default();
        let on = |k: &str| std::env::var(k).is_ok_and(|v| v != "0");
        if let Ok(cat) = std::env::var("NAVCORE_DISPLAY_CAT") {
            match cat.to_ascii_lowercase().as_str() {
                "base" | "displaybase" => {
                    s.show_standard = false;
                    s.show_other = false;
                }
                "standard" => {
                    s.show_standard = true;
                    s.show_other = false;
                }
                "all" | "other" => {
                    s.show_standard = true;
                    s.show_other = true;
                }
                other => log::warn!("NAVCORE_DISPLAY_CAT: unknown value {:?}", other),
            }
        }
        if on("NAVCORE_QUALITY_OF_DATA") {
            s.show_quality_of_data = true;
        }
        if on("NAVCORE_SHOW_META") {
            s.show_meta_objects = true;
        }
        if on("NAVCORE_SUPER_SCAMIN") {
            s.use_super_scamin = true;
        }
        if on("NAVCORE_DEPTH_RELIEF") {
            s.depth_relief = true;
        }
        if let Ok(style) = std::env::var("NAVCORE_SYMBOLS") {
            match style.to_ascii_lowercase().as_str() {
                "simplified" | "simple" => s.simplified_points = true,
                "paper" | "paperchart" | "paper_chart" => s.simplified_points = false,
                other => log::warn!("NAVCORE_SYMBOLS: unknown value {:?}", other),
            }
        }

        // The mariner's depth settings. These are the navigationally important
        // controls — the safety depth is where you put your draft plus
        // clearance — and until now they were compiled-in constants with no way
        // to change them, which made the whole four-shade mechanism inert.
        let depth = |k: &str| {
            std::env::var(k).ok().and_then(|v| match v.parse::<f32>() {
                Ok(d) if d.is_finite() => Some(d),
                _ => {
                    log::warn!("{}: expected a depth in metres, got {:?}", k, v);
                    None
                }
            })
        };
        if let Some(d) = depth("NAVCORE_SAFETY_DEPTH") {
            s.safety_depth = d;
        }
        if let Some(d) = depth("NAVCORE_SAFETY_CONTOUR") {
            s.safety_contour = d;
        }
        if let Some(d) = depth("NAVCORE_SHALLOW_CONTOUR") {
            s.shallow_contour = d;
        }
        if let Some(d) = depth("NAVCORE_DEEP_CONTOUR") {
            s.deep_contour = d;
        }
        if let Ok(v) = std::env::var("NAVCORE_DEPTH_SHADES") {
            match v.as_str() {
                "2" | "two" => s.depth_shade_mode = DepthShadeMode::TwoShades,
                "4" | "four" => s.depth_shade_mode = DepthShadeMode::FourShades,
                other => log::warn!("NAVCORE_DEPTH_SHADES: expected 2 or 4, got {:?}", other),
            }
        }
        if let Ok(v) = std::env::var("NAVCORE_DEPTH_UNIT") {
            match v.to_ascii_lowercase().as_str() {
                "m" | "metres" | "meters" => s.depth_unit = DepthUnit::Meters,
                "ft" | "feet" => s.depth_unit = DepthUnit::Feet,
                "fm" | "fathoms" => s.depth_unit = DepthUnit::Fathoms,
                other => log::warn!("NAVCORE_DEPTH_UNIT: expected m/ft/fm, got {:?}", other),
            }
        }

        // The safety contour has to be at least the safety depth, or the chart
        // says water is safe that the soundings call dangerous. S-52 leaves
        // them independent; this is the one consistency rule worth enforcing.
        if s.safety_contour < s.safety_depth {
            log::warn!(
                "safety contour {}m is shallower than safety depth {}m; raising it",
                s.safety_contour,
                s.safety_depth
            );
            s.safety_contour = s.safety_depth;
        }
        s
    }

    /// Create settings with all categories visible
    pub fn show_all() -> Self {
        Self {
            show_displaybase: true,
            show_standard: true,
            show_other: true,
            ..Default::default()
        }
    }

    /// Check if a display category should be shown
    pub fn should_show(&self, category: DisplayCategory) -> bool {
        match category {
            DisplayCategory::Displaybase => self.show_displaybase,
            DisplayCategory::Standard => self.show_standard,
            DisplayCategory::Other => self.show_other,
            DisplayCategory::Mariners => self.show_standard, // Treat as standard
        }
    }
}
