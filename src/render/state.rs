//! WGPU render state and chart rendering pipeline.

use std::sync::Arc;
use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;
use winit::window::Window;

use super::camera::Camera;
use super::colors::{
    depth_color, Color, COASTLINE_COLOR,
    LAND_COLOR, OCEAN_BACKGROUND,
};
use super::s52_styles::{COASTLINE_STYLE, CONTOUR_STYLE};
use super::symbols::SymbolRenderer;
use crate::senc::{ChartData, Feature, features_to_instances};

/// Vertex with position and color
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct Vertex {
    pub position: [f32; 2],
    pub color: [f32; 4],
}

impl Vertex {
    const ATTRIBS: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![
        0 => Float32x2,
        1 => Float32x4,
    ];

    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBS,
        }
    }
}

/// Uniform buffer for view-projection matrix and camera info
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct Uniforms {
    pub view_proj: [[f32; 4]; 4],
    /// View size in pixels [width, height]
    pub view_size: [f32; 2],
    /// Pixels per meter at current zoom
    pub pixels_per_meter: f32,
    /// Padding for alignment
    pub _pad: f32,
}

/// Line vertex with adjacency data for shader-based polyline rendering.
/// 32 bytes per vertex (3×vec2 + 2×f32).
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct LineVertex {
    /// Previous point in polyline (SM meters)
    pub prev: [f32; 2],
    /// Current point in polyline (SM meters)
    pub curr: [f32; 2],
    /// Next point in polyline (SM meters)
    pub next: [f32; 2],
    /// Side: +1.0 or -1.0 for left/right expansion
    pub side: f32,
    /// Cumulative arc length from start of polyline (SM meters)
    pub arc_len: f32,
}

impl LineVertex {
    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<LineVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                // prev: [f32; 2] at offset 0
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // curr: [f32; 2] at offset 8
                wgpu::VertexAttribute {
                    offset: 8,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // next: [f32; 2] at offset 16
                wgpu::VertexAttribute {
                    offset: 16,
                    shader_location: 2,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // side: f32 at offset 24
                wgpu::VertexAttribute {
                    offset: 24,
                    shader_location: 3,
                    format: wgpu::VertexFormat::Float32,
                },
                // arc_len: f32 at offset 28
                wgpu::VertexAttribute {
                    offset: 28,
                    shader_location: 4,
                    format: wgpu::VertexFormat::Float32,
                },
            ],
        }
    }
}

/// Uniforms for line shader (112 bytes).
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct LineUniforms {
    /// View-projection matrix (SM -> clip space)
    pub view_proj: [[f32; 4]; 4],
    /// Viewport size in pixels [width, height]
    pub viewport_size: [f32; 2],
    /// Line width in screen pixels
    pub line_width_px: f32,
    /// Miter limit (bevel fallback when exceeded)
    pub join_limit: f32,
    /// Line color RGBA
    pub color: [f32; 4],
    /// Dash on length in screen pixels (0.0 = solid)
    pub dash_on_px: f32,
    /// Dash off (gap) length in screen pixels (0.0 = solid)
    pub dash_off_px: f32,
    /// Pixels per meter at current zoom (for arc_len conversion)
    pub px_per_meter: f32,
    /// Padding for 16-byte alignment
    pub _pad: f32,
}

/// Line style configuration for a batch of lines.
#[derive(Clone, Copy, Debug)]
pub struct LineStyle {
    /// Line color RGBA
    pub color: Color,
    /// Line width in screen pixels
    pub width_px: f32,
    /// Dash on length in screen pixels (0.0 = solid)
    pub dash_on_px: f32,
    /// Dash off (gap) length in screen pixels (0.0 = solid)
    pub dash_off_px: f32,
}

impl LineStyle {
    /// Create a solid line style
    pub const fn solid(color: Color, width_px: f32) -> Self {
        Self {
            color,
            width_px,
            dash_on_px: 0.0,
            dash_off_px: 0.0,
        }
    }

