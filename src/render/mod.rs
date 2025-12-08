//! WGPU-based chart renderer.
//!
//! Renders nautical chart features (areas, lines, points) using
//! a Mercator projection with pan/zoom support.

mod camera;
mod colors;
mod projection;
pub mod s52_styles;
mod state;
pub mod symbols;

pub use camera::Camera;
pub use colors::{depth_color, DepthPalette, LAND_COLOR, WATER_DEEP, WATER_SHALLOW};
pub use projection::Projection;
pub use state::RenderState;
pub use symbols::{SymbolId, SymbolInstance, SymbolRenderer};
