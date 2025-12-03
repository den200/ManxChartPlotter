//! WGPU-based chart renderer.
//!
//! Renders nautical chart features (areas, lines, points) using
//! a Mercator projection with pan/zoom support.

mod camera;
mod colors;
mod projection;
mod state;

pub use camera::Camera;
pub use colors::{depth_color, DepthPalette, LAND_COLOR, WATER_DEEP, WATER_SHALLOW};
pub use projection::Projection;
pub use state::RenderState;
