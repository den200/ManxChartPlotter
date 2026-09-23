// Sounding (depth) rendering shader
// Renders depth values as simple numeric text using procedural 7-segment style digits
// Each instance is a sounding with position and depth value

struct CameraUniform {
    view_proj: mat4x4<f32>,
    view_size: vec2<f32>,
    pixels_per_meter: f32,
    _pad: f32,
}

@group(0) @binding(0) var<uniform> camera: CameraUniform;
@group(0) @binding(1) var<storage, read> palette: array<vec4<f32>>;

struct InstanceInput {
    @location(0) position: vec2<f32>,  // Metres from this draw's origin (tile centre)
    @location(1) depth: f32,           // Whole-part depth in display units
    @location(2) flags: u32,           // SNDFRM02 flags
    @location(3) scale: f32,           // Soft-SCAMIN scale
    @location(4) color_index: u32,     // SNDG1/SNDG2 palette index
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) local_uv: vec2<f32>,  // Local UV for digit rendering
    @location(1) depth_value: f32,     // Pass depth to fragment shader
    @location(2) sounding_flags: u32,  // Pass flags to fragment shader
    @location(3) @interpolate(flat) color_index: u32,
}

// Flag bit definitions (from SoundingRenderInfo::to_flags)
const FLAG_IS_SHALLOW: u32 = 1u;      // Bit 0: shallow/danger (SNDG2 black)
const FLAG_IS_DRYING: u32 = 2u;       // Bit 1: drying height
const FLAG_UNCERTAINTY: u32 = 4u;     // Bit 2: show question mark
const FLAG_IS_SWEPT: u32 = 8u;        // Bit 3: swept depth
const FLAG_HAS_DECIMAL: u32 = 16u;    // Bit 4: decimal digit present

// Sounding text size in screen pixels. Matched to openCPN's SOUNDG25 atlas
// glyph (~6 × 10 px per digit) scaled up ~2× so a 2-digit reading is ~24 × 20 px
// on screen — close enough in size and weight to pass as the same reference,
// pending a full atlas-glyph rewrite.
const TEXT_WIDTH: f32 = 44.0;
const TEXT_HEIGHT: f32 = 20.0;

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    instance: InstanceInput,
) -> VertexOutput {
    var out: VertexOutput;

    // Generate quad corners from vertex_index (0-5 for two triangles)
    var corner: vec2<f32>;
    var uv: vec2<f32>;

    switch (vertex_index) {
        case 0u: { corner = vec2(-0.5, -0.5); uv = vec2(0.0, 1.0); }  // BL
        case 1u: { corner = vec2(0.5, -0.5);  uv = vec2(1.0, 1.0); }  // BR
        case 2u: { corner = vec2(-0.5, 0.5);  uv = vec2(0.0, 0.0); }  // TL
        case 3u: { corner = vec2(-0.5, 0.5);  uv = vec2(0.0, 0.0); }  // TL
        case 4u: { corner = vec2(0.5, -0.5);  uv = vec2(1.0, 1.0); }  // BR
        case 5u: { corner = vec2(0.5, 0.5);   uv = vec2(1.0, 0.0); }  // TR
        default: { corner = vec2(0.0); uv = vec2(0.0); }
    }

    // Scale to text size
    let pixel_offset = corner * vec2<f32>(TEXT_WIDTH, TEXT_HEIGHT) * instance.scale;

    // Transform world position to clip space
    let world_pos = vec4<f32>(instance.position, 0.0, 1.0);
    var clip_pos = camera.view_proj * world_pos;

    // Add screen-space offset (text stays fixed size)
    let ndc_offset = pixel_offset / camera.view_size * 2.0;
    clip_pos.x += ndc_offset.x * clip_pos.w;
    clip_pos.y += ndc_offset.y * clip_pos.w;
    clip_pos.z = 0.0; // Draw soundings on top of everything

    out.clip_position = clip_pos;
    out.local_uv = uv;
    out.depth_value = instance.depth;
    out.sounding_flags = instance.flags;
    out.color_index = instance.color_index;

    return out;
}

