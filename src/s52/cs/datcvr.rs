use crate::s52::instruction::RenderInstruction;
use crate::senc::Feature;

/// S-52 CS: DATCVR01
/// Returns the LineComplex instruction `HODATA01` to denote chart data coverage bounds.
pub fn datcvr01_instructions(_feature: &Feature) -> Vec<RenderInstruction> {
    vec![RenderInstruction::LineComplex {
        name: "HODATA01".to_string(),
    }]
}
