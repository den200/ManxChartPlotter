//! DEPARE02 - Depth Area Conditional Symbology Procedure
//!
//! Determines depth area coloring based on DRVAL1/DRVAL2 attributes
//! and the mariner's selected safety depth.

use crate::s52::instruction::{LinePattern, RenderInstruction};
use crate::s52::{DepthShadeMode, MarinerSettings};
use crate::senc::{Feature, ObjectClass};

/// S-52 depth color tokens from chartsymbols.xml color table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepthColorToken {
    /// DEPIT - Drying/intertidal area (below chart datum)
    Drying,
    /// DEPVS - Very shallow (0 to safety_depth) - DANGER
    VeryShallow,
    /// DEPMS - Medium shallow (crosses safety depth)
    MediumShallow,
    /// DEPMD - Medium deep (safe but shallow)
    MediumDeep,
    /// DEPDW - Deep water
    DeepWater,
}

impl DepthColorToken {
    /// Convert to S-52 color token string
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Drying => "DEPIT",
            Self::VeryShallow => "DEPVS",
            Self::MediumShallow => "DEPMS",
            Self::MediumDeep => "DEPMD",
            Self::DeepWater => "DEPDW",
        }
    }
}

/// DEPARE02 - Depth area coloring based on safety depth
///
/// Returns color token based on depth range (DRVAL1/DRVAL2) vs mariner contour settings.
/// This matches OpenCPN's DEPARE01 logic (s52cnsy.cpp:617-688).
pub fn depare02(feature: &Feature, settings: &MarinerSettings) -> DepthColorToken {
    // OpenCPN defaults: drval1 = -1.0 (drying)
    let drval1 = feature.drval1().unwrap_or(-1.0);
    // If drval2 missing, use drval1 + 0.01 per OpenCPN
    let drval2 = feature.drval2().unwrap_or(drval1 + 0.01);

    // Ensure drval2 > drval1 (OpenCPN does this for bad charts)
    let drval2 = if drval2 <= drval1 {
        drval1 + 0.01
    } else {
        drval2
    };

    let shallow = settings.shallow_contour as f64;
    let safety = settings.safety_contour as f64;
    let deep = settings.deep_contour as f64;

    // Start with drying (DEPIT) as default
    let mut token = DepthColorToken::Drying;

    // Any positive depth range -> Very Shallow (DEPVS)
    // This is the ONLY check that uses both drval1 and drval2
    if drval1 >= 0.0 && drval2 > 0.0 {
        token = DepthColorToken::VeryShallow;
    }

    if settings.depth_shade_mode == DepthShadeMode::TwoShades {
        // Two-shade mode: Only Safe (DEPDW) or Unsafe (DEPVS)
        if drval1 >= safety && drval2 > safety {
            token = DepthColorToken::DeepWater;
        }
    } else {
        // Four-shade mode per OpenCPN SEABED01: only upgrade when BOTH endpoints
        // of the depth range clear the threshold. Using drval1 alone is too
        // permissive — a polygon with drval1=3, drval2=30 around the 2m shallow
        // threshold would falsely promote all the way to deep-water shade.
        if drval1 >= shallow && drval2 > shallow {
            token = DepthColorToken::MediumShallow;
        }

        if drval1 >= safety && drval2 > safety {
            token = DepthColorToken::MediumDeep;
        }

        if drval1 >= deep && drval2 > deep {
            token = DepthColorToken::DeepWater;
        }
    }

    token
}

/// Get color token string directly for a depth area feature
pub fn depare02_color_token(feature: &Feature, settings: &MarinerSettings) -> &'static str {
    depare02(feature, settings).as_str()
}

