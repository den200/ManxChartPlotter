//! SLCONS03 - Shoreline Construction Conditional Symbology Procedure
//!
//! Matches OpenCPN logic:
//! - QUAPOS 2..9 -> LC(LOWACC01)
//! - CONDTN 1/2 -> LS(DASH,1,CSTLN)
//! - CATSLC 6/15/16 -> LS(SOLD,4,CSTLN)
//! - WATLEV 2 -> LS(SOLD,2,CSTLN)
//! - WATLEV 3/4 -> LS(DASH,2,CSTLN)
//! - default -> LS(SOLD,2,CSTLN)

use crate::s52::instruction::{LinePattern, RenderInstruction};
use crate::s52::MarinerSettings;
use crate::senc::Feature;

/// S-52 shoreline construction style variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlconsStyle {
    /// Major structure (wharf types) - solid thick line
    MajorStructure,
    /// Minor structure - solid medium line
    MinorStructure,
    /// Submerged structure (WATLEV=3/4) - dashed line
    Submerged,
    /// Under construction or ruined (CONDTN=1/2) - dashed line
    Provisional,
}

impl SlconsStyle {
    pub fn pattern(&self) -> LinePattern {
        match self {
            Self::MajorStructure => LinePattern::Solid,
            Self::MinorStructure => LinePattern::Solid,
            Self::Submerged => LinePattern::Dashed,
            Self::Provisional => LinePattern::Dashed,
        }
    }

    pub fn width(&self) -> u8 {
        match self {
            Self::MajorStructure => 4,
            Self::MinorStructure => 2,
            Self::Submerged => 2,
            Self::Provisional => 1,
        }
    }

    pub fn color_token(&self) -> &'static str {
        "CSTLN"
    }
}

/// Result for SLCONS03 evaluation.
pub enum SlconsResult {
    LineStyle(SlconsStyle),
    LineComplex(&'static str),
}

/// SLCONS03 - Shoreline construction styling per OpenCPN.
pub fn slcons03(feature: &Feature, _settings: &MarinerSettings) -> SlconsResult {
    let catslc = feature.attribute_int("CATSLC").unwrap_or(0);
    let watlev = feature.attribute_int("WATLEV").unwrap_or(0);
    let condtn = feature.attribute_int("CONDTN").unwrap_or(0);
    let quapos = feature.attribute_int("QUAPOS").unwrap_or(0);

    if (2..10).contains(&quapos) {
        return SlconsResult::LineComplex("LOWACC01");
    }

    if condtn == 1 || condtn == 2 {
        return SlconsResult::LineStyle(SlconsStyle::Provisional);
    }

    if matches!(catslc, 6 | 15 | 16) {
        return SlconsResult::LineStyle(SlconsStyle::MajorStructure);
    }

    if watlev == 2 {
        return SlconsResult::LineStyle(SlconsStyle::MinorStructure);
    }

    if watlev == 3 || watlev == 4 {
        return SlconsResult::LineStyle(SlconsStyle::Submerged);
    }

    SlconsResult::LineStyle(SlconsStyle::MinorStructure)
}

/// Render instructions for SLCONS03.
pub fn slcons03_instructions(feature: &Feature, settings: &MarinerSettings) -> Vec<RenderInstruction> {
    match slcons03(feature, settings) {
        SlconsResult::LineStyle(style) => vec![RenderInstruction::LineStyle {
            pattern: style.pattern(),
            width: style.width(),
            color: style.color_token().to_string(),
        }],
        SlconsResult::LineComplex(name) => vec![RenderInstruction::LineComplex {
            name: name.to_string(),
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::senc::{AttributeValue, FeatureType, ObjectClass};
    use std::collections::HashMap;

    fn make_slcons(catslc: Option<i32>, watlev: Option<i32>, condtn: Option<i32>, quapos: Option<i32>) -> Feature {
        let mut attributes = HashMap::new();
        if let Some(v) = catslc {
            attributes.insert("CATSLC".to_string(), AttributeValue::Integer(v));
        }
        if let Some(v) = watlev {
            attributes.insert("WATLEV".to_string(), AttributeValue::Integer(v));
        }
        if let Some(v) = condtn {
            attributes.insert("CONDTN".to_string(), AttributeValue::Integer(v));
        }
        if let Some(v) = quapos {
            attributes.insert("QUAPOS".to_string(), AttributeValue::Integer(v));
        }
        Feature {
            type_code: 0,
            object_class: ObjectClass::ShorelineConstruction,
            feature_type: FeatureType::Line,
            attributes,
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        }
    }

    #[test]
    fn test_major_structure_wharf() {
        let feature = make_slcons(Some(6), None, None, None);
        let settings = MarinerSettings::default();
        let result = slcons03(&feature, &settings);
        assert!(matches!(result, SlconsResult::LineStyle(SlconsStyle::MajorStructure)));
    }

    #[test]
    fn test_minor_structure_default() {
        let feature = make_slcons(None, None, None, None);
        let settings = MarinerSettings::default();
        let result = slcons03(&feature, &settings);
        assert!(matches!(result, SlconsResult::LineStyle(SlconsStyle::MinorStructure)));
    }

    #[test]
    fn test_submerged_structure() {
        let feature = make_slcons(None, Some(3), None, None);
        let settings = MarinerSettings::default();
        let result = slcons03(&feature, &settings);
        assert!(matches!(result, SlconsResult::LineStyle(SlconsStyle::Submerged)));
    }

    #[test]
    fn test_low_accuracy() {
        let feature = make_slcons(None, None, None, Some(2));
        let settings = MarinerSettings::default();
        let result = slcons03(&feature, &settings);
        assert!(matches!(result, SlconsResult::LineComplex("LOWACC01")));
    }
}
