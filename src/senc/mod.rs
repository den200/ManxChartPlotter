//! SENC (System Electronic Nautical Chart) parser.
//!
//! Parses the TLV (Type-Length-Value) format output by `oexserverd`.
//! Key insight: OSENC files contain **pre-triangulated** area geometry,
//! so no earcut/tessellation is needed - just read and render.

mod reader;
mod records;
mod geometry;
mod features;

pub use reader::SencReader;
pub use records::{RecordType, SencHeader};
pub use geometry::{TriPrim, TriPrimType, AreaGeometry};
pub use features::{Feature, FeatureType, ChartData};
