//! WGPU-based chart renderer.
//!
//! Renders nautical chart features (areas, lines, points) using
//! a Mercator projection with pan/zoom support.

mod camera;
mod colors;
mod label;
mod line_vertices;
pub mod lc_pattern;
pub mod text_layout;
mod projection;
pub mod s52_styles;
mod state;
pub mod patterns;
pub mod symbols;
pub mod text;

pub use camera::Camera;
pub use colors::{depth_color, DepthPalette, LAND_COLOR, WATER_DEEP, WATER_SHALLOW};
pub use lc_pattern::{LcPatternData, LcPatternTable, LcRenderConfig, LcStamp, compute_phase_offset, generate_stamps_along_polyline, generate_stamps_along_polyline_with_phase, stamps_to_line_vertices};
pub use line_vertices::{build_line_vertices, build_line_vertices_multi_indexed, PRIMITIVE_RESTART_INDEX};
pub use projection::Projection;
pub use state::RenderState;
pub use state::{LineStyle, LineUniforms, LineVertex};
pub use label::{LabelGlyphInstance, LabelRenderer, glyph_uv_rect};
pub use symbols::{SymbolId, SymbolInstance, SymbolRenderer};
pub use patterns::{PatternRenderer, PatternVertex, pattern_id_from_name};
pub use text::{SoundingInstance, TextRenderer};
pub use text_layout::{layout_text, layout_light_text, declutter_and_layout_labels, TextParams, HJust, VJust};

use std::sync::OnceLock;

static DEBUG_RENDER_MODE: OnceLock<u8> = OnceLock::new();

/// NAVCORE_DEBUG_RENDER:
/// 1 = solid magenta symbols, 2 = no depth/cull for symbols, 3 = atlas debug quad.
pub fn debug_render_mode() -> u8 {
    *DEBUG_RENDER_MODE.get_or_init(|| {
        std::env::var("NAVCORE_DEBUG_RENDER")
            .ok()
            .and_then(|v| v.parse::<u8>().ok())
            .filter(|v| (1..=3).contains(v))
            .unwrap_or(0)
    })
}