    /// Create a dashed line style with 6px dash, 6px gap
    pub const fn dashed(color: Color, width_px: f32) -> Self {
        Self {
            color,
            width_px,
            dash_on_px: 6.0,
            dash_off_px: 6.0,
        }
    }
}

/// A batch of lines with shared style and vertex buffer.
pub struct LineBatch {
    /// Style configuration for this batch
    pub style: LineStyle,
    /// GPU vertex buffer
    pub vertex_buffer: wgpu::Buffer,
    /// Number of vertices in the buffer
    pub vertex_count: u32,
}

/// Complete render state for chart visualization
pub struct RenderState {
    pub window: Arc<Window>,
    pub surface: wgpu::Surface<'static>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub config: wgpu::SurfaceConfiguration,
    pub size: winit::dpi::PhysicalSize<u32>,

    // Pipeline
    pub pipeline: wgpu::RenderPipeline,
    pub uniform_buffer: wgpu::Buffer,
    pub uniform_bind_group: wgpu::BindGroup,

    // Geometry
    pub vertex_buffer: Option<wgpu::Buffer>,
    pub vertex_count: u32,

    // Camera
    pub camera: Camera,

    // Symbol rendering
    pub symbol_renderer: Option<SymbolRenderer>,
    pub symbol_camera_bind_group: Option<wgpu::BindGroup>,

    // Line rendering (shader-based polylines with per-batch styling)
    pub line_pipeline: wgpu::RenderPipeline,
    pub line_bind_group_layout: wgpu::BindGroupLayout,
    pub line_uniform_buffer: wgpu::Buffer,
    pub line_bind_group: wgpu::BindGroup,
    /// Line batches with different styles (coastlines, contours, etc.)
    pub line_batches: Vec<LineBatch>,
}

