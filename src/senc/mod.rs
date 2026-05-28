//! SENC (System Electronic Nautical Chart) parser.
//!
//! Parses the TLV (Type-Length-Value) format output by `oexserverd`.
//! Key insight: OSENC files contain **pre-triangulated** area geometry,
//! so no earcut/tessellation is needed - just read and render.

mod reader;
mod records;
mod geometry;
mod features;
mod catalog;
pub mod symbol_lookup;

pub use reader::{SencReader, SencError};
pub use records::{RecordType, SencHeader, CellExtent, ObjectClass, s57_code_to_acronym};
pub use geometry::{TriPrim, TriPrimType, AreaGeometry, LineGeometry, EdgeRef, EdgeTable, BBox, BBoxTileRelation};
pub use features::{AttributeValue, Feature, FeatureType, ChartData};
pub use catalog::{ChartCatalog, ChartInfo, CatalogError};
pub use symbol_lookup::{features_to_instances, soundings_to_instances};
