//! egui integration.
//!
//! Deliberately shallow. egui owns no window, no event loop and no part of the
//! chart pipeline: it draws in its own pass over the already-resolved surface,
//! after the chart is finished with it. That keeps the two independent — the
//! chart pass keeps its multisampling and its depth buffer, egui needs neither
//! — and it means a mistake in the UI cannot change a pixel of the chart.
//!
//! Everything the UI needs to say about the world goes through [`UiState`], and
//! everything it wants done comes back as [`UiAction`]. The chart renderer
//! never calls egui and egui never calls the chart renderer.

use egui_wgpu::ScreenDescriptor;

/// What the UI asks the application to do.
///
/// Returned rather than executed so the UI stays a pure function of state:
/// nothing in here borrows the renderer, so a panel cannot accidentally reach
/// into the camera or the tile cache.
#[derive(Debug, Clone, PartialEq)]
pub enum UiAction {
    /// Close the object-query bubble.
    DismissPick,
}

/// What the UI is allowed to see.
pub struct UiState<'a> {
    /// Objects under the last tap, if the bubble is open.
    pub picked: Option<&'a [crate::pick::PickedObject]>,
    /// Where the picked position currently is on screen, in *logical* points.
    pub pick_anchor: Option<[f32; 2]>,
}

pub struct Ui {
    ctx: egui::Context,
    state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
    /// Collected during a frame, drained by the caller.
    actions: Vec<UiAction>,
}

impl Ui {
    pub fn new(
        device: &wgpu::Device,
        window: &winit::window::Window,
        surface_format: wgpu::TextureFormat,
    ) -> Self {
        let ctx = egui::Context::default();
        style(&ctx, false);
        let state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        // No depth and no multisampling: this pass renders over the resolved
        // image, and egui antialiases its own geometry.
        let renderer = egui_wgpu::Renderer::new(device, surface_format, None, 1, false);
        Self {
            ctx,
            state,
            renderer,
            actions: Vec::new(),
        }
    }

    /// Offer an event to the UI. `true` means the UI consumed it and the chart
    /// must not also act on it — otherwise a drag on a panel would pan the map
    /// underneath.
    pub fn on_window_event(
        &mut self,
        window: &winit::window::Window,
        event: &winit::event::WindowEvent,
    ) -> bool {
        self.state.on_window_event(window, event).consumed
    }

    /// Follow the chart's palette: a light interface over the Day chart, a dark
    /// one over Dusk and Night. A bright panel at night ruins night vision,
    /// which is the whole point of the dark palettes.
    pub fn set_dark(&mut self, dark: bool) {
        style(&self.ctx, dark);
    }

    /// Does the pointer currently sit over a panel?
    pub fn wants_pointer(&self) -> bool {
        self.ctx.wants_pointer_input()
    }

    /// Build a frame, draw it over `view`, and return what the user asked for.
    #[allow(clippy::too_many_arguments)]
    pub fn run(
        &mut self,
        window: &winit::window::Window,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        size: [u32; 2],
        state: UiState<'_>,
    ) -> Vec<UiAction> {
        self.actions.clear();
        let input = self.state.take_egui_input(window);
        let actions = &mut self.actions;
        let output = self.ctx.run(input, |ctx| {
            super::ui_panels::build(ctx, &state, actions);
        });
        self.state
            .handle_platform_output(window, output.platform_output);

        let jobs = self
            .ctx
            .tessellate(output.shapes, output.pixels_per_point);
        for (id, delta) in &output.textures_delta.set {
            self.renderer.update_texture(device, queue, *id, delta);
        }
        let desc = ScreenDescriptor {
            size_in_pixels: size,
            pixels_per_point: output.pixels_per_point,
        };
        self.renderer
            .update_buffers(device, queue, encoder, &jobs, &desc);

        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Load, not Clear: the chart is already there.
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            self.renderer
                .render(&mut pass.forget_lifetime(), &jobs, &desc);
        }

        for id in &output.textures_delta.free {
            self.renderer.free_texture(id);
        }
        std::mem::take(&mut self.actions)
    }
}

/// navcore's look: bigger than egui's default, because this is read at arm's
/// length on a boat, often through spray and often through reading glasses.
fn style(ctx: &egui::Context, dark: bool) {
    use egui::{FontFamily, FontId, TextStyle};
    ctx.set_visuals(if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    });
    let mut style = (*ctx.style()).clone();
    // Points, so these track the display's scale factor.
    style.text_styles = [
        (TextStyle::Heading, FontId::new(15.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(13.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(12.0, FontFamily::Monospace)),
        (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(11.0, FontFamily::Proportional)),
    ]
    .into();
    // Touch first: a finger is about 9 mm, so controls get room. These are the
    // numbers to revisit if the UI ever feels cramped on the plotter.
    style.spacing.button_padding = egui::vec2(10.0, 8.0);
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.interact_size.y = 32.0;
    style.visuals.window_rounding = egui::Rounding::same(6.0);
    ctx.set_style(style);
}
