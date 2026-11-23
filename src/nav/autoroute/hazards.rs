//! What the chart forbids: the hard-obstacle predicate and the safety knobs.
//!
//! This is the spec's §6 rendered as one function per question. Everything is
//! deliberately conservative where the chart is silent: an obstruction with no
//! sounded depth is treated as dangerous, exactly as S-52's own UDWHAZ logic
//! treats it, because "the surveyor didn't say" is not the same as "deep
//! enough".

use crate::senc::{s57_code_to_acronym, Feature};

/// The boat, as the router needs to know it.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SafetyConfig {
    pub draft_m: f64,
    /// Under-keel clearance on top of the draft.
    pub ukc_m: f64,
    pub squat_m: f64,
    /// For bridges. `f64::INFINITY` would mean "never fits under anything";
    /// zero means a dinghy.
    pub air_draft_m: f64,
    /// Never closer than this to unsafe water, away from the ends of the
    /// passage. Small on purpose: a dredged channel can be a few tens of
    /// metres wide, and a floor any larger closes it. 0.005 nm (9 m): on
    /// the 10 m fine grid a cell beside a blocked one qualifies, its centre
    /// at least half a cell from the hazard. 0.01 nm shut the channel over
    /// Nibe Bredning, west of Aalborg, once the grid was that fine.
    pub offing_hard_nm: f64,
    /// The offing the route keeps wherever the water allows: closer costs
    /// steeply, so it is given up only where the water is narrower.
    pub offing_min_nm: f64,
    /// Below this distance the route pays a gently rising cost — prefer sea
    /// room when it is free.
    pub offing_soft_nm: f64,
    /// The coarse grid's cell size, over the whole passage (the fine grid,
    /// `fine_res_m`, is held only along the way this one picks). The spec
    /// says match the finest channel to transit.
    /// 20 m: at 60 m Svendborgsund was one or two cells wide and closed, and
    /// at 30 m the dredged channel east of Aalborg — about 100 m, running
    /// diagonally across the grid — came out a one-cell staircase joined
    /// only at its corners, open or shut depending on where the box put the
    /// cell boundaries. Long passages coarsen the grid themselves to fit its
    /// cell budget.
    pub grid_res_m: f64,
    /// The fine grid's cell size, in the corridor a route is finally found
    /// in (see `autoroute::corridor`). 10 m: a 50 m dredged channel keeps
    /// three cells after both its shores are stamped over the cells they
    /// touch.
    pub fine_res_m: f64,
    /// Unsurveyed areas: unsafe by default (decision §10.4).
    pub unsare_navigable: bool,
    /// The LEAST tide height above chart datum expected during the passage,
    /// metres. §6.1's `− tide_height(t)`, made static and conservative: the
    /// hazard grid is built once, so it is built for the worst water the
    /// passage can meet. Zero — chart datum, roughly LAT — is the safe
    /// default; a positive value credits water the tide guarantees.
    pub tide_height_min_m: f64,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            draft_m: 2.0,
            ukc_m: 0.5,
            squat_m: 0.0,
            air_draft_m: 20.0,
            offing_hard_nm: 0.005,
            offing_min_nm: 0.2,
            offing_soft_nm: 0.5,
            grid_res_m: 20.0,
            fine_res_m: 10.0,
            unsare_navigable: false,
            tide_height_min_m: 0.0,
        }
    }
}

impl SafetyConfig {
    /// The depth that divides water into safe and not: §6.1's
    /// `draft + squat + UKC − tide_height`, with tide entered as the least
    /// height the passage window guarantees.
    pub fn safety_contour_m(&self) -> f64 {
        self.draft_m + self.ukc_m + self.squat_m - self.tide_height_min_m
    }
}

/// How much a feature matters to a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Never enter.
    Hard,
    /// Enter if you must, at a cost — cautions, exercise areas, restricted
    /// areas short of prohibition.
    Soft,
}

/// WATLEV values that put something at or above the surface a hull meets:
/// partly submerged (1), always dry (2), covers and uncovers (4), awash (5).
/// Always-under-water (3) is dangerous only through its sounding, floating (7)
/// and subject-to-flooding (6) likewise.
fn watlev_dangerous(watlev: Option<i32>) -> bool {
    matches!(watlev, Some(1) | Some(2) | Some(4) | Some(5))
}

/// The RESTRN codes on a feature.
fn restrictions(feature: &Feature) -> Vec<i32> {
    let mut v: Vec<i32> = feature
        .attribute_str("RESTRN")
        .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_default();
    v.extend(feature.attribute_int("RESTRN"));
    v
}

