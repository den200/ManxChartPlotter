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
    /// Inside this distance of a hazard is as forbidden as the hazard.
    pub offing_min_nm: f64,
    /// Below this distance the route pays a rising cost — prefer sea room
    /// when it is free.
    pub offing_soft_nm: f64,
    /// Grid cell size. The spec says match the finest channel to transit;
    /// 60 m resolves anything a sailing yacht calls a channel.
    pub grid_res_m: f64,
    /// Unsurveyed areas: unsafe by default (decision §10.4).
    pub unsare_navigable: bool,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            draft_m: 2.0,
            ukc_m: 0.5,
            squat_m: 0.0,
            air_draft_m: 20.0,
            offing_min_nm: 0.2,
            offing_soft_nm: 0.5,
            grid_res_m: 60.0,
            unsare_navigable: false,
        }
    }
}

impl SafetyConfig {
    /// The depth that divides water into safe and not, before tide (M6 adds
    /// `− tide_height(t)`).
    pub fn safety_contour_m(&self) -> f64 {
        self.draft_m + self.ukc_m + self.squat_m
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

/// Does the RESTRN list prohibit entry? Value 7 is "entry prohibited".
fn entry_prohibited(feature: &Feature) -> bool {
    feature
        .attribute_str("RESTRN")
        .map(|s| s.split(',').any(|v| v.trim() == "7"))
        .unwrap_or(false)
        || feature.attribute_int("RESTRN") == Some(7)
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
        "PONTON" | "DAMCON" | "SLCONS" | "FLODOC" | "HULKES" | "DYKCON" | "CAUSWY" => {
            Some(Severity::Hard)
        }
        "UNSARE" => (!safety.unsare_navigable).then_some(Severity::Hard),
        "MARCUL" => Some(Severity::Hard),
        "RESARE" => Some(if entry_prohibited(feature) {
            Severity::Hard
        } else {
            Severity::Soft
        }),
        "MIPARE" | "CTNARE" => Some(Severity::Soft),
        // Separation zones are for staying out of; the lanes themselves are
        // M4 cost territory, not obstacles.
        "TSEZNE" => Some(Severity::Hard),
        "BRIDGE" => {
            let verclr = feature.attribute_float("VERCLR");
            let fits = verclr
                .map(|c| safety.air_draft_m + 0.5 <= c)
                .unwrap_or(false);
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
        let anchoring_banned = feature("RESARE", &[("RESTRN", AttributeValue::String("1".into()))]);
        assert_eq!(classify(&anchoring_banned, &cfg()), Some(Severity::Soft));
        assert_eq!(classify(&feature("CTNARE", &[]), &cfg()), Some(Severity::Soft));
        assert_eq!(classify(&feature("MARCUL", &[]), &cfg()), Some(Severity::Hard));
    }

    #[test]
    fn bridges_gate_on_air_draft_and_unsare_on_the_decision() {
        let low = feature("BRIDGE", &[("VERCLR", AttributeValue::Float(18.0))]);
        assert_eq!(classify(&low, &cfg()), Some(Severity::Hard), "20 m mast, 18 m bridge");
        let high = feature("BRIDGE", &[("VERCLR", AttributeValue::Float(35.0))]);
        assert_eq!(classify(&high, &cfg()), None);
        // No clearance stated: do not sail under it.
        assert_eq!(classify(&feature("BRIDGE", &[]), &cfg()), Some(Severity::Hard));

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
