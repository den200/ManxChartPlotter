//! QUAPOS01 - Quality of Position Conditional Symbology Procedure
//!
//! Matches OpenCPN logic:
//! - QUAPOS 2..9 -> LC(LOWACC21)
//! - For COALNE (coastline), if CONRAD=1 -> LS(SOLD,3,CHMGF) + LS(SOLD,1,CSTLN)
//! - Otherwise -> LS(SOLD,1,CSTLN)

use crate::s52::instruction::{LinePattern, RenderInstruction};
use crate::s52::MarinerSettings;
use crate::senc::{Feature, ObjectClass};

/// QUAPOS01 - Position accuracy styling per OpenCPN.
pub fn quapos01_instructions(
    feature: &Feature,
    _settings: &MarinerSettings,
) -> Vec<RenderInstruction> {
    let quapos = feature.attribute_int("QUAPOS").unwrap_or(0);

    if (2..10).contains(&quapos) {
        return vec![RenderInstruction::LineComplex {
            name: "LOWACC21".to_string(),
        }];
    }

    if feature.object_class == ObjectClass::Coastline {
        let conrad = feature.attribute_int("CONRAD").unwrap_or(0);
        if conrad == 1 {
            return vec![
                RenderInstruction::LineStyle {
                    pattern: LinePattern::Solid,
                    width: 3,
                    color: "CHMGF".to_string(),
                },
                RenderInstruction::LineStyle {
                    pattern: LinePattern::Solid,
                    width: 1,
                    color: "CSTLN".to_string(),
                },
            ];
        }
    }

    vec![RenderInstruction::LineStyle {
        pattern: LinePattern::Solid,
        width: 1,
        color: "CSTLN".to_string(),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::senc::{AttributeValue, FeatureType};
    use std::collections::HashMap;

    fn make_feature(object_class: ObjectClass, attrs: Vec<(&str, AttributeValue)>) -> Feature {
        let mut attributes = HashMap::new();
        for (k, v) in attrs {
            attributes.insert(k.to_string(), v);
        }
        Feature {
            type_code: 0,
            object_class,
            feature_type: FeatureType::Line,
            attributes,
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        }
    }

    #[test]
    fn test_low_accuracy_quapos() {
        let feature = make_feature(
            ObjectClass::Coastline,
            vec![("QUAPOS", AttributeValue::Integer(4))],
        );
        let settings = MarinerSettings::default();
        let instructions = quapos01_instructions(&feature, &settings);
        assert!(matches!(
            instructions.as_slice(),
            [RenderInstruction::LineComplex { .. }]
        ));
    }

    #[test]
    fn test_coastline_conrad_emphasis() {
        let feature = make_feature(
            ObjectClass::Coastline,
            vec![("CONRAD", AttributeValue::Integer(1))],
        );
        let settings = MarinerSettings::default();
        let instructions = quapos01_instructions(&feature, &settings);
        assert_eq!(instructions.len(), 2);
    }
}