/// Does the RESTRN list prohibit entry? Value 7 is "entry prohibited".
fn entry_prohibited(feature: &Feature) -> bool {
    restrictions(feature).contains(&7)
}

/// Does it restrict passage short of prohibiting it — "entry restricted"
/// (8) or "area to be avoided" (14)? Anchoring, fishing, discharge,
/// reporting and the rest regulate what a boat does there, not whether
/// she may sail through, and a route should not pay for them: NOAA wraps
/// whole bays in right-whale and no-discharge areas.
fn passage_restricted(feature: &Feature) -> bool {
    restrictions(feature).iter().any(|r| matches!(r, 8 | 14))
}

/// An opening bridge — opening, swing, lifting, bascule or draw (CATBRG
/// 2, 3, 4, 5, 7).
fn opening_bridge(feature: &Feature) -> bool {
    let opens = |v: i32| matches!(v, 2 | 3 | 4 | 5 | 7);
    feature.attribute_int("CATBRG").is_some_and(opens)
        || feature
            .attribute_str("CATBRG")
            .is_some_and(|s| s.split(',').filter_map(|v| v.trim().parse().ok()).any(opens))
}

/// Headroom kept over the masthead under anything overhead, metres.
const AIR_MARGIN_M: f64 = 0.5;

/// An opening bridge the boat cannot get under closed: a way through, but
/// only through its opening span. The grid carves these open after every
/// chart is stamped — see `build_hazard_grid` — because a bascule opening is
/// typically 30 m wide, a single grid cell, and the fixed spans and piers
/// either side of it are stamped over every cell they touch.
pub fn is_gate(feature: &Feature, safety: &SafetyConfig) -> bool {
    s57_code_to_acronym(feature.type_code) == "BRIDGE"
        && classify(feature, safety) == Some(Severity::Soft)
}