impl RenderState {
    pub async fn new(window: Arc<Window>) -> Self {
        let size = window.inner_size();

        // WGPU setup
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        let surface = instance.create_surface(window.clone()).expect("Failed to create surface");

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .expect("Failed to find adapter");

        println!("DEBUG: Using GPU: {}", adapter.get_info().name);

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::default(),
                    label: Some("navcore_device"),
                    memory_hints: Default::default(),
                },
                None,
            )
            .await
            .expect("Failed to create device");

        let surface_caps = surface.get_capabilities(&adapter);
        let surface_format = surface_caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(surface_caps.formats[0]);

        println!("DEBUG: Surface format: {:?}", surface_format);
        println!("DEBUG: Available formats: {:?}", surface_caps.formats);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width,
            height: size.height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        // Shader
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Chart Shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../../assets/shaders/chart.wgsl").into()),
        });

        // Uniforms
        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Uniform Buffer"),
            contents: bytemuck::cast_slice(&[Uniforms {
                view_proj: glam::Mat4::IDENTITY.to_cols_array_2d(),
                view_size: [size.width as f32, size.height as f32],
                pixels_per_meter: 1.0,
                _pad: 0.0,
            }]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let uniform_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
                label: Some("uniform_bind_group_layout"),
            });

        let uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &uniform_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
            label: Some("uniform_bind_group"),
        });

        // Pipeline
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Pipeline Layout"),
            bind_group_layouts: &[&uniform_bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Chart Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[Vertex::desc()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None, // No culling for 2D
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: 1,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview: None,
            cache: None,
        });

        // Default camera (will be updated when chart is loaded)
        let camera = Camera::new(0.0, 0.0, 1000.0, size.width as f32, size.height as f32);

        // Line rendering pipeline (shader-based polylines with screen-space width)
        let line_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Line Shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../../assets/shaders/line.wgsl").into()),
        });

        let line_bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("line_bind_group_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let line_uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Line Uniform Buffer"),
            contents: bytemuck::cast_slice(&[LineUniforms {
                view_proj: glam::Mat4::IDENTITY.to_cols_array_2d(),
                viewport_size: [size.width as f32, size.height as f32],
                line_width_px: 2.0,
                join_limit: 4.0,
                color: COASTLINE_COLOR,
                dash_on_px: 0.0,
                dash_off_px: 0.0,
                px_per_meter: 1.0,
                _pad: 0.0,
            }]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let line_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("line_bind_group"),
            layout: &line_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: line_uniform_buffer.as_entire_binding(),
            }],
        });

        let line_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Line Pipeline Layout"),
            bind_group_layouts: &[&line_bind_group_layout],
            push_constant_ranges: &[],
        });

        let line_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Line Pipeline"),
            layout: Some(&line_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &line_shader,
                entry_point: "vs_main",
                buffers: &[LineVertex::desc()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &line_shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        // Symbol renderer (optional - may fail if assets not found)
        let (symbol_renderer, symbol_camera_bind_group) = match SymbolRenderer::new(
            &device,
            &queue,
            surface_format,
            &uniform_bind_group_layout,
        ) {
            Ok(renderer) => {
                let bind_group = renderer.create_camera_bind_group(&device, &uniform_buffer);
                println!("DEBUG: Symbol renderer initialized");
                (Some(renderer), Some(bind_group))
            }
            Err(e) => {
                eprintln!("WARNING: Failed to initialize symbol renderer: {}", e);
                (None, None)
            }
        };

        Self {
            window,
            surface,
            device,
            queue,
            config,
            size,
            pipeline,
            uniform_buffer,
            uniform_bind_group,
            vertex_buffer: None,
            vertex_count: 0,
            camera,
            symbol_renderer,
            symbol_camera_bind_group,
            line_pipeline,
            line_bind_group_layout,
            line_uniform_buffer,
            line_bind_group,
            line_batches: Vec::new(),
        }
    }

    /// Load chart data and create vertex buffers
    pub fn load_chart(&mut self, chart: &ChartData) {
        let mut vertices = Vec::new();

        // Debug: show ALL area feature type codes to identify diagonal lines
        use std::collections::HashMap;
        let mut area_type_counts: HashMap<u16, usize> = HashMap::new();
        for feature in chart.areas() {
            *area_type_counts.entry(feature.type_code).or_insert(0) += 1;
        }
        println!("DEBUG: Area feature type breakdown:");
        let mut sorted_types: Vec<_> = area_type_counts.iter().collect();
        sorted_types.sort_by_key(|(code, _)| *code);
        for (type_code, count) in sorted_types {
            println!("  type_code={}: {} features", type_code, count);
        }

        // Process area features - only land and depth areas
        // Skip meta features (M_COVR, M_QUAL, etc.) and unknown types
        // SM coordinates are already relative to chart center - use directly
        let mut rendered_area_types: HashMap<u16, usize> = HashMap::new();
        let mut total_plain_triangles = 0usize;
        let mut total_strip_triangles = 0usize;
        let mut total_fan_triangles = 0usize;
        let mut total_skinny_triangles = 0usize;
        let mut total_degenerate_triangles = 0usize;

        for feature in chart.areas() {
            // Skip meta features (type codes 300+) - these define metadata, not chart content
            if feature.type_code >= 300 {
                continue;
            }

            // Only render features we have colors for (land and depth areas)
            // Unknown features would get transparent color and waste vertices
            if !feature.is_land() && !feature.is_depth_area() {
                continue;
            }

            let color = feature_color(feature);

            if let Some(ref geom) = feature.area_geometry {
                *rendered_area_types.entry(feature.type_code).or_insert(0) += 1;

                // Track primitive statistics
                let (plain, strip, fan) = geom.primitive_stats();
                total_plain_triangles += plain;
                total_strip_triangles += strip;
                total_fan_triangles += fan;

                // Analyze triangle quality for this feature
                let (_total, skinny, degenerate) = geom.analyze_triangle_quality();
                total_skinny_triangles += skinny;
                total_degenerate_triangles += degenerate;

                // SM coordinates used directly - no conversion needed
                let tri_verts = geom.to_vertices();

                // Just push all vertices - large triangles are valid for chart-spanning features
                for pos in tri_verts {
                    vertices.push(Vertex {
                        position: pos,
                        color,
                    });
                }
            }
        }
        println!("DEBUG: Actually rendered area types: {:?}", rendered_area_types);
        println!("DEBUG: Primitive stats - plain: {}, strip: {}, fan: {} triangles",
            total_plain_triangles, total_strip_triangles, total_fan_triangles);
        if total_skinny_triangles > 0 || total_degenerate_triangles > 0 {
            println!("DEBUG: Triangle quality - skinny: {}, degenerate: {}",
                total_skinny_triangles, total_degenerate_triangles);
        }

        // Debug: Check which recognized features have geometry
        let land_with_geom = chart.land_areas().filter(|f| f.area_geometry.is_some()).count();
        let depth_with_geom = chart.depth_areas().filter(|f| f.area_geometry.is_some()).count();
        println!("DEBUG: Land areas with geometry: {}, Depth areas with geometry: {}", land_with_geom, depth_with_geom);

        let area_vertex_count = vertices.len();

        // Debug: show ALL line feature type codes
        let mut line_type_counts: HashMap<u16, usize> = HashMap::new();
        for feature in chart.lines() {
            *line_type_counts.entry(feature.type_code).or_insert(0) += 1;
        }
        println!("DEBUG: Line feature type breakdown:");
        let mut sorted_line_types: Vec<_> = line_type_counts.iter().collect();
        sorted_line_types.sort_by_key(|(code, _)| *code);
        for (type_code, count) in sorted_line_types {
            println!("  type_code={}: {} features", type_code, count);
        }

        // Process line features using new shader-based polyline rendering
        // Collect all polylines for coastlines and contours
        let mut coastline_polylines: Vec<Vec<[f32; 2]>> = Vec::new();
        for feature in chart.coastlines() {
            if let Some(ref geom) = feature.line_geometry {
                let segments = geom.resolve(&chart.edge_table);
                for segment in segments {
                    if segment.len() >= 2 {
                        coastline_polylines.push(segment);
                    }
                }
            }
        }

        let mut contour_polylines: Vec<Vec<[f32; 2]>> = Vec::new();
        for feature in chart.depth_contours() {
            if let Some(ref geom) = feature.line_geometry {
                let segments = geom.resolve(&chart.edge_table);
                for segment in segments {
                    if segment.len() >= 2 {
                        contour_polylines.push(segment);
                    }
                }
            }
        }

        println!("DEBUG: Lines: coastlines={} polylines, contours={} polylines",
            coastline_polylines.len(), contour_polylines.len());

        // Calculate bounds FIRST (before polylines are consumed)
        let mut min_x = f32::MAX;
        let mut max_x = f32::MIN;
        let mut min_y = f32::MAX;
        let mut max_y = f32::MIN;

        // Include area vertex bounds
        for v in &vertices {
            min_x = min_x.min(v.position[0]);
            max_x = max_x.max(v.position[0]);
            min_y = min_y.min(v.position[1]);
            max_y = max_y.max(v.position[1]);
        }

        // Include line bounds from polylines
        for polyline in coastline_polylines.iter().chain(contour_polylines.iter()) {
            for point in polyline {
                min_x = min_x.min(point[0]);
                max_x = max_x.max(point[0]);
                min_y = min_y.min(point[1]);
                max_y = max_y.max(point[1]);
            }
        }

        // Clear existing line batches
        self.line_batches.clear();

        // Build coastline batch (solid, 2px, dark brown)
        if !coastline_polylines.is_empty() {
            let coastline_verts = build_line_vertices_multi(&coastline_polylines);
            if !coastline_verts.is_empty() {
                self.line_batches.push(LineBatch {
                    style: COASTLINE_STYLE,
                    vertex_buffer: self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("Coastline Vertex Buffer"),
                        contents: bytemuck::cast_slice(&coastline_verts),
                        usage: wgpu::BufferUsages::VERTEX,
                    }),
                    vertex_count: coastline_verts.len() as u32,
                });
                println!("DEBUG: Coastline batch: {} vertices", coastline_verts.len());
            }
        }

        // Build contour batch (dashed, 1px, gray-blue)
        if !contour_polylines.is_empty() {
            let contour_verts = build_line_vertices_multi(&contour_polylines);
            if !contour_verts.is_empty() {
                self.line_batches.push(LineBatch {
                    style: CONTOUR_STYLE,
                    vertex_buffer: self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("Contour Vertex Buffer"),
                        contents: bytemuck::cast_slice(&contour_verts),
                        usage: wgpu::BufferUsages::VERTEX,
                    }),
                    vertex_count: contour_verts.len() as u32,
                });
                println!("DEBUG: Contour batch: {} vertices", contour_verts.len());
            }
        }

        // Total line vertex count for logging
        let total_line_verts: u32 = self.line_batches.iter().map(|b| b.vertex_count).sum();
        println!("DEBUG: Loaded {} area vertices, {} line vertices in {} batches",
            area_vertex_count, total_line_verts, self.line_batches.len());

        println!("Loaded {} total area vertices from chart", vertices.len());

        // Create vertex buffer for areas (if any)
        if !vertices.is_empty() {
            self.vertex_buffer = Some(self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("Chart Vertex Buffer"),
                contents: bytemuck::cast_slice(&vertices),
                usage: wgpu::BufferUsages::VERTEX,
            }));
            self.vertex_count = vertices.len() as u32;
        }

        // Check if we have any geometry at all
        if min_x == f32::MAX {
            println!("DEBUG: No geometry to render");
            return;
        }

        println!("DEBUG: Combined bounds: X=[{:.0}, {:.0}], Y=[{:.0}, {:.0}]", min_x, max_x, min_y, max_y);
        if !vertices.is_empty() {
            println!("DEBUG: First 3 area vertices: {:?}", &vertices[..3.min(vertices.len())]);
        }

        // Update camera to fit all geometry (areas + lines)
        // SM coordinates are centered at (0,0) - camera at origin
        // Zoom = meters per pixel (larger = more zoomed out)
        let width = max_x - min_x;
        let height = max_y - min_y;
        let screen_w = self.size.width as f32;
        let screen_h = self.size.height as f32;
        let zoom = if width > 0.0 && height > 0.0 {
            // Fit chart in view with 10% padding
            let zoom_x = (width * 1.1) / screen_w;   // meters per pixel
            let zoom_y = (height * 1.1) / screen_h;  // meters per pixel
            zoom_x.max(zoom_y)  // Use larger to fit both dimensions
        } else {
            1.0
        };

        // Center camera on actual geometry bounds (not assumed 0,0)
        let center_x = (min_x + max_x) / 2.0;
        let center_y = (min_y + max_y) / 2.0;
        self.camera = Camera::new(center_x, center_y, zoom, screen_w, screen_h);
        println!("DEBUG: Camera at ({:.0}, {:.0}), zoom: {:.2} m/px (SM bounds: {:.0}x{:.0})", center_x, center_y, zoom, width, height);

        // Log view_proj matrix to verify it's sensible
        let vp = self.camera.view_projection_matrix();
        println!("DEBUG: View-proj matrix:\n  {:?}\n  {:?}\n  {:?}\n  {:?}",
            vp.row(0), vp.row(1), vp.row(2), vp.row(3));

        // Update symbol instances
        if let Some(ref mut symbol_renderer) = self.symbol_renderer {
            let ref_lat = chart.header.ref_lat;
            let ref_lon = chart.header.ref_lon;
            let instances = features_to_instances(&chart.features, ref_lat, ref_lon);
            println!("DEBUG: Created {} symbol instances", instances.len());
            symbol_renderer.update_instances(&self.queue, &instances);
        }
    }

    /// Load a test triangle to verify the rendering pipeline works.
    /// Uses NDC coordinates (-1 to 1) with identity-like matrix.
    pub fn load_test_triangle(&mut self) {
        println!("DEBUG: Loading test triangle to verify pipeline...");

        // Simple triangle in NDC space
        let vertices = vec![
            Vertex { position: [0.0, 0.5], color: [1.0, 0.0, 0.0, 1.0] },   // Top - RED
            Vertex { position: [-0.5, -0.5], color: [0.0, 1.0, 0.0, 1.0] }, // Bottom-left - GREEN
            Vertex { position: [0.5, -0.5], color: [0.0, 0.0, 1.0, 1.0] },  // Bottom-right - BLUE
        ];

        // Create vertex buffer
        self.vertex_buffer = Some(self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Test Triangle Buffer"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        }));
        self.vertex_count = vertices.len() as u32;

        // Set camera to identity-like: maps -1..1 to -1..1
        // For ortho_rh(-half_w, half_w, -half_h, half_h) to map to NDC:
        // half_w = 1, half_h = 1
        // So: viewport_width * zoom / 2.0 = 1 → zoom = 2.0 / viewport_width
        let zoom = 2.0 / self.size.width as f32;
        self.camera = Camera::new(0.0, 0.0, zoom, self.size.width as f32, self.size.height as f32);

        let vp = self.camera.view_projection_matrix();
        println!("DEBUG: Test triangle view_proj matrix:");
        println!("  {:?}", vp.row(0));
        println!("  {:?}", vp.row(1));
        println!("  {:?}", vp.row(2));
        println!("  {:?}", vp.row(3));
    }

    pub fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        if new_size.width > 0 && new_size.height > 0 {
            self.size = new_size;
            self.config.width = new_size.width;
            self.config.height = new_size.height;
            self.surface.configure(&self.device, &self.config);
            self.camera.resize(new_size.width as f32, new_size.height as f32);
        }
    }

    pub fn render(&mut self) -> Result<(), wgpu::SurfaceError> {
        // Update uniforms
        let uniforms = Uniforms {
            view_proj: self.camera.view_projection_matrix().to_cols_array_2d(),
            view_size: [self.size.width as f32, self.size.height as f32],
            pixels_per_meter: 1.0 / self.camera.zoom, // Convert zoom (m/px) to px/m
            _pad: 0.0,
        };
        self.queue.write_buffer(&self.uniform_buffer, 0, bytemuck::cast_slice(&[uniforms]));

        let output = self.surface.get_current_texture()?;
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Render Encoder"),
        });

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Render Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: OCEAN_BACKGROUND[0] as f64,
                            g: OCEAN_BACKGROUND[1] as f64,
                            b: OCEAN_BACKGROUND[2] as f64,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            // 1. Render areas (land, depth)
            render_pass.set_pipeline(&self.pipeline);
            render_pass.set_bind_group(0, &self.uniform_bind_group, &[]);

            if let Some(ref buffer) = self.vertex_buffer {
                render_pass.set_vertex_buffer(0, buffer.slice(..));
                render_pass.draw(0..self.vertex_count, 0..1);
            }

            // 2. Render lines (coastlines + depth contours) with shader-based pipeline
            // Iterate over batches, each with its own style
            for batch in &self.line_batches {
                // px_per_meter: at zoom=1 m/px, 1 meter = 1 pixel
                // zoom is meters-per-pixel, so px/m = 1/zoom
                let px_per_meter = 1.0 / self.camera.zoom;

                // Update line uniforms with batch-specific styling
                let line_uniforms = LineUniforms {
                    view_proj: self.camera.view_projection_matrix().to_cols_array_2d(),
                    viewport_size: [self.size.width as f32, self.size.height as f32],
                    line_width_px: batch.style.width_px,
                    join_limit: 4.0,
                    color: batch.style.color,
                    dash_on_px: batch.style.dash_on_px,
                    dash_off_px: batch.style.dash_off_px,
                    px_per_meter,
                    _pad: 0.0,
                };
                self.queue.write_buffer(&self.line_uniform_buffer, 0, bytemuck::cast_slice(&[line_uniforms]));

                render_pass.set_pipeline(&self.line_pipeline);
                render_pass.set_bind_group(0, &self.line_bind_group, &[]);
                render_pass.set_vertex_buffer(0, batch.vertex_buffer.slice(..));
                render_pass.draw(0..batch.vertex_count, 0..1);
            }

            // 3. Render symbols on top
            if let (Some(ref symbol_renderer), Some(ref symbol_bind_group)) =
                (&self.symbol_renderer, &self.symbol_camera_bind_group)
            {
                symbol_renderer.render(&mut render_pass, symbol_bind_group);
            }
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        output.present();

        Ok(())
    }

    /// Get window reference for redraw requests
    pub fn window(&self) -> &Window {
        &self.window
    }
}

