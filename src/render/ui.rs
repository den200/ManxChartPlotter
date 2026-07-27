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
    /// Sign in to the chart shop and list what the account owns.
    ShopSignIn { email: String, password: String },
    /// Re-read the entitlement list.
    ShopRefresh,
    /// Forget the session.
    ShopSignOut,
    /// Register this machine with the account under a name.
    ShopRegister { system_name: String },
    /// Claim a slot for a chart and ask the shop for a download.
    ///
    /// `edition` names the edition to ask for; `None` means the shop's
    /// current one, which is right for a live subscription and wrong for a
    /// lapsed one.
    ShopDownload {
        chart_id: String,
        edition: Option<String>,
    },
    /// Put a lapsed chart's download to the user before sending it.
    ShopConfirmDownload { chart_id: String },
    /// Drop the pending confirmation.
    ShopCancelDownload,
}

/// A download awaiting the user's word.
///
/// A lapsed subscription cannot have the shop's current edition, so navcore
/// asks for an older one — a decision worth showing rather than making
/// silently, because it is the user's licence and the user's money.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingDownload {
    pub chart_id: String,
    pub chart_name: String,
    /// The edition to ask for, when one could be worked out.
    pub edition: Option<String>,
    /// Where that edition came from, in words.
    pub because: String,
}

/// What the shop panel is showing.
///
/// Lives here because egui needs somewhere to keep the text a user is typing;
/// the renderer fills in the rest as the worker answers.
#[derive(Default)]
pub struct ShopView {
    pub open: bool,
    pub email: String,
    /// Kept only for as long as it takes to send. Never written to disk.
    pub password: String,
    pub status: String,
    pub busy: bool,
    pub signed_in: bool,
    pub system_name: Option<String>,
    pub systems: Vec<String>,
    pub charts: Vec<crate::shop::types::Chart>,
    /// Editions already installed, by chart id.
    pub installed: std::collections::HashMap<String, crate::shop::types::Edition>,
    /// A name being typed for this machine.
    pub new_system_name: String,
    /// What the shop said about the last download request, per chart id.
    pub grants: std::collections::HashMap<String, String>,
    /// A download the user has not yet confirmed.
    pub pending: Option<PendingDownload>,
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
    /// egui asked to be drawn again, and how soon.
    repaint_after: Option<std::time::Duration>,
    /// The chart shop panel's own state.
    pub shop: ShopView,
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
            repaint_after: None,
            shop: ShopView::default(),
        }
    }

    /// Does egui still have work to finish?
    ///
    /// It animates — a window fades in, a hover highlight grows — and it
    /// reports how soon it wants the next frame. navcore only redraws on
    /// demand, so ignoring this froze every animation part-way: a panel that
    /// had faded to a third of its opacity simply stayed there, looking like a
    /// rendering fault rather than an unfinished fade.
    pub fn wants_repaint(&self) -> bool {
        matches!(self.repaint_after, Some(d) if d < std::time::Duration::from_millis(100))
    }

    /// Offer an event to the UI.
    ///
    /// Both halves of the answer matter. `consumed` is not enough on its own: a
    /// *press* is essentially never consumed, because egui decides consumption
    /// from `wants_pointer_input`, which is false the moment a button goes
    /// down and only becomes true once a frame has run with that press. Use
    /// [`pointer_over_ui`](Self::pointer_over_ui) to gate a press, and
    /// `repaint` to keep the state that answer depends on current.
    pub fn on_window_event(
        &mut self,
        window: &winit::window::Window,
        event: &winit::event::WindowEvent,
    ) -> egui_winit::EventResponse {
        self.state.on_window_event(window, event)
    }

    /// Follow the chart's palette: a light interface over the Day chart, a dark
    /// one over Dusk and Night. A bright panel at night ruins night vision,
    /// which is the whole point of the dark palettes.
    pub fn set_dark(&mut self, dark: bool) {
        style(&self.ctx, dark);
    }

    /// Is the pointer over any part of the interface — a panel, a window, a
    /// widget?
    ///
    /// This, not `wants_pointer_input`, is the gate for "should the chart get
    /// this click". `wants_pointer_input` is false whenever a button is down,
    /// so that a drag begun on the chart keeps working as it passes over a
    /// panel — useful, and exactly wrong for deciding whether a *press* was
    /// meant for the interface. It is why clicking the menu bar also dropped a
    /// pin on the chart behind it: the bar's background is not a widget, egui
    /// did not claim the click, and it fell through.
    pub fn pointer_over_ui(&self) -> bool {
        self.ctx.is_pointer_over_area()
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
        let shop = &mut self.shop;
        let output = self.ctx.run(input, |ctx| {
            super::ui_panels::build(ctx, &state, shop, actions);
        });
        self.state
            .handle_platform_output(window, output.platform_output);
        self.repaint_after = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay);

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
