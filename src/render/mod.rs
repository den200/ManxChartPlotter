//! WGPU-based chart renderer.
//!
//! Renders nautical chart features (areas, lines, points) using
//! a Mercator projection with pan/zoom support.

/// Multisample count for the main render pass.
///
/// OpenCPN antialiases its line work; navcore's hard-edged strokes came out
/// consistently heavier — a 2px dashed boundary measured 3px against the
/// reference's 2px, and that surplus ink dominated the picture-level parity gap
/// once the content differences were fixed. 4x MSAA is the cheapest fix that
/// applies to strokes, polygon edges and symbol art alike.
/// Chosen once at startup from the GPU (see `choose_msaa_samples`); 1 disables
/// multisampling altogether, which is the difference between a usable and an
/// unusable frame rate on a Raspberry Pi.
static MSAA: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

/// Sample count for every render target and pipeline.
pub fn msaa_samples() -> u32 {
    *MSAA.get_or_init(|| 4)
}

/// Fix the sample count. Must be called before the first pipeline is built;
/// later calls are ignored.
pub fn set_msaa_samples(n: u32) {
    let _ = MSAA.set(n.clamp(1, 8));
}

/// How much multisampling this GPU can afford.
///
/// `NAVCORE_MSAA` overrides. Otherwise 4x, except on a software rasteriser or
/// the low-power parts found in single-board computers, where the fill-rate
/// cost of 4x is the whole frame budget and a slightly heavier stroke is a much
/// better trade than a slideshow.
pub fn choose_msaa_samples(info: &wgpu::AdapterInfo) -> u32 {
    if let Ok(v) = std::env::var("NAVCORE_MSAA") {
        if let Ok(n) = v.trim().parse::<u32>() {
            return n.clamp(1, 8);
        }
    }
    if info.device_type == wgpu::DeviceType::Cpu {
        return 1;
    }
    let name = info.name.to_ascii_lowercase();
    const LOW_POWER: [&str; 6] = ["v3d", "videocore", "llvmpipe", "swiftshader", "mali", "adreno"];
    if LOW_POWER.iter().any(|n| name.contains(n)) {
        1
    } else {
        4
    }
}

mod camera;
pub mod ui;
mod ui_instruments;
pub mod mariner;
pub mod ui_ais;
pub mod ui_boat;
pub mod ui_ownship;
mod ui_panels;
pub mod ui_routes;
pub mod ui_batch;
pub mod ui_weather;
pub mod ui_wind;
pub mod spectrum;
pub use camera::MAX_TILT;
pub mod font;
mod colors;
mod label;
mod line_vertices;
pub mod lc_pattern;
pub mod text_layout;
pub mod projection;
pub mod s52_styles;
mod state;
pub mod patterns;
pub mod sectors;
pub mod symbols;
pub mod text;

pub use camera::Camera;
pub use colors::{depth_color, DepthPalette, LAND_COLOR, WATER_DEEP, WATER_SHALLOW};
pub use lc_pattern::{lc_segments, lc_symbol_index, lc_symbol_names, LcAtlas, LcSegment, LcSymbolGpu};
pub use line_vertices::{build_line_vertices, build_line_vertices_multi_indexed, PRIMITIVE_RESTART_INDEX};
pub use projection::Projection;
pub use state::RenderState;
pub use state::{LineStyle, LineUniforms, LineVertex};
pub use label::{LabelGlyphInstance, LabelRenderer};
pub use symbols::{SymbolId, SymbolInstance, SymbolRenderer};
pub use patterns::{PatternRenderer, PatternVertex, pattern_id_from_name};
pub use sectors::{SectorInstance, SectorRenderer};
pub use text::{SoundingInstance, TextRenderer};
pub use text_layout::{layout_text, layout_light_text, declutter_and_layout_labels, TextParams, HJust, VJust};

use std::sync::OnceLock;

static DEBUG_RENDER_MODE: OnceLock<u8> = OnceLock::new();

static DEBUG_SKIP: OnceLock<Vec<String>> = OnceLock::new();

/// NAVCORE_SKIP=bg,area,pattern,line,sector,symbol,text,label,mariner leaves
/// those chart layers out of the frame. For bisecting a GPU hang by layer.
pub fn debug_skip(kind: &str) -> bool {
    DEBUG_SKIP
        .get_or_init(|| {
            std::env::var("NAVCORE_SKIP")
                .map(|v| v.split(',').map(|s| s.trim().to_string()).collect())
                .unwrap_or_default()
        })
        .iter()
        .any(|k| k == kind)
}

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