/// Determine color for a feature
fn feature_color(feature: &Feature) -> Color {
    if feature.is_land() {
        LAND_COLOR
    } else if feature.is_depth_area() {
        depth_color(feature.drval1(), feature.drval2())
    } else {
        // Unknown areas - use transparent (skip rendering)
        // or ocean background to not obscure land/depth
        [0.0, 0.0, 0.0, 0.0]  // Fully transparent
    }
}

/// Build line vertices with adjacency data and arc-length for shader-based rendering.
///
/// Each point in the polyline produces 2 vertices (one for each side: +1.0, -1.0).
/// At endpoints, prev/next are duplicated to handle caps correctly.
/// Arc length is cumulative distance from the start of the polyline (in meters).
/// Uses TriangleStrip topology.
fn build_line_vertices(points: &[[f32; 2]]) -> Vec<LineVertex> {
    let n = points.len();
    if n < 2 {
        return vec![];
    }

    let mut vertices = Vec::with_capacity(n * 2);
    let mut arc_len: f32 = 0.0;

    for i in 0..n {
        // Accumulate arc length from previous point
        if i > 0 {
            let dx = points[i][0] - points[i - 1][0];
            let dy = points[i][1] - points[i - 1][1];
            arc_len += (dx * dx + dy * dy).sqrt();
        }

        let prev = if i == 0 { points[0] } else { points[i - 1] };
        let curr = points[i];
        let next = if i == n - 1 { points[n - 1] } else { points[i + 1] };

        // Emit +side vertex
        vertices.push(LineVertex {
            prev,
            curr,
            next,
            side: 1.0,
            arc_len,
        });
        // Emit -side vertex
        vertices.push(LineVertex {
            prev,
            curr,
            next,
            side: -1.0,
            arc_len,
        });
    }
    vertices
}

