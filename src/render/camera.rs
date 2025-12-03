//! 2D orthographic camera with pan/zoom.
//!
//! Provides view-projection matrix for rendering chart data
//! in Mercator coordinates.

use glam::{Mat4, Vec2, Vec3};

use super::projection::MercatorBounds;

/// 2D camera for chart viewing
#[derive(Debug, Clone)]
pub struct Camera {
    /// Camera center position in Mercator meters
    pub position: Vec2,
    /// Zoom level (meters per screen unit)
    pub zoom: f32,
    /// Viewport width in pixels
    pub viewport_width: f32,
    /// Viewport height in pixels
    pub viewport_height: f32,
    /// Minimum zoom (most zoomed out)
    pub min_zoom: f32,
    /// Maximum zoom (most zoomed in)
    pub max_zoom: f32,
}

impl Camera {
    /// Create camera centered on bounds
    pub fn from_bounds(bounds: &MercatorBounds, viewport_width: f32, viewport_height: f32) -> Self {
        let (cx, cy) = bounds.center();
        let width = bounds.width() as f32;
        let height = bounds.height() as f32;

        // Calculate zoom to fit bounds with some padding
        let padding = 1.1; // 10% padding
        let zoom_x = (width * padding) / viewport_width;
        let zoom_y = (height * padding) / viewport_height;
        let zoom = zoom_x.max(zoom_y);

        Self {
            position: Vec2::new(cx as f32, cy as f32),
            zoom,
            viewport_width,
            viewport_height,
            min_zoom: zoom * 0.001, // Allow zooming in 1000x
            max_zoom: zoom * 10.0,  // Allow zooming out 10x
        }
    }

    /// Create camera at specific location
    pub fn new(center_x: f32, center_y: f32, zoom: f32, width: f32, height: f32) -> Self {
        Self {
            position: Vec2::new(center_x, center_y),
            zoom,
            viewport_width: width,
            viewport_height: height,
            min_zoom: 0.1,
            max_zoom: 1_000_000.0,
        }
    }

    /// Update viewport size (e.g., on window resize)
    pub fn resize(&mut self, width: f32, height: f32) {
        self.viewport_width = width;
        self.viewport_height = height;
    }

    /// Pan camera by screen pixels
    pub fn pan(&mut self, dx: f32, dy: f32) {
        // Convert screen movement to world movement
        self.position.x -= dx * self.zoom;
        self.position.y += dy * self.zoom; // Y is inverted
    }

    /// Zoom camera by factor, centered on screen point
    ///
    /// factor > 1 = zoom in, factor < 1 = zoom out
    pub fn zoom_at(&mut self, factor: f32, screen_x: f32, screen_y: f32) {
        // Get world position under cursor before zoom
        let world_before = self.screen_to_world(screen_x, screen_y);

        // Apply zoom
        let new_zoom = (self.zoom / factor).clamp(self.min_zoom, self.max_zoom);
        self.zoom = new_zoom;

        // Get world position under cursor after zoom
        let world_after = self.screen_to_world(screen_x, screen_y);

        // Adjust position to keep cursor at same world point
        self.position.x += world_before.x - world_after.x;
        self.position.y += world_before.y - world_after.y;
    }

    /// Zoom in/out by wheel delta (positive = zoom in)
    pub fn zoom_by_wheel(&mut self, delta: f32, screen_x: f32, screen_y: f32) {
        let factor = 1.1_f32.powf(delta);
        self.zoom_at(factor, screen_x, screen_y);
    }

    /// Convert screen coordinates to world (Mercator) coordinates
    pub fn screen_to_world(&self, screen_x: f32, screen_y: f32) -> Vec2 {
        // Screen center is at self.position
        let half_w = self.viewport_width / 2.0;
        let half_h = self.viewport_height / 2.0;

        Vec2::new(
            self.position.x + (screen_x - half_w) * self.zoom,
            self.position.y + (half_h - screen_y) * self.zoom, // Y inverted
        )
    }

    /// Convert world coordinates to screen coordinates
    pub fn world_to_screen(&self, world_x: f32, world_y: f32) -> Vec2 {
        let half_w = self.viewport_width / 2.0;
        let half_h = self.viewport_height / 2.0;

        Vec2::new(
            half_w + (world_x - self.position.x) / self.zoom,
            half_h - (world_y - self.position.y) / self.zoom, // Y inverted
        )
    }

    /// Get orthographic view-projection matrix for shaders
    pub fn view_projection_matrix(&self) -> Mat4 {
        // View: translate world so camera is at origin
        let view = Mat4::from_translation(Vec3::new(-self.position.x, -self.position.y, 0.0));

        // Projection: orthographic, scaled by zoom
        let half_w = self.viewport_width * self.zoom / 2.0;
        let half_h = self.viewport_height * self.zoom / 2.0;

        let proj = Mat4::orthographic_rh(-half_w, half_w, -half_h, half_h, -1.0, 1.0);

        proj * view
    }

    /// Get visible bounds in world coordinates
    pub fn visible_bounds(&self) -> MercatorBounds {
        let half_w = self.viewport_width * self.zoom / 2.0;
        let half_h = self.viewport_height * self.zoom / 2.0;

        MercatorBounds {
            min_x: (self.position.x - half_w) as f64,
            max_x: (self.position.x + half_w) as f64,
            min_y: (self.position.y - half_h) as f64,
            max_y: (self.position.y + half_h) as f64,
        }
    }

    /// Get approximate zoom level (similar to OpenStreetMap)
    pub fn approximate_osm_zoom(&self) -> f32 {
        // At zoom 0, 256px = 2π × R meters
        // So meters_per_pixel at zoom 0 = 2π × 6378137 / 256 ≈ 156543
        let meters_per_pixel_z0 = 156543.0_f32;
        (meters_per_pixel_z0 / self.zoom).log2()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_world_roundtrip() {
        let camera = Camera::new(1000.0, 2000.0, 10.0, 800.0, 600.0);

        let screen = Vec2::new(100.0, 200.0);
        let world = camera.screen_to_world(screen.x, screen.y);
        let back = camera.world_to_screen(world.x, world.y);

        assert!((screen.x - back.x).abs() < 0.01);
        assert!((screen.y - back.y).abs() < 0.01);
    }

    #[test]
    fn center_maps_to_position() {
        let camera = Camera::new(5000.0, 3000.0, 1.0, 800.0, 600.0);

        // Center of screen should be camera position
        let center = camera.screen_to_world(400.0, 300.0);
        assert!((center.x - 5000.0).abs() < 0.01);
        assert!((center.y - 3000.0).abs() < 0.01);
    }
}
