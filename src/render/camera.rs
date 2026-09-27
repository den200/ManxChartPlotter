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
//!
//! [`rotation`](Camera::rotation) turns the chart about the centre of the
//! screen: it is the true bearing that points up the screen, so 0 is the
//! ordinary north-up chart and the boat's heading is head-up. Like tilt it is
//! a view, not a projection. Text and plain symbols stay upright on the screen;
//! anything drawn *at a bearing* — a directional light, a heading line — has
//! to take the rotation off to stay pointing where it points on the ground.
//!
//! ## Precision
//!
//! The position and every transform are kept in f64. Global Mercator
//! coordinates at Danish latitudes are 7.5–8.4e6 m, where one f32 step is
//! 0.5–1 m: a camera stored in f32 could not pan by less than that, and a
//! matrix that carries the full translation loses the same metre again when
//! the GPU applies it. Geometry is therefore stored relative to a nearby
//! origin (each tile's centre), and the GPU is handed
//! [`view_projection_relative`](Camera::view_projection_relative) — the camera
//! transform with that origin folded in *in f64*, so the large terms cancel
//! before anything is rounded to f32.

use glam::{DMat4, DVec2, DVec3, DVec4, Mat4, Vec2};

use super::projection::MercatorBounds;
use crate::tiles::TileBounds;

/// Vertical field of view used when tilted.
const FOV_Y: f64 = std::f64::consts::FRAC_PI_4;

/// Hard cap on tilt.
///
/// The top edge of the screen looks out at `tilt + FOV_Y/2` from straight down;
/// at 90° it reaches the horizon and beyond it there is no ground to draw. The
/// cap leaves 7.5° of margin, so the whole viewport is always on the chart —
/// no sky, no clipped edge — and the far edge lands about eight eye-distances
/// out, which is what the visible-tile count scales with.
pub const MAX_TILT: f32 = 60.0 * std::f32::consts::PI / 180.0;

/// The closest zoom allowed, in Mercator metres per pixel.
///
/// Close enough to put a boat in her berth. This used to be 0.5: chart
/// vertices were f32 *global* Mercator, one step of which is 0.5–1 m here, and
/// below 0.5 m/px geometry visibly shuffled as the chart panned. Vertices are
/// now tile-relative and the camera is f64 (see the module note), so the floor
/// is a matter of usefulness rather than arithmetic.
pub const MIN_METRES_PER_PIXEL: f32 = 0.1;