/// Build line vertices for multiple polylines, inserting degenerate vertices between them.
///
/// Degenerate vertices create zero-area triangles that effectively "break" the strip
/// between separate polylines without requiring multiple draw calls.
///
/// To properly break a triangle strip, we need 4 degenerate vertices:
/// - Repeat last vertex of previous strip TWICE (creates 2 degenerate triangles)
/// - Repeat first vertex of new strip TWICE (creates 2 more degenerate triangles)
/// This ensures no real triangle spans between polylines.
fn build_line_vertices_multi(polylines: &[Vec<[f32; 2]>]) -> Vec<LineVertex> {
    let total_points: usize = polylines.iter().map(|p| p.len()).sum();
    // Each point = 2 vertices, plus 4 degenerate vertices between polylines
    let estimated_capacity = total_points * 2 + polylines.len() * 4;
    let mut vertices = Vec::with_capacity(estimated_capacity);

    for points in polylines.iter() {
        let line_verts = build_line_vertices(points);
        if line_verts.is_empty() {
            continue;
        }

        // Insert degenerate vertices to break the strip (except for first polyline)
        if !vertices.is_empty() {
            // Repeat last vertex of previous strip twice
            let last = *vertices.last().unwrap();
            vertices.push(last);
            vertices.push(last);
            // Repeat first vertex of new strip twice
            vertices.push(line_verts[0]);
            vertices.push(line_verts[0]);
        }

        vertices.extend(line_verts);
    }
    vertices
}

// Old tessellate_line_with_max and tessellate_polyline_continuous functions
// have been DELETED as part of migration to shader-based polyline rendering.
// See line.wgsl and build_line_vertices() for the new implementation.
