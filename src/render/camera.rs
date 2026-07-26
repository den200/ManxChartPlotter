//! Chart camera with pan/zoom, and an optional tilt.
//!
//! Provides view-projection matrix for rendering chart data
//! in Mercator coordinates.
//!
//! Untilted the camera is orthographic and one screen pixel is `zoom` metres
//! everywhere, which is what a chart is: a plan. [`tilt`](Camera::tilt) pitches
//! the eye back for a perspective view of the same plane — a display mode, not
//! a chart projection. S-52 says nothing about it, and nothing in the render
//! path changes shape under it: symbols, text and line widths are already
//! applied as screen-space offsets scaled by `clip.w`, so they keep their
//! nominal pixel size at any distance. What changes is which chart *scale* is
//! right where, since a fixed ground distance covers fewer pixels the further
//! away it is — see [`Camera::ground_mpp_at`].

use glam::{Mat4, Vec2, Vec3};

use super::projection::MercatorBounds;
use crate::tiles::TileBounds;

/// Vertical field of view used when tilted.
const FOV_Y: f32 = std::f32::consts::FRAC_PI_4;

/// Hard cap on tilt.
///
/// The top edge of the screen looks out at `tilt + FOV_Y/2` from straight down;
/// at 90° it reaches the horizon and beyond it there is no ground to draw. The
/// cap leaves 7.5° of margin, so the whole viewport is always on the chart —
/// no sky, no clipped edge — and the far edge lands about eight eye-distances
/// out, which is what the visible-tile count scales with.
pub const MAX_TILT: f32 = 60.0 * std::f32::consts::PI / 180.0;

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
    /// Pitch away from straight down, in radians. 0 is the plan view.
    pub tilt: f32,
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
            tilt: 0.0,
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
            tilt: 0.0,
        }
    }

    /// Update viewport size (e.g., on window resize)
    pub fn resize(&mut self, width: f32, height: f32) {
        self.viewport_width = width;
        self.viewport_height = height;
    }

    /// Pan camera by screen pixels
    ///
    /// Tilted, a pixel is not a fixed number of metres, so the drag is resolved
    /// on the ground: how far did the chart under the middle of the screen move.
    /// Using `zoom` there instead would make the map slide out from under the
    /// cursor as soon as the eye was pitched back.
    pub fn pan(&mut self, dx: f32, dy: f32) {
        if self.tilt <= 0.0 {
            // Convert screen movement to world movement
            self.position.x -= dx * self.zoom;
            self.position.y += dy * self.zoom; // Y is inverted
            return;
        }
        // The local ground scale at the middle of the screen, by symmetric
        // difference. Reading the ground under `centre - delta` directly would
        // be truer to the drag but is not reciprocal: a pixel up the screen is
        // worth far more ground than a pixel down it, so a drag out and back
        // would not return the view to where it started.
        let cx = self.viewport_width * 0.5;
        let cy = self.viewport_height * 0.5;
        let e = 1.0;
        let gx = (self.screen_to_world(cx + e, cy) - self.screen_to_world(cx - e, cy)) / (2.0 * e);
        let gy = (self.screen_to_world(cx, cy + e) - self.screen_to_world(cx, cy - e)) / (2.0 * e);
        self.position -= gx * dx + gy * dy;
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
    ///
    /// Tilted, this is a ray cast onto the chart plane, so it still answers the
    /// only question the caller ever has: which piece of sea is under this
    /// pixel. Rays that pass above the horizon have no answer; the tilt cap
    /// keeps them off the viewport, and a ray that somehow escapes is clamped
    /// to the far edge rather than returning a point behind the camera.
    pub fn screen_to_world(&self, screen_x: f32, screen_y: f32) -> Vec2 {
        if self.tilt <= 0.0 {
            let half_w = self.viewport_width / 2.0;
            let half_h = self.viewport_height / 2.0;
            return Vec2::new(
                self.position.x + (screen_x - half_w) * self.zoom,
                self.position.y + (half_h - screen_y) * self.zoom, // Y inverted
            );
        }
        let ndc = Vec2::new(
            screen_x / self.viewport_width * 2.0 - 1.0,
            1.0 - screen_y / self.viewport_height * 2.0,
        );
        self.ndc_to_ground(ndc)
    }

    /// Convert world coordinates to screen coordinates
    pub fn world_to_screen(&self, world_x: f32, world_y: f32) -> Vec2 {
        if self.tilt <= 0.0 {
            let half_w = self.viewport_width / 2.0;
            let half_h = self.viewport_height / 2.0;
            return Vec2::new(
                half_w + (world_x - self.position.x) / self.zoom,
                half_h - (world_y - self.position.y) / self.zoom, // Y inverted
            );
        }
        let clip = self.view_projection_matrix() * Vec3::new(world_x, world_y, 0.0).extend(1.0);
        if clip.w.abs() < 1e-6 {
            return Vec2::new(f32::NAN, f32::NAN);
        }
        let ndc = clip.truncate() / clip.w;
        Vec2::new(
            (ndc.x * 0.5 + 0.5) * self.viewport_width,
            (0.5 - ndc.y * 0.5) * self.viewport_height,
        )
    }

    /// Distance from the eye to the point it is looking at.
    ///
    /// Chosen so the centre of the screen keeps exactly the untilted scale:
    /// the frustum is `2 d tan(fov/2)` tall at the target, and that has to
    /// come to `viewport_height * zoom` metres.
    pub fn eye_distance(&self) -> f32 {
        self.viewport_height * self.zoom / (2.0 * (FOV_Y * 0.5).tan())
    }

    /// Eye position in world space (metres, chart plane at z = 0).
    pub fn eye(&self) -> Vec3 {
        let d = self.eye_distance();
        Vec3::new(
            self.position.x,
            self.position.y - d * self.tilt.sin(),
            d * self.tilt.cos(),
        )
    }

    /// Metres per pixel on the chart plane at a given ground point.
    ///
    /// Under perspective this grows with distance from the eye; untilted it is
    /// just `zoom` everywhere. Tile selection uses it to pick a coarser chart
    /// scale for the far half of the view — the whole reason the tilted view
    /// does not cost several times more tiles than the plan view.
    pub fn ground_mpp_at(&self, x: f64, y: f64) -> f32 {
        if self.tilt <= 0.0 {
            return self.zoom;
        }
        let d = self.eye_distance();
        let e = self.eye();
        let dist = (Vec3::new(x as f32, y as f32, 0.0) - e).length();
        self.zoom * (dist / d).max(1e-3)
    }

    /// View-projection matrix for shaders.
    pub fn view_projection_matrix(&self) -> Mat4 {
        if self.tilt <= 0.0 {
            // View: translate world so camera is at origin
            let view = Mat4::from_translation(Vec3::new(-self.position.x, -self.position.y, 0.0));

            // Projection: orthographic, scaled by zoom
            let half_w = self.viewport_width * self.zoom / 2.0;
            let half_h = self.viewport_height * self.zoom / 2.0;

            let proj = Mat4::orthographic_rh(-half_w, half_w, -half_h, half_h, -1.0, 1.0);

            return proj * view;
        }

        let d = self.eye_distance();
        let target = Vec3::new(self.position.x, self.position.y, 0.0);
        let up = Vec3::new(0.0, self.tilt.cos(), self.tilt.sin());
        let view = Mat4::look_at_rh(self.eye(), target, up);
        // The far plane sits beyond the ground the top edge reaches, so the
        // chart is never cut off in mid-water; the near plane is close enough
        // that the bottom edge is never clipped either.
        let aspect = self.viewport_width / self.viewport_height;
        let proj = Mat4::perspective_rh(FOV_Y, aspect, d * 0.02, d * 12.0);
        proj * view
    }

    /// Where a normalised-device-coordinate point lands on the chart plane.
    fn ndc_to_ground(&self, ndc: Vec2) -> Vec2 {
        let inv = self.view_projection_matrix().inverse();
        let near = inv * ndc.extend(0.0).extend(1.0);
        let far = inv * ndc.extend(1.0).extend(1.0);
        if near.w.abs() < 1e-9 || far.w.abs() < 1e-9 {
            return self.position;
        }
        let a = near.truncate() / near.w;
        let b = far.truncate() / far.w;
        let dz = a.z - b.z;
        // Parallel to the plane (at or above the horizon): take the far end.
        if dz.abs() < 1e-6 {
            return Vec2::new(b.x, b.y);
        }
        let t = (a.z / dz).clamp(0.0, 1.0);
        let p = a.lerp(b, t);
        Vec2::new(p.x, p.y)
    }

    /// The four screen corners cast onto the chart plane, in metres.
    ///
    /// Untilted this is just the view rectangle. Tilted it is a trapezium,
    /// narrow at the bottom of the screen and wide at the top.
    pub fn ground_footprint(&self) -> [Vec2; 4] {
        if self.tilt <= 0.0 {
            let half_w = self.viewport_width * self.zoom / 2.0;
            let half_h = self.viewport_height * self.zoom / 2.0;
            let (x, y) = (self.position.x, self.position.y);
            return [
                Vec2::new(x - half_w, y - half_h),
                Vec2::new(x + half_w, y - half_h),
                Vec2::new(x + half_w, y + half_h),
                Vec2::new(x - half_w, y + half_h),
            ];
        }
        [
            self.ndc_to_ground(Vec2::new(-1.0, -1.0)),
            self.ndc_to_ground(Vec2::new(1.0, -1.0)),
            self.ndc_to_ground(Vec2::new(1.0, 1.0)),
            self.ndc_to_ground(Vec2::new(-1.0, 1.0)),
        ]
    }

    /// Get visible bounds in world coordinates
    pub fn visible_bounds(&self) -> MercatorBounds {
        let b = self.visible_bounds_tile();
        MercatorBounds {
            min_x: b.min_x,
            max_x: b.max_x,
            min_y: b.min_y,
            max_y: b.max_y,
        }
    }

    /// Get visible bounds as TileBounds for tile system
    ///
    /// This is the bounding box of the ground footprint, so tilted it is the
    /// box around the trapezium — larger than the trapezium itself, and larger
    /// again than the untilted view. Only the tile walk narrows it back down.
    pub fn visible_bounds_tile(&self) -> TileBounds {
        let f = self.ground_footprint();
        let (mut min_x, mut max_x) = (f64::MAX, f64::MIN);
        let (mut min_y, mut max_y) = (f64::MAX, f64::MIN);
        for p in f {
            min_x = min_x.min(p.x as f64);
            max_x = max_x.max(p.x as f64);
            min_y = min_y.min(p.y as f64);
            max_y = max_y.max(p.y as f64);
        }
        TileBounds::new(min_x, max_x, min_y, max_y)
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
    fn tilt_zero_is_the_orthographic_view() {
        let mut cam = Camera::new(1000.0, 2000.0, 5.0, 800.0, 600.0);
        let flat = cam.view_projection_matrix();
        cam.tilt = 0.0;
        assert_eq!(flat, cam.view_projection_matrix());
    }

    #[test]
    fn tilted_footprint_is_a_trapezium_ahead_of_the_camera() {
        let mut cam = Camera::new(0.0, 0.0, 5.0, 800.0, 600.0);
        cam.tilt = MAX_TILT;
        let f = cam.ground_footprint();
        let near_width = (f[1].x - f[0].x).abs();
        let far_width = (f[2].x - f[3].x).abs();
        assert!(far_width > near_width * 1.5, "near {near_width} far {far_width}");
        // North is up, so the far edge is the northern one.
        assert!(f[2].y > f[1].y);
        // The centre of the screen still lands on the camera position.
        let mid = cam.screen_to_world(400.0, 300.0);
        assert!(mid.length() < 1.0, "centre drifted to {mid:?}");
        // And the centre keeps the untilted scale.
        let mpp = cam.ground_mpp_at(0.0, 0.0);
        assert!((mpp - cam.zoom).abs() < 0.01, "mpp {mpp}");
    }

    #[test]
    fn tilted_screen_world_roundtrip() {
        let mut cam = Camera::new(1000.0, 2000.0, 5.0, 800.0, 600.0);
        cam.tilt = 0.6;
        for &(sx, sy) in &[(400.0, 300.0), (100.0, 500.0), (700.0, 120.0)] {
            let w = cam.screen_to_world(sx, sy);
            let back = cam.world_to_screen(w.x, w.y);
            assert!((back.x - sx).abs() < 0.5 && (back.y - sy).abs() < 0.5, "{sx},{sy} -> {back:?}");
        }
    }

    #[test]
    fn tilted_pan_follows_the_ground() {
        let mut cam = Camera::new(0.0, 0.0, 5.0, 800.0, 600.0);
        cam.tilt = 0.7;
        let before = cam.screen_to_world(400.0, 300.0);
        cam.pan(0.0, 40.0); // drag the chart down: the view moves north
        let after = cam.screen_to_world(400.0, 300.0);
        assert!(after.y > before.y, "{before:?} -> {after:?}");
        // Dragging back returns to where it started.
        cam.pan(0.0, -40.0);
        let back = cam.screen_to_world(400.0, 300.0);
        assert!((back - before).length() < 1.0, "{before:?} -> {back:?}");
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