// 7-segment display digit patterns (bits: top, top-right, bottom-right, bottom, bottom-left, top-left, middle)
// Each digit is encoded as which segments are ON
fn get_digit_segments(digit: i32) -> u32 {
    switch (digit) {
        case 0: { return 0x3Fu; } // 0b0111111 = all except middle
        case 1: { return 0x06u; } // 0b0000110 = right side only
        case 2: { return 0x5Bu; } // 0b1011011
        case 3: { return 0x4Fu; } // 0b1001111
        case 4: { return 0x66u; } // 0b1100110
        case 5: { return 0x6Du; } // 0b1101101
        case 6: { return 0x7Du; } // 0b1111101
        case 7: { return 0x07u; } // 0b0000111
        case 8: { return 0x7Fu; } // 0b1111111
        case 9: { return 0x6Fu; } // 0b1101111
        default: { return 0x00u; }
    }
}

// Check if a point is inside a segment box. Segments are drawn as thicker slabs
// so digits read as bold black numerals (closer to openCPN's SOUNDG25 glyphs)
// rather than the previous hairline 7-segment readout.
fn in_segment(uv: vec2<f32>, seg_id: u32, char_uv: vec2<f32>) -> bool {
    let x = char_uv.x;
    let y = char_uv.y;

    switch (seg_id) {
        // Top horizontal
        case 0u: { return x > 0.10 && x < 0.90 && y < 0.20; }
        // Top-right vertical
        case 1u: { return x > 0.72 && y > 0.06 && y < 0.50; }
        // Bottom-right vertical
        case 2u: { return x > 0.72 && y > 0.50 && y < 0.94; }
        // Bottom horizontal
        case 3u: { return x > 0.10 && x < 0.90 && y > 0.80; }
        // Bottom-left vertical
        case 4u: { return x < 0.28 && y > 0.50 && y < 0.94; }
        // Top-left vertical
        case 5u: { return x < 0.28 && y > 0.06 && y < 0.50; }
        // Middle horizontal
        case 6u: { return x > 0.10 && x < 0.90 && y > 0.40 && y < 0.60; }
        default: { return false; }
    }
}

// Render a single digit at given position
fn render_digit(char_uv: vec2<f32>, digit: i32) -> f32 {
    let segs = get_digit_segments(digit);

    var intensity = 0.0;
    for (var i = 0u; i < 7u; i++) {
        if ((segs & (1u << i)) != 0u && in_segment(char_uv, i, char_uv)) {
            intensity = 1.0;
            break;
        }
    }

    return intensity;
}

// Render a smaller subscript digit inside a cell.
fn render_subscript_digit(char_uv: vec2<f32>, digit: i32) -> f32 {
    let scale = 0.7;
    let offset = vec2<f32>(0.15, 0.30);
    let sub_uv = (char_uv - offset) / scale;

    if (sub_uv.x < 0.0 || sub_uv.x > 1.0 || sub_uv.y < 0.0 || sub_uv.y > 1.0) {
        return 0.0;
    }

    return render_digit(sub_uv, digit);
}

fn render_underscore(char_uv: vec2<f32>) -> f32 {
    if (char_uv.y > 0.86 && char_uv.y < 0.95 && char_uv.x > 0.15 && char_uv.x < 0.85) {
        return 1.0;
    }
    return 0.0;
}

fn render_question_mark(char_uv: vec2<f32>) -> f32 {
    let top = char_uv.y < 0.12 && char_uv.x > 0.2 && char_uv.x < 0.8;
    let upper_right = char_uv.x > 0.8 && char_uv.y > 0.12 && char_uv.y < 0.55;
    let middle = char_uv.y > 0.46 && char_uv.y < 0.54 && char_uv.x > 0.2 && char_uv.x < 0.8;
    // Dot at bottom - circular distance check
    let dx = char_uv.x - 0.5;
    let dy = char_uv.y - 0.85;
    let dot = dx * dx + dy * dy < 0.02;
    if (top || upper_right || middle || dot) {
        return 1.0;
    }
    return 0.0;
}