/// 2D camera for chart viewing
#[derive(Debug, Clone)]
pub struct Camera {
    /// Camera center position in Mercator meters (f64: see the module note)
    pub position: DVec2,
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
    /// True bearing at the top of the screen, radians clockwise from north.
    /// 0 is north-up.
    pub rotation: f64,
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
            position: DVec2::new(cx, cy),
            zoom,
            viewport_width,
            viewport_height,
            // Allow zooming in 1000x, but no closer than the floor.
            min_zoom: (zoom * 0.001).max(MIN_METRES_PER_PIXEL),
            max_zoom: zoom * 10.0,  // Allow zooming out 10x
            tilt: 0.0,
            rotation: 0.0,
        }
    }

    /// Create camera at specific location
    pub fn new(center_x: f64, center_y: f64, zoom: f32, width: f32, height: f32) -> Self {
        Self {
            position: DVec2::new(center_x, center_y),
            zoom,
            viewport_width: width,
            viewport_height: height,
            min_zoom: MIN_METRES_PER_PIXEL,
            max_zoom: 1_000_000.0,
            tilt: 0.0,
            rotation: 0.0,
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
        let (dx, dy) = (dx as f64, dy as f64);
        if self.tilt <= 0.0 {
            // Convert screen movement to world movement
            let zoom = self.zoom as f64;
            // Screen y is down, the view's y is up.
            self.position -= self.view_to_world(DVec2::new(dx * zoom, -dy * zoom));
            return;
        }
        // The local ground scale at the middle of the screen, by symmetric
        // difference. Reading the ground under `centre - delta` directly would
        // be truer to the drag but is not reciprocal: a pixel up the screen is
        // worth far more ground than a pixel down it, so a drag out and back
        // would not return the view to where it started.
        let cx = self.viewport_width * 0.5;
        let cy = self.viewport_height * 0.5;
        let e = 1.0f32;
        let two_e = 2.0 * e as f64;
        let gx = (self.screen_to_world(cx + e, cy) - self.screen_to_world(cx - e, cy)) / two_e;
        let gy = (self.screen_to_world(cx, cy + e) - self.screen_to_world(cx, cy - e)) / two_e;
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
        self.position += world_before - world_after;
    }

    /// Turn the chart by `delta` radians clockwise on the screen, about a
    /// screen point, which keeps the same piece of chart under it — the two
    /// fingers of a twist, or the middle of the screen.
    ///
    /// The chart turning clockwise brings a bearing further anticlockwise to
    /// the top, so the rotation (the bearing at the top) goes *down*.
    pub fn rotate_at(&mut self, delta: f64, screen_x: f32, screen_y: f32) {
        let before = self.screen_to_world(screen_x, screen_y);
        self.rotation = (self.rotation - delta).rem_euclid(std::f64::consts::TAU);
        let after = self.screen_to_world(screen_x, screen_y);
        self.position += before - after;
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
    pub fn screen_to_world(&self, screen_x: f32, screen_y: f32) -> DVec2 {
        if self.tilt <= 0.0 {
            let half_w = self.viewport_width as f64 / 2.0;
            let half_h = self.viewport_height as f64 / 2.0;
            let zoom = self.zoom as f64;
            let v = DVec2::new(
                (screen_x as f64 - half_w) * zoom,
                (half_h - screen_y as f64) * zoom, // Y inverted
            );
            return self.position + self.view_to_world(v);
        }
        let ndc = DVec2::new(
            screen_x as f64 / self.viewport_width as f64 * 2.0 - 1.0,
            1.0 - screen_y as f64 / self.viewport_height as f64 * 2.0,
        );
        self.ndc_to_ground(ndc)
    }

    /// Convert world coordinates to screen coordinates
    pub fn world_to_screen(&self, world_x: f64, world_y: f64) -> Vec2 {
        if self.tilt <= 0.0 {
            let half_w = self.viewport_width as f64 / 2.0;
            let half_h = self.viewport_height as f64 / 2.0;
            let zoom = self.zoom as f64;
            let v = self.world_to_view(DVec2::new(world_x, world_y) - self.position);
            return Vec2::new(
                (half_w + v.x / zoom) as f32,
                (half_h - v.y / zoom) as f32, // Y inverted
            );
        }
        let clip = self.view_projection_f64() * DVec4::new(world_x, world_y, 0.0, 1.0);
        if clip.w.abs() < 1e-9 {
            return Vec2::new(f32::NAN, f32::NAN);
        }
        let ndc = clip.truncate() / clip.w;
        Vec2::new(
            ((ndc.x * 0.5 + 0.5) * self.viewport_width as f64) as f32,
            ((0.5 - ndc.y * 0.5) * self.viewport_height as f64) as f32,
        )
    }

    /// Distance from the eye to the point it is looking at.
    ///
    /// Chosen so the centre of the screen keeps exactly the untilted scale:
    /// the frustum is `2 d tan(fov/2)` tall at the target, and that has to
    /// come to `viewport_height * zoom` metres.
    pub fn eye_distance(&self) -> f64 {
        self.viewport_height as f64 * self.zoom as f64 / (2.0 * (FOV_Y * 0.5).tan())
    }

    /// Eye position in world space (metres, chart plane at z = 0).
    ///
    /// Tilted, the eye stands back from the target against the direction the
    /// top of the screen faces, and looks forward along it.
    pub fn eye(&self) -> DVec3 {
        let d = self.eye_distance();
        let tilt = self.tilt as f64;
        let back = self.position - self.forward() * (d * tilt.sin());
        DVec3::new(back.x, back.y, d * tilt.cos())
    }

    /// The ground direction the top of the screen faces: north when
    /// north-up, the boat's heading when head-up.
    pub fn forward(&self) -> DVec2 {
        DVec2::new(self.rotation.sin(), self.rotation.cos())
    }

    /// A ground offset turned into the view's axes (x right, y up the screen).
    fn world_to_view(&self, w: DVec2) -> DVec2 {
        let (s, c) = self.rotation.sin_cos();
        DVec2::new(w.x * c - w.y * s, w.x * s + w.y * c)
    }

    /// The inverse of [`world_to_view`](Self::world_to_view).
    fn view_to_world(&self, v: DVec2) -> DVec2 {
        let (s, c) = self.rotation.sin_cos();
        DVec2::new(v.x * c + v.y * s, -v.x * s + v.y * c)
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
        let dist = (DVec3::new(x, y, 0.0) - self.eye()).length();
        (self.zoom as f64 * (dist / d).max(1e-3)) as f32
    }

    /// View-projection matrix in f64, for world (global Mercator) positions.
    pub fn view_projection_f64(&self) -> DMat4 {
        if self.tilt <= 0.0 {
            // View: translate world so camera is at origin
            // Rotating anticlockwise by the bearing brings that bearing to
            // screen-up.
            let view = DMat4::from_rotation_z(self.rotation)
                * DMat4::from_translation(DVec3::new(-self.position.x, -self.position.y, 0.0));

            // Projection: orthographic, scaled by zoom
            let half_w = self.viewport_width as f64 * self.zoom as f64 / 2.0;
            let half_h = self.viewport_height as f64 * self.zoom as f64 / 2.0;

            let proj = DMat4::orthographic_rh(-half_w, half_w, -half_h, half_h, -1.0, 1.0);

            return proj * view;
        }

        let d = self.eye_distance();
        let tilt = self.tilt as f64;
        let target = DVec3::new(self.position.x, self.position.y, 0.0);
        let f = self.forward();
        let up = DVec3::new(f.x * tilt.cos(), f.y * tilt.cos(), tilt.sin());
        let view = DMat4::look_at_rh(self.eye(), target, up);
        // The far plane sits beyond the ground the top edge reaches, so the
        // chart is never cut off in mid-water; the near plane is close enough
        // that the bottom edge is never clipped either.
        let aspect = self.viewport_width as f64 / self.viewport_height as f64;
        let proj = DMat4::perspective_rh(FOV_Y, aspect, d * 0.02, d * 12.0);
        proj * view
    }

    /// View-projection matrix for positions stored relative to `origin`.
    ///
    /// `origin` is folded in before the cast to f32, so the matrix the GPU
    /// sees only ever carries the (small) distance from the camera to the
    /// origin. This is what makes tile-relative vertices precise.
    pub fn view_projection_relative(&self, origin: DVec2) -> Mat4 {
        (self.view_projection_f64() * DMat4::from_translation(origin.extend(0.0))).as_mat4()
    }

    /// View-projection matrix for positions in global Mercator metres.
    ///
    /// Only for data that is still global (the single-chart debug path);
    /// anything drawn at close zoom must use
    /// [`view_projection_relative`](Self::view_projection_relative).
    pub fn view_projection_matrix(&self) -> Mat4 {
        self.view_projection_f64().as_mat4()
    }

    /// Where a normalised-device-coordinate point lands on the chart plane.
    fn ndc_to_ground(&self, ndc: DVec2) -> DVec2 {
        let inv = self.view_projection_f64().inverse();
        let near = inv * DVec4::new(ndc.x, ndc.y, 0.0, 1.0);
        let far = inv * DVec4::new(ndc.x, ndc.y, 1.0, 1.0);
        if near.w.abs() < 1e-12 || far.w.abs() < 1e-12 {
            return self.position;
        }
        let a = near.truncate() / near.w;
        let b = far.truncate() / far.w;
        let dz = a.z - b.z;
        // Parallel to the plane (at or above the horizon): take the far end.
        if dz.abs() < 1e-9 {
            return DVec2::new(b.x, b.y);
        }
        let t = (a.z / dz).clamp(0.0, 1.0);
        let p = a.lerp(b, t);
        DVec2::new(p.x, p.y)
    }

    /// The four screen corners cast onto the chart plane, in metres.
    ///
    /// Untilted this is just the view rectangle (turned, if the chart is).
    /// Tilted it is a trapezium, narrow at the bottom of the screen and wide
    /// at the top. Either way the order is bottom-left, bottom-right,
    /// top-right, top-left as seen on the screen.
    pub fn ground_footprint(&self) -> [DVec2; 4] {
        if self.tilt <= 0.0 {
            let half_w = self.viewport_width as f64 * self.zoom as f64 / 2.0;
            let half_h = self.viewport_height as f64 * self.zoom as f64 / 2.0;
            return [
                DVec2::new(-half_w, -half_h),
                DVec2::new(half_w, -half_h),
                DVec2::new(half_w, half_h),
                DVec2::new(-half_w, half_h),
            ]
            .map(|v| self.position + self.view_to_world(v));
        }
        [
            self.ndc_to_ground(DVec2::new(-1.0, -1.0)),
            self.ndc_to_ground(DVec2::new(1.0, -1.0)),
            self.ndc_to_ground(DVec2::new(1.0, 1.0)),
            self.ndc_to_ground(DVec2::new(-1.0, 1.0)),
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
            min_x = min_x.min(p.x);
            max_x = max_x.max(p.x);
            min_y = min_y.min(p.y);
            max_y = max_y.max(p.y);
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
    fn rotated_screen_world_roundtrip() {
        for tilt in [0.0, 0.6] {
            let mut cam = Camera::new(1000.0, 2000.0, 5.0, 800.0, 600.0);
            cam.tilt = tilt;
            cam.rotation = 1.1;
            for &(sx, sy) in &[(400.0, 300.0), (100.0, 500.0), (700.0, 120.0)] {
                let w = cam.screen_to_world(sx, sy);
                let back = cam.world_to_screen(w.x, w.y);
                assert!((back.x - sx).abs() < 0.5 && (back.y - sy).abs() < 0.5, "{sx},{sy} -> {back:?}");
            }
            // The matrix the GPU gets agrees with the CPU mapping.
            let p = cam.screen_to_world(150.0, 450.0);
            let got = gpu_screen(&cam, DVec2::ZERO, [p.x as f32, p.y as f32]);
            assert!((got - Vec2::new(150.0, 450.0)).length() < 0.5, "tilt {tilt}: {got:?}");
        }
    }

    #[test]
    fn head_up_puts_the_heading_at_the_top() {
        let mut cam = Camera::new(0.0, 0.0, 1.0, 800.0, 600.0);
        cam.rotation = std::f64::consts::FRAC_PI_2; // heading east
        // A point due east of the camera is straight up the screen.
        let s = cam.world_to_screen(100.0, 0.0);
        assert!((s.x - 400.0).abs() < 0.01 && (s.y - 200.0).abs() < 0.01, "{s:?}");
        // So north is off to the left.
        let n = cam.world_to_screen(0.0, 100.0);
        assert!(n.x < 400.0 && (n.y - 300.0).abs() < 0.01, "{n:?}");
        // Dragging the chart down the screen moves the view east, ahead.
        cam.pan(0.0, 50.0);
        assert!(cam.position.x > 49.0 && cam.position.y.abs() < 0.01, "{:?}", cam.position);
    }

    #[test]
    fn rotated_footprint_covers_the_screen_corners() {
        let mut cam = Camera::new(0.0, 0.0, 2.0, 800.0, 600.0);
        cam.rotation = 0.5;
        let f = cam.ground_footprint();
        let corners = [(0.0, 600.0), (800.0, 600.0), (800.0, 0.0), (0.0, 0.0)];
        for (p, (sx, sy)) in f.iter().zip(corners) {
            let w = cam.screen_to_world(sx, sy);
            assert!((*p - w).length() < 0.01, "{p:?} vs {w:?}");
        }
    }

    #[test]
    fn rotating_about_a_point_keeps_it_still() {
        let mut cam = Camera::new(1000.0, 2000.0, 3.0, 800.0, 600.0);
        let under = cam.screen_to_world(600.0, 150.0);
        cam.rotate_at(0.4, 600.0, 150.0);
        assert!((cam.screen_to_world(600.0, 150.0) - under).length() < 0.01);
        // A clockwise twist brings a bearing west of north to the top.
        assert!(cam.rotation > std::f64::consts::PI, "{}", cam.rotation);
    }

    #[test]
    fn center_maps_to_position() {
        let camera = Camera::new(5000.0, 3000.0, 1.0, 800.0, 600.0);

        // Center of screen should be camera position
        let center = camera.screen_to_world(400.0, 300.0);
        assert!((center.x - 5000.0).abs() < 0.01);
        assert!((center.y - 3000.0).abs() < 0.01);
    }

    /// Where the GPU puts `rel` (metres from `origin`), in screen pixels,
    /// doing the arithmetic in f32 exactly as the vertex shader does.
    fn gpu_screen(cam: &Camera, origin: DVec2, rel: [f32; 2]) -> Vec2 {
        let clip = cam.view_projection_relative(origin) * glam::Vec4::new(rel[0], rel[1], 0.0, 1.0);
        let ndc = clip.truncate() / clip.w;
        Vec2::new(
            (ndc.x * 0.5 + 0.5) * cam.viewport_width,
            (0.5 - ndc.y * 0.5) * cam.viewport_height,
        )
    }

    /// Copenhagen, at the closest zoom. A point a few metres from the camera,
    /// stored relative to a tile centre, must land within a hundredth of a
    /// pixel of where f64 puts it.
    #[test]
    fn tile_relative_positions_are_exact_at_the_closest_zoom() {
        let cam = Camera::new(1_404_812.37, 7_503_991.81, MIN_METRES_PER_PIXEL, 1400.0, 900.0);
        let origin = DVec2::new(1_404_850.0, 7_503_960.0); // a z18 tile centre nearby
        let p = cam.position + DVec2::new(12.345, -6.789);
        let expected = cam.world_to_screen(p.x, p.y);
        assert!((expected.x - (700.0 + 123.45)).abs() < 1e-3, "{expected:?}");

        let rel = [(p.x - origin.x) as f32, (p.y - origin.y) as f32];
        let got = gpu_screen(&cam, origin, rel);
        assert!((got - expected).length() < 0.01, "relative: {got:?} vs {expected:?}");
        // (The old scheme — `[p.x as f32, p.y as f32]` through the global
        // matrix — misses here by about 2.6 px, and by a different amount for
        // every point, which is what made the chart shuffle as it panned.)
    }

    /// Same, tilted: the perspective matrix is re-based in f64 too.
    #[test]
    fn tile_relative_positions_are_exact_when_tilted() {
        let mut cam = Camera::new(1_404_812.37, 7_503_991.81, 0.2, 1400.0, 900.0);
        cam.tilt = 0.8;
        let origin = DVec2::new(1_404_700.0, 7_504_100.0);
        for off in [DVec2::new(3.3, 20.1), DVec2::new(-40.7, 55.2), DVec2::new(0.01, -0.02)] {
            let p = cam.position + off;
            let expected = cam.world_to_screen(p.x, p.y);
            let rel = [(p.x - origin.x) as f32, (p.y - origin.y) as f32];
            let got = gpu_screen(&cam, origin, rel);
            assert!((got - expected).length() < 0.02, "{off:?}: {got:?} vs {expected:?}");
        }
    }

    /// A quarter-pixel drag at 0.1 m/px is 2.5 cm, which an f32 camera at
    /// 7.5e6 m could not represent at all: the chart would not move.
    #[test]
    fn a_sub_pixel_pan_moves_the_chart_by_exactly_that_much() {
        let mut cam = Camera::new(1_404_812.0, 7_503_991.0, MIN_METRES_PER_PIXEL, 1400.0, 900.0);
        let origin = DVec2::new(1_404_850.0, 7_503_960.0);
        let rel = [-20.0f32, 15.0];
        let before = gpu_screen(&cam, origin, rel);
        cam.pan(0.25, 0.0);
        let after = gpu_screen(&cam, origin, rel);
        assert!(((after.x - before.x) - 0.25).abs() < 0.01, "{before:?} -> {after:?}");
        assert!((after.y - before.y).abs() < 0.01);
    }
}
