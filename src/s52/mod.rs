//! S-52 Presentation Library implementation.
//!
//! Parses OpenCPN's chartsymbols.xml and provides:
//! - Lookup tables for feature→symbol mapping
//! - Display category filtering (Displaybase/Standard/Other)
//! - Render instruction parsing (LS, AC, SY, etc.)
//! - Conditional symbology procedure dispatch
//! - Line-style (LC) symbol definitions

pub mod cs;
mod engine;
pub mod instruction;
pub mod lc;
mod lookup;

mod parser;
pub mod patterns;
mod settings;

pub use cs::{depare02, depare02_color_token, depcnt02, depcnt02_params, is_safety_contour, execute_cs, DepthColorToken, DepthContourStyle};
pub use cs::{light_render_info, light_sector_info, lights06_symbol, litdsn01, LightRenderInfo, LightSectorInfo, LightSymbol};
pub use cs::{sndfrm02, sounding_color, sounding_color_rgb, SoundingRenderInfo};
pub use engine::{S52Engine, ResolvedFeature};
pub use instruction::{LineOp, LinePattern, LineStyleKey, RenderInstruction, parse_instructions};
pub use lc::{LineStyleSymbol, LineStyleTable};
pub use lookup::{DisplayCategory, DisplayPriority, GeometryType, LookupEntry, LookupTables};

pub use parser::parse_chartsymbols;
pub use settings::{DepthShadeMode, DepthUnit, MarinerSettings};