/// Full DEPARE01/02 instructions including DRGARE logic
pub fn depare02_instructions(
    feature: &Feature,
    settings: &MarinerSettings,
) -> Vec<RenderInstruction> {
    let mut instructions = Vec::new();

    // 1. Get area color based on depth
    let color_token = depare02_color_token(feature, settings);
    instructions.push(RenderInstruction::AreaColor {
        color: color_token.to_string(),
    });

    // 2. Special logic for DRGARE (Dredged Area)
    //
    // OpenCPN can emit AP(DRGARE01), but our current pattern fill path renders the
    // hatch much heavier than the reference and dominates harbor basins. Keep the
    // dredged boundary styling and defer the fill pattern until AP parity improves.
    if feature.object_class == ObjectClass::DredgedArea {
        // Add dashed boundary line
        instructions.push(RenderInstruction::LineStyle {
            pattern: LinePattern::Dashed,
            width: 1,
            color: "CHGRF".to_string(),
        });
    }

    instructions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::senc::{AttributeValue, FeatureType, ObjectClass};
    use std::collections::HashMap;

    fn make_depare(drval1: f64, drval2: f64) -> Feature {
        let mut attributes = HashMap::new();
        attributes.insert("DRVAL1".to_string(), AttributeValue::Float(drval1));
        attributes.insert("DRVAL2".to_string(), AttributeValue::Float(drval2));
        Feature {
            type_code: 0,
            object_class: ObjectClass::DepthArea,
            feature_type: FeatureType::Area,
            attributes,
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        }
    }

    fn make_drgare(drval1: f64, drval2: f64) -> Feature {
        let mut f = make_depare(drval1, drval2);
        f.object_class = ObjectClass::DredgedArea;
        f
    }

    #[test]
    fn test_drying_area() {
        let feature = make_depare(-2.0, 0.0);
        let settings = MarinerSettings::default();
        assert_eq!(depare02(&feature, &settings), DepthColorToken::Drying);
    }

    #[test]
    fn test_very_shallow() {
        let feature = make_depare(0.0, 1.5);
        let settings = MarinerSettings::default();
        assert_eq!(depare02(&feature, &settings), DepthColorToken::VeryShallow);
    }

    #[test]
    fn test_medium_shallow() {
        let feature = make_depare(2.5, 4.0);
        let settings = MarinerSettings::default();
        assert_eq!(
            depare02(&feature, &settings),
            DepthColorToken::MediumShallow
        );
    }

    #[test]
    fn test_medium_deep() {
        let feature = make_depare(12.0, 20.0);
        let settings = MarinerSettings::default();
        assert_eq!(depare02(&feature, &settings), DepthColorToken::MediumDeep);
    }

    #[test]
    fn test_deep_water() {
        let feature = make_depare(35.0, 50.0);
        let settings = MarinerSettings::default();
        assert_eq!(depare02(&feature, &settings), DepthColorToken::DeepWater);
    }

    #[test]
    fn test_crosses_shallow_contour() {
        let feature = make_depare(1.0, 3.0);
        let settings = MarinerSettings::default();
        assert_eq!(depare02(&feature, &settings), DepthColorToken::VeryShallow);
    }

    #[test]
    fn test_drval1_only_check() {
        // Malformed chart where drval2<drval1; helper normalises drval2 = drval1+0.01,
        // so behaviour is driven by drval1 alone.
        let feature = make_depare(5.0, 3.0);
        let settings = MarinerSettings::default();
        assert_eq!(
            depare02(&feature, &settings),
            DepthColorToken::MediumShallow
        );
    }

    #[test]
    fn test_custom_safety_contour() {
        let feature = make_depare(6.0, 15.0);
        let mut settings = MarinerSettings::default();
        settings.safety_contour = 5.0;
        assert_eq!(depare02(&feature, &settings), DepthColorToken::MediumDeep);
    }

    #[test]
    fn test_two_shades_safe() {
        let feature = make_depare(12.0, 20.0);
        let mut settings = MarinerSettings::default();
        settings.depth_shade_mode = DepthShadeMode::TwoShades;
        settings.safety_contour = 10.0;
        assert_eq!(depare02(&feature, &settings), DepthColorToken::DeepWater);
    }

    #[test]
    fn test_two_shades_unsafe() {
        let feature = make_depare(5.0, 15.0);
        let mut settings = MarinerSettings::default();
        settings.depth_shade_mode = DepthShadeMode::TwoShades;
        settings.safety_contour = 10.0;
        assert_eq!(depare02(&feature, &settings), DepthColorToken::VeryShallow);
    }

    #[test]
    fn test_drgare_instructions() {
        let feature = make_drgare(5.0, 10.0);
        let settings = MarinerSettings::default();
        let instrs = depare02_instructions(&feature, &settings);

        // Should have AC and LS
        assert_eq!(instrs.len(), 2);
        assert!(matches!(instrs[0], RenderInstruction::AreaColor { .. }));
        if let RenderInstruction::LineStyle { pattern, .. } = &instrs[1] {
            assert_eq!(*pattern, LinePattern::Dashed);
        } else {
            panic!("Expected LineStyle");
        }
    }
}