fn render_swept(char_uv: vec2<f32>) -> f32 {
    if (char_uv.y > 0.45 && char_uv.y < 0.55 && char_uv.x > 0.2 && char_uv.x < 0.8) {
        return 1.0;
    }
    return 0.0;
}

fn digit_for_index(whole: u32, digit_count: u32, index: u32) -> i32 {
    let d0 = whole % 10u;
    let d1 = (whole / 10u) % 10u;
    let d2 = (whole / 100u) % 10u;
    let d3 = (whole / 1000u) % 10u;
    let d4 = (whole / 10000u) % 10u;

    if (digit_count == 1u) {
        return i32(d0);
    }
    if (digit_count == 2u) {
        return i32(select(d1, d0, index == 1u));
    }
    if (digit_count == 3u) {
        if (index == 0u) { return i32(d2); }
        if (index == 1u) { return i32(d1); }
        return i32(d0);
    }
    if (digit_count == 4u) {
        if (index == 0u) { return i32(d3); }
        if (index == 1u) { return i32(d2); }
        if (index == 2u) { return i32(d1); }
        return i32(d0);
    }
    // digit_count == 5
    if (index == 0u) { return i32(d4); }
    if (index == 1u) { return i32(d3); }
    if (index == 2u) { return i32(d2); }
    if (index == 3u) { return i32(d1); }
    return i32(d0);
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let uv = in.local_uv;
    let flags = in.sounding_flags;

    let has_decimal = (flags & FLAG_HAS_DECIMAL) != 0u;
    let digit_count = (flags >> 5) & 0x7u;
    let decimal_digit = (flags >> 8) & 0xFu;
    let whole = (flags >> 12) & 0x1FFFFu;

    let show_uncertainty = (flags & FLAG_UNCERTAINTY) != 0u;
    let is_swept = (flags & FLAG_IS_SWEPT) != 0u;
    let is_drying = (flags & FLAG_IS_DRYING) != 0u;

    let left_markers = (select(0u, 1u, is_swept) + select(0u, 1u, show_uncertainty));
    let right_markers = select(0u, 1u, is_drying);
    let decimal_cells = select(0u, 1u, has_decimal);
    let decimal_ratio = 0.6;
    let normal_cells = left_markers + digit_count + right_markers;
    let total_weight = f32(normal_cells) + f32(decimal_cells) * decimal_ratio;
    let normal_w = select(1.0, 1.0 / total_weight, total_weight > 0.0);
    let decimal_w = normal_w * decimal_ratio;

    // Walk cells to find which one uv.x falls in
    var accum = 0.0;
    var cell_index = 0u;
    var current_w = normal_w;
    let effective_count = normal_cells + decimal_cells;
    for (var i = 0u; i < effective_count; i++) {
        let is_dec = has_decimal && (i == left_markers + digit_count);
        let w = select(normal_w, decimal_w, is_dec);
        if (uv.x < accum + w || i == effective_count - 1u) {
            cell_index = i;
            current_w = w;
            break;
        }
        accum += w;
    }
    let cell_uv = vec2<f32>((uv.x - accum) / current_w, uv.y);

    var intensity = 0.0;

    if (cell_index < left_markers) {
        if (is_swept && cell_index == 0u) {
            intensity = render_swept(cell_uv);
        } else {
            intensity = render_question_mark(cell_uv);
        }
    } else if (cell_index < left_markers + digit_count) {
        let digit_index = cell_index - left_markers;
        let digit = digit_for_index(whole, digit_count, digit_index);
        intensity = render_digit(cell_uv, digit);
    } else if (has_decimal && cell_index == left_markers + digit_count) {
        intensity = render_subscript_digit(cell_uv, i32(decimal_digit));
    } else if (is_drying && cell_index == effective_count - 1u) {
        intensity = render_underscore(cell_uv);
    }

    // Discard if nothing to draw
    if (intensity < 0.5) {
        discard;
    }

    return palette[in.color_index];
}