/// Classify one feature against the boat. `None` means the router does not
/// care about it at all.
///
/// The table is the spec's §6.2/§6.3, with the conservative readings written
/// down: a WRECKS or OBSTRN with neither a sounding nor a depth-clearing
/// WATLEV is dangerous; a sounded value clears it only when it clears the
/// safety contour.
pub fn classify(feature: &Feature, safety: &SafetyConfig) -> Option<Severity> {
    let contour = safety.safety_contour_m();
    let acronym = s57_code_to_acronym(feature.type_code);
    let valsou = feature.attribute_float("VALSOU");
    let shallow_or_unknown =
        |v: Option<f64>| v.map(|d| d < contour).unwrap_or(true);

    match acronym {
        "LNDARE" | "LNDRGN" => Some(Severity::Hard),
        "DEPARE" | "DRGARE" => {
            let drval1 = feature.drval1().unwrap_or(-1.0);
            (drval1 < contour).then_some(Severity::Hard)
        }
        "WRECKS" => {
            let catwrk = feature.attribute_int("CATWRK");
            let dangerous_class = matches!(catwrk, Some(2) | Some(4) | Some(5));
            let cleared = valsou.map(|d| d >= contour).unwrap_or(false)
                && !dangerous_class;
            (!cleared).then_some(Severity::Hard)
        }
        "OBSTRN" | "UWTROC" => {
            let dangerous =
                watlev_dangerous(feature.attribute_int("WATLEV")) || shallow_or_unknown(valsou);
            dangerous.then_some(Severity::Hard)
        }
        // Fixed structures a hull meets.
        "PONTON" | "DAMCON" | "SLCONS" | "FLODOC" | "HULKES" | "DYKCON" | "CAUSWY" | "GATCON" => {
            Some(Severity::Hard)
        }
        "UNSARE" => (!safety.unsare_navigable).then_some(Severity::Hard),
        "MARCUL" => Some(Severity::Hard),
        "RESARE" => {
            if entry_prohibited(feature) {
                Some(Severity::Hard)
            } else if passage_restricted(feature) {
                Some(Severity::Soft)
            } else {
                None
            }
        }
        // Military practice: firing, exercises — worth going round.
        "MIPARE" => Some(Severity::Soft),
        // A caution area is a note on the chart ("mandatory reporting",
        // "see the Coast Pilot"), shown to the skipper, not a cost.
        "CTNARE" => None,
        // Separation zones are for staying out of; the lanes themselves are
        // M4 cost territory, not obstacles.
        "TSEZNE" => Some(Severity::Hard),
        "BRIDGE" => {
            // Fixed clearance, or the clearance of an opening bridge when
            // closed — either lets the mast under without anyone's help.
            let clearance = feature
                .attribute_float("VERCLR")
                .or_else(|| feature.attribute_float("VERCCL"));
            let fits = clearance.is_some_and(|c| safety.air_draft_m + AIR_MARGIN_M <= c);
            // A lifting bridge only rises so far: its clearance when open
            // (VERCOP) is a limit like any other.
            let fits_open = feature
                .attribute_float("VERCOP")
                .is_none_or(|c| safety.air_draft_m + AIR_MARGIN_M <= c);
            if fits {
                None
            } else if opening_bridge(feature) && fits_open {
                // It opens: passable, at a cost, because it opens on its own
                // schedule. The Limfjord and a dozen Danish sounds are only
                // reachable through one.
                Some(Severity::Soft)
            } else {
                Some(Severity::Hard)
            }
        }
        // Overhead cables, pipes and conveyors: what goes for a fixed bridge
        // goes for these. A power line gives a *safe* clearance (VERCSA),
        // which allows for the arc and is the one to respect; without it,
        // the physical clearance. Neither stated: do not sail under it.
        "CBLOHD" | "PIPOHD" | "CONVYR" => {
            let clearance = feature
                .attribute_float("VERCSA")
                .or_else(|| feature.attribute_float("VERCLR"));
            let fits = clearance.is_some_and(|c| safety.air_draft_m + AIR_MARGIN_M <= c);
            (!fits).then_some(Severity::Hard)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::senc::{AttributeValue, Attributes, FeatureType, ObjectClass};

    fn feature(acronym: &str, attrs: &[(&str, AttributeValue)]) -> Feature {
        let type_code = crate::senc::s57_acronym_to_code(acronym)
            .unwrap_or_else(|| panic!("unknown acronym {acronym}"));
        let mut attributes = Attributes::new();
        for (k, v) in attrs {
            attributes.insert(k, v.clone());
        }
        Feature {
            type_code,
            object_class: ObjectClass::Other,
            feature_type: FeatureType::Area,
            attributes,
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        }
    }

    fn cfg() -> SafetyConfig {
        SafetyConfig::default() // contour 2.5 m
    }

    #[test]
    fn guaranteed_tide_buys_back_charted_shallows() {
        // 2.5 m boat, 2.0 m charted depth: blocked at datum…
        let shallow = feature("DEPARE", &[("DRVAL1", AttributeValue::Float(2.0))]);
        assert_eq!(classify(&shallow, &cfg()), Some(Severity::Hard));
        // …but with a guaranteed metre of tide the same area carries her.
        let mut with_tide = cfg();
        with_tide.tide_height_min_m = 1.0;
        assert_eq!(classify(&shallow, &with_tide), None);
        assert!((with_tide.safety_contour_m() - 1.5).abs() < 1e-9);
    }

    /// Every term of the contour counts: squat and under-keel clearance
    /// deepen it just as draft does.
    #[test]
    fn squat_and_clearance_deepen_the_safety_contour() {
        let area = feature("DEPARE", &[("DRVAL1", AttributeValue::Float(2.7))]);
        assert_eq!(classify(&area, &cfg()), None, "2.7 m clears a 2.5 m contour");
        let mut squatting = cfg();
        squatting.squat_m = 0.3;
        assert!((squatting.safety_contour_m() - 2.8).abs() < 1e-9);
        assert_eq!(classify(&area, &squatting), Some(Severity::Hard));
        let mut careful = cfg();
        careful.ukc_m = 1.0;
        assert_eq!(classify(&area, &careful), Some(Severity::Hard));
    }

    #[test]
    fn land_is_always_hard_and_deep_water_is_free() {
        assert_eq!(classify(&feature("LNDARE", &[]), &cfg()), Some(Severity::Hard));
        let deep = feature("DEPARE", &[("DRVAL1", AttributeValue::Float(10.0))]);
        assert_eq!(classify(&deep, &cfg()), None);
        let shallow = feature("DEPARE", &[("DRVAL1", AttributeValue::Float(2.0))]);
        assert_eq!(classify(&shallow, &cfg()), Some(Severity::Hard));
        // Exactly at the contour is safe: DRVAL1 is the shallow bound of the
        // area, so equal means "at least this deep".
        let at = feature("DEPARE", &[("DRVAL1", AttributeValue::Float(2.5))]);
        assert_eq!(classify(&at, &cfg()), None);
    }

    #[test]
    fn silence_is_dangerous_for_wrecks_and_obstructions() {
        // No sounding, no category: dangerous.
        assert_eq!(classify(&feature("WRECKS", &[]), &cfg()), Some(Severity::Hard));
        assert_eq!(classify(&feature("OBSTRN", &[]), &cfg()), Some(Severity::Hard));
        assert_eq!(classify(&feature("UWTROC", &[]), &cfg()), Some(Severity::Hard));

        // A sounded wreck deeper than the contour, not flagged dangerous:
        // clear.
        let cleared = feature(
            "WRECKS",
            &[("VALSOU", AttributeValue::Float(12.0)), ("CATWRK", AttributeValue::Integer(1))],
        );
        assert_eq!(classify(&cleared, &cfg()), None);

        // Sounded deep but a mast shows: still hard.
        let mast = feature(
            "WRECKS",
            &[("VALSOU", AttributeValue::Float(12.0)), ("CATWRK", AttributeValue::Integer(4))],
        );
        assert_eq!(classify(&mast, &cfg()), Some(Severity::Hard));

        // A sounded wreck shallower than the contour: hard, whatever its
        // category says.
        let shoal_wreck = feature(
            "WRECKS",
            &[("VALSOU", AttributeValue::Float(1.5)), ("CATWRK", AttributeValue::Integer(1))],
        );
        assert_eq!(classify(&shoal_wreck, &cfg()), Some(Severity::Hard));
        // An obstruction always under water but sounded shallow: hard.
        let shoal_obs = feature(
            "OBSTRN",
            &[("VALSOU", AttributeValue::Float(1.5)), ("WATLEV", AttributeValue::Integer(3))],
        );
        assert_eq!(classify(&shoal_obs, &cfg()), Some(Severity::Hard));

        // An obstruction always under water and sounded deep: clear.
        let deep_obs = feature(
            "OBSTRN",
            &[("VALSOU", AttributeValue::Float(8.0)), ("WATLEV", AttributeValue::Integer(3))],
        );
        assert_eq!(classify(&deep_obs, &cfg()), None);
        // Awash is hard whatever the sounding says.
        let awash = feature(
            "OBSTRN",
            &[("VALSOU", AttributeValue::Float(8.0)), ("WATLEV", AttributeValue::Integer(5))],
        );
        assert_eq!(classify(&awash, &cfg()), Some(Severity::Hard));
    }

    #[test]
    fn restricted_areas_split_on_entry_prohibited() {
        let prohibited = feature("RESARE", &[("RESTRN", AttributeValue::String("7".into()))]);
        assert_eq!(classify(&prohibited, &cfg()), Some(Severity::Hard));
        let listed = feature("RESARE", &[("RESTRN", AttributeValue::String("1,7,8".into()))]);
        assert_eq!(classify(&listed, &cfg()), Some(Severity::Hard));
        // Anchoring banned, discharge banned: rules for being there, not
        // for passing through.
        let anchoring_banned = feature("RESARE", &[("RESTRN", AttributeValue::String("1".into()))]);
        assert_eq!(classify(&anchoring_banned, &cfg()), None);
        let no_discharge = feature("RESARE", &[("RESTRN", AttributeValue::String("16".into()))]);
        assert_eq!(classify(&no_discharge, &cfg()), None);
        let avoid = feature("RESARE", &[("RESTRN", AttributeValue::String("14".into()))]);
        assert_eq!(classify(&avoid, &cfg()), Some(Severity::Soft));
        assert_eq!(classify(&feature("CTNARE", &[]), &cfg()), None);
        assert_eq!(classify(&feature("MIPARE", &[]), &cfg()), Some(Severity::Soft));
        assert_eq!(classify(&feature("MARCUL", &[]), &cfg()), Some(Severity::Hard));
    }

    /// Spec §6.2 lists GATCON with the hard structures: a lock or sluice
    /// gate drawn over deep dredged water is still a gate.
    #[test]
    fn a_gate_is_a_hard_structure() {
        assert_eq!(classify(&feature("GATCON", &[]), &cfg()), Some(Severity::Hard));
    }

    /// Spec §6.4's air-draft rule, for what else hangs over the water. A
    /// 20 m mast with half a metre kept over it needs 20.5 m.
    #[test]
    fn overhead_cables_pipes_and_conveyors_gate_on_air_draft() {
        for class in ["CBLOHD", "PIPOHD", "CONVYR"] {
            let low = feature(class, &[("VERCLR", AttributeValue::Float(15.0))]);
            assert_eq!(classify(&low, &cfg()), Some(Severity::Hard), "{class} at 15 m");
            let high = feature(class, &[("VERCLR", AttributeValue::Float(20.5))]);
            assert_eq!(classify(&high, &cfg()), None, "{class} at 20.5 m");
            assert_eq!(classify(&feature(class, &[]), &cfg()), Some(Severity::Hard), "{class}, no clearance");
        }
        // A power line's safe clearance is lower than its physical one, and
        // is the one that counts.
        let live = feature(
            "CBLOHD",
            &[("VERCLR", AttributeValue::Float(24.0)), ("VERCSA", AttributeValue::Float(19.0))],
        );
        assert_eq!(classify(&live, &cfg()), Some(Severity::Hard));
    }

    /// A lifting bridge that rises to 18 m does not pass a 20 m mast open
    /// or closed; one that rises to 30 m is a gate.
    #[test]
    fn a_lifting_bridge_is_limited_by_its_open_clearance() {
        let lift = |open: f64| {
            feature(
                "BRIDGE",
                &[
                    ("CATBRG", AttributeValue::Integer(4)),
                    ("VERCCL", AttributeValue::Float(5.0)),
                    ("VERCOP", AttributeValue::Float(open)),
                ],
            )
        };
        assert_eq!(classify(&lift(18.0), &cfg()), Some(Severity::Hard));
        assert!(!is_gate(&lift(18.0), &cfg()));
        assert_eq!(classify(&lift(30.0), &cfg()), Some(Severity::Soft));
        assert!(is_gate(&lift(30.0), &cfg()));
    }

    #[test]
    fn bridges_gate_on_air_draft_and_unsare_on_the_decision() {
        let low = feature("BRIDGE", &[("VERCLR", AttributeValue::Float(18.0))]);
        assert_eq!(classify(&low, &cfg()), Some(Severity::Hard), "20 m mast, 18 m bridge");
        let high = feature("BRIDGE", &[("VERCLR", AttributeValue::Float(35.0))]);
        assert_eq!(classify(&high, &cfg()), None);
        // Half a metre of headroom is kept over the masthead: 20.3 m does not
        // pass a 20 m mast, 20.5 m does.
        let tight = feature("BRIDGE", &[("VERCLR", AttributeValue::Float(20.3))]);
        assert_eq!(classify(&tight, &cfg()), Some(Severity::Hard));
        let enough = feature("BRIDGE", &[("VERCLR", AttributeValue::Float(20.5))]);
        assert_eq!(classify(&enough, &cfg()), None);
        // An opening bridge whose closed clearance clears the mast needs no
        // opening, so it is no gate.
        let high_bascule = feature(
            "BRIDGE",
            &[("CATBRG", AttributeValue::Integer(5)), ("VERCCL", AttributeValue::Float(25.0))],
        );
        assert_eq!(classify(&high_bascule, &cfg()), None);
        assert!(!is_gate(&high_bascule, &cfg()));
        // No clearance stated: do not sail under it.
        assert_eq!(classify(&feature("BRIDGE", &[]), &cfg()), Some(Severity::Hard));
        // Unless it opens: then it is a wait, not a wall.
        let bascule = feature("BRIDGE", &[("CATBRG", AttributeValue::Integer(5))]);
        assert_eq!(classify(&bascule, &cfg()), Some(Severity::Soft));
        let listed = feature("BRIDGE", &[("CATBRG", AttributeValue::String("1,3".into()))]);
        assert_eq!(classify(&listed, &cfg()), Some(Severity::Soft));
        // Only the opening span is a gate; fixed spans and high bridges are not.
        assert!(is_gate(&bascule, &cfg()));
        assert!(!is_gate(&low, &cfg()));
        assert!(!is_gate(&high, &cfg()));

        assert_eq!(classify(&feature("UNSARE", &[]), &cfg()), Some(Severity::Hard));
        let mut relaxed = cfg();
        relaxed.unsare_navigable = true;
        assert_eq!(classify(&feature("UNSARE", &[]), &relaxed), None);
    }

    #[test]
    fn the_ordinary_sea_is_not_an_obstacle() {
        for a in ["SEAARE", "COALNE", "LIGHTS", "BOYLAT", "FAIRWY", "M_COVR", "M_NSYS"] {
            assert_eq!(classify(&feature(a, &[]), &cfg()), None, "{a}");
        }
    }
}
