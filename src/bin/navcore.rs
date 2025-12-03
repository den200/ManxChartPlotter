//! NavCore Chart Plotter
//!
//! Usage:
//!   navcore                         - Test triangle (verify GPU)
//!   navcore <chart.oesu>            - Render a chart
//!   navcore --info <chart.oesu>     - Show chart info without rendering

use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use winit::{
    application::ApplicationHandler,
    dpi::PhysicalPosition,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Window, WindowId},
};

use navcore2::{ChartDecryptor, KeyStore};
use navcore2::render::RenderState;
use navcore2::senc::ChartData;

fn main() {
    env_logger::init();

    let args: Vec<String> = env::args().collect();

    // Handle --info mode
    if args.len() >= 3 && args[1] == "--info" {
        info_mode(&args[2]);
        return;
    }

    // Get chart path from args (if any)
    let chart_path = args.get(1).map(PathBuf::from);

    let event_loop = EventLoop::new().expect("Failed to create event loop");
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = App::new(chart_path);
    event_loop.run_app(&mut app).expect("Event loop failed");
}

/// Print chart info and exit
fn info_mode(chart_path: &str) {
    println!("Loading chart: {}", chart_path);

    // Find keys
    let chart_dir = PathBuf::from(chart_path)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let mut keys = KeyStore::new();
    if let Err(e) = keys.load_keylists_in_dir(&chart_dir) {
        eprintln!("Warning: Could not load keys from {}: {}", chart_dir.display(), e);
    }

    // Get chart name for key lookup
    let chart_name = PathBuf::from(chart_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_default();

    let install_key = match keys.lookup(&chart_name) {
        Some(k) => k.to_string(),
        None => {
            eprintln!("No install key found for {}", chart_name);
            return;
        }
    };

    // Decrypt
    let mut decryptor = match ChartDecryptor::new("license") {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Failed to create decryptor: {}", e);
            return;
        }
    };

    let senc_bytes = match decryptor.decrypt_chart(chart_path, &install_key) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Failed to decrypt chart: {}", e);
            return;
        }
    };

    // Parse
    let chart = match ChartData::parse(senc_bytes) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to parse SENC: {}", e);
            return;
        }
    };

    println!("\n{}", chart.summary());
}

struct App {
    chart_path: Option<PathBuf>,
    state: Option<RenderState>,
    chart_data: Option<ChartData>,
    // Mouse state for pan
    last_mouse_pos: PhysicalPosition<f64>,
    mouse_pressed: bool,
}

impl App {
    fn new(chart_path: Option<PathBuf>) -> Self {
        Self {
            chart_path,
            state: None,
            chart_data: None,
            last_mouse_pos: PhysicalPosition::new(0.0, 0.0),
            mouse_pressed: false,
        }
    }

    fn load_chart(&mut self) {
        let Some(ref path) = self.chart_path else {
            // No chart - load test triangle to verify GPU
            if let Some(ref mut state) = self.state {
                println!("No chart path provided - loading test triangle");
                state.load_test_triangle();
            }
            return;
        };

        println!("Loading chart: {}", path.display());

        // Find keys directory
        let chart_dir = path.parent().unwrap_or(path);
        let mut keys = KeyStore::new();

        if let Err(e) = keys.load_keylists_in_dir(chart_dir) {
            eprintln!("Warning: Could not load keys from {}: {}", chart_dir.display(), e);
        }

        // Get chart name for key lookup
        let chart_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_default();

        let install_key = match keys.lookup(&chart_name) {
            Some(k) => k.to_string(),
            None => {
                eprintln!("No install key found for {}. Keys loaded: {}", chart_name, keys.len());
                return;
            }
        };

        println!("Found key for {}", chart_name);

        // Create decryptor
        let mut decryptor = match ChartDecryptor::new("license") {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Failed to create decryptor: {}", e);
                return;
            }
        };

        // Decrypt chart
        let senc_bytes = match decryptor.decrypt_chart(path, &install_key) {
            Ok(b) => {
                println!("Decrypted {} bytes", b.len());
                b
            }
            Err(e) => {
                eprintln!("Failed to decrypt: {}", e);
                return;
            }
        };

        // Parse SENC
        let chart = match ChartData::parse(senc_bytes) {
            Ok(c) => {
                println!("{}", c.summary());
                c
            }
            Err(e) => {
                eprintln!("Failed to parse SENC: {}", e);
                return;
            }
        };

        self.chart_data = Some(chart);

        // Load into renderer if available
        if let (Some(ref mut state), Some(ref chart)) = (&mut self.state, &self.chart_data) {
            state.load_chart(chart);
        }
    }
}

impl ApplicationHandler for App {
    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Critical for winit 0.30: drives the render loop
        if let Some(ref state) = self.state {
            state.window().request_redraw();
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_none() {
            let title = if self.chart_path.is_some() {
                "NavCore - Chart Viewer"
            } else {
                "NavCore - Test Triangle (pass chart path to render)"
            };

            let window = Arc::new(
                event_loop
                    .create_window(
                        Window::default_attributes()
                            .with_title(title)
                            .with_inner_size(winit::dpi::LogicalSize::new(1024, 768)),
                    )
                    .expect("Failed to create window"),
            );

            let state = pollster::block_on(RenderState::new(window));
            self.state = Some(state);

            // Load chart after renderer is ready
            self.load_chart();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = &mut self.state else { return };

        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }

            WindowEvent::Resized(physical_size) => {
                state.resize(physical_size);
            }

            WindowEvent::RedrawRequested => {
                match state.render() {
                    Ok(_) => {}
                    Err(wgpu::SurfaceError::Lost) => state.resize(state.size),
                    Err(wgpu::SurfaceError::OutOfMemory) => event_loop.exit(),
                    Err(e) => eprintln!("Render error: {:?}", e),
                }
                state.window().request_redraw();
            }

            WindowEvent::MouseInput { state: btn_state, button, .. } => {
                if button == MouseButton::Left {
                    self.mouse_pressed = btn_state == ElementState::Pressed;
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                if self.mouse_pressed {
                    let dx = (position.x - self.last_mouse_pos.x) as f32;
                    let dy = (position.y - self.last_mouse_pos.y) as f32;
                    state.camera.pan(dx, dy);
                }
                self.last_mouse_pos = position;
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let scroll = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(pos) => pos.y as f32 / 50.0,
                };
                state.camera.zoom_by_wheel(
                    scroll,
                    self.last_mouse_pos.x as f32,
                    self.last_mouse_pos.y as f32,
                );
            }

            _ => {}
        }
    }
}
