//! WGPU render state and chart rendering pipeline.

use std::sync::Arc;
use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;
use winit::window::Window;

use super::camera::Camera;
use super::colors::{depth_color, Color, LAND_COLOR, OCEAN_BACKGROUND};
use crate::senc::{ChartData, Feature};

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

/// Uniform buffer for view-projection matrix
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct Uniforms {
    pub view_proj: [[f32; 4]; 4],
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
        }
    }

    /// Load chart data and create vertex buffers
    pub fn load_chart(&mut self, chart: &ChartData) {
        let mut vertices = Vec::new();

        // Debug: show feature classification for first few areas
        for (i, feature) in chart.areas().take(5).enumerate() {
            println!("DEBUG: Area[{}] type_code={} object_class={:?} is_land={} is_depth={}",
                i, feature.type_code, feature.object_class, feature.is_land(), feature.is_depth_area());
        }

        // Process all area features
        // SM coordinates are already relative to chart center - use directly
        for feature in chart.areas() {
            let color = feature_color(feature);

            if let Some(ref geom) = feature.area_geometry {
                // SM coordinates used directly - no conversion needed
                let tri_verts = geom.to_vertices();
                for pos in tri_verts {
                    vertices.push(Vertex {
                        position: pos,
                        color,
                    });
                }
            }
        }

        // Debug: Check which recognized features have geometry
        let land_with_geom = chart.land_areas().filter(|f| f.area_geometry.is_some()).count();
        let depth_with_geom = chart.depth_areas().filter(|f| f.area_geometry.is_some()).count();
        println!("DEBUG: Land areas with geometry: {}, Depth areas with geometry: {}", land_with_geom, depth_with_geom);

        println!("Loaded {} vertices from chart", vertices.len());

        if vertices.is_empty() {
            return;
        }

        // Debug: show vertex bounds to verify coordinate transformation
        let min_x = vertices.iter().map(|v| v.position[0]).fold(f32::MAX, f32::min);
        let max_x = vertices.iter().map(|v| v.position[0]).fold(f32::MIN, f32::max);
        let min_y = vertices.iter().map(|v| v.position[1]).fold(f32::MAX, f32::min);
        let max_y = vertices.iter().map(|v| v.position[1]).fold(f32::MIN, f32::max);
        println!("DEBUG: Vertex bounds: X=[{:.0}, {:.0}], Y=[{:.0}, {:.0}]", min_x, max_x, min_y, max_y);
        println!("DEBUG: First 3 vertices: {:?}", &vertices[..3.min(vertices.len())]);

        // Create vertex buffer
        self.vertex_buffer = Some(self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Chart Vertex Buffer"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        }));
        self.vertex_count = vertices.len() as u32;

        // Update camera to fit chart
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

            render_pass.set_pipeline(&self.pipeline);
            render_pass.set_bind_group(0, &self.uniform_bind_group, &[]);

            if let Some(ref buffer) = self.vertex_buffer {
                render_pass.set_vertex_buffer(0, buffer.slice(..));
                render_pass.draw(0..self.vertex_count, 0..1);
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
