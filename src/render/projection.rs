//! Mercator projection for chart coordinates.
//!
//! Converts WGS84 lat/lon to Mercator meters for rendering.
//! The SENC files already contain pre-projected Mercator coordinates,
//! so this is mainly for computing extents and viewport transforms.

use std::f64::consts::PI;

/// WGS84 semi-major axis in meters
const WGS84_SEMIMAJOR: f64 = 6_378_137.0;

/// Scale factor for SM (Simple Mercator) - same as OpenCPN
/// This matches the constant used in geometry.rs for SM→Global conversion
const MERCATOR_K0: f64 = 0.9996;

/// Earth radius for Mercator calculations
/// Uses K0 factor to match OpenCPN's SM coordinate system
pub const EARTH_RADIUS: f64 = WGS84_SEMIMAJOR * MERCATOR_K0; // ~6378123.44

/// Maximum latitude for Mercator (avoids infinity at poles)
pub const MAX_LAT: f64 = 85.051129;

/// Mercator projection utilities
#[derive(Debug, Clone, Copy)]
pub struct Projection {
    /// Reference latitude for scale calculations
    pub ref_lat: f64,
    /// Reference longitude for centering
    pub ref_lon: f64,
}

impl Default for Projection {
    fn default() -> Self {
        Self {
            ref_lat: 0.0,
            ref_lon: 0.0,
        }
    }
}

impl Projection {
    /// Create projection centered on given coordinates
    pub fn centered_on(lat: f64, lon: f64) -> Self {
        Self {
            ref_lat: lat.clamp(-MAX_LAT, MAX_LAT),
            ref_lon: lon,
        }
    }

    /// Convert latitude to Mercator Y (meters from equator)
    pub fn lat_to_y(lat: f64) -> f64 {
        let lat_rad = lat.clamp(-MAX_LAT, MAX_LAT).to_radians();
        EARTH_RADIUS * (PI / 4.0 + lat_rad / 2.0).tan().ln()
    }

    /// Convert longitude to Mercator X (meters from prime meridian)
    pub fn lon_to_x(lon: f64) -> f64 {
        lon.to_radians() * EARTH_RADIUS
    }

    /// Convert Mercator Y back to latitude
    pub fn y_to_lat(y: f64) -> f64 {
        (2.0 * (y / EARTH_RADIUS).exp().atan() - PI / 2.0).to_degrees()
    }

    /// Convert Mercator X back to longitude
    pub fn x_to_lon(x: f64) -> f64 {
        (x / EARTH_RADIUS).to_degrees()
    }

    /// Convert WGS84 to Mercator coordinates
    pub fn to_mercator(lat: f64, lon: f64) -> (f64, f64) {
        (Self::lon_to_x(lon), Self::lat_to_y(lat))
    }

    /// Convert Mercator to WGS84 coordinates
    pub fn to_wgs84(x: f64, y: f64) -> (f64, f64) {
        (Self::y_to_lat(y), Self::x_to_lon(x))
    }

    /// Calculate scale factor at given latitude
    ///
    /// Mercator distorts distances away from the equator.
    /// This returns how many meters on ground = 1 Mercator meter.
    pub fn scale_at_lat(lat: f64) -> f64 {
        lat.to_radians().cos()
    }

    /// Convert screen pixels to Mercator meters given zoom level
    ///
    /// Zoom level follows OpenStreetMap convention:
    /// - zoom 0: entire world in 256px
    /// - zoom 1: world in 512px
    /// - etc.
    pub fn pixels_to_meters(zoom: f64) -> f64 {
        // At zoom 0, world is 256 pixels = 2π × R meters
        let world_px = 256.0 * 2.0_f64.powf(zoom);
        2.0 * PI * EARTH_RADIUS / world_px
    }

    /// Convert Mercator meters to screen pixels given zoom level
    pub fn meters_to_pixels(zoom: f64) -> f64 {
        1.0 / Self::pixels_to_meters(zoom)
    }
}

/// Axis-aligned bounding box in Mercator coordinates
#[derive(Debug, Clone, Copy)]
pub struct MercatorBounds {
    pub min_x: f64,
    pub max_x: f64,
    pub min_y: f64,
    pub max_y: f64,
}

impl MercatorBounds {
    /// Create from WGS84 extent
    pub fn from_wgs84(min_lat: f64, max_lat: f64, min_lon: f64, max_lon: f64) -> Self {
        let (min_x, min_y) = Projection::to_mercator(min_lat, min_lon);
        let (max_x, max_y) = Projection::to_mercator(max_lat, max_lon);

        Self {
            min_x: min_x.min(max_x),
            max_x: min_x.max(max_x),
            min_y: min_y.min(max_y),
            max_y: min_y.max(max_y),
        }
    }

    /// Center point
    pub fn center(&self) -> (f64, f64) {
        ((self.min_x + self.max_x) / 2.0, (self.min_y + self.max_y) / 2.0)
    }

    /// Width in Mercator meters
    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }

    /// Height in Mercator meters
    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }

    /// Expand bounds to include a point
    pub fn include_point(&mut self, x: f64, y: f64) {
        self.min_x = self.min_x.min(x);
        self.max_x = self.max_x.max(x);
        self.min_y = self.min_y.min(y);
        self.max_y = self.max_y.max(y);
    }

    /// Expand bounds by a margin (in meters)
    pub fn expand(&self, margin: f64) -> Self {
        Self {
            min_x: self.min_x - margin,
            max_x: self.max_x + margin,
            min_y: self.min_y - margin,
            max_y: self.max_y + margin,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mercator_roundtrip() {
        let lat = 55.67; // Copenhagen
        let lon = 12.57;

        let (x, y) = Projection::to_mercator(lat, lon);
        let (lat2, lon2) = Projection::to_wgs84(x, y);

        assert!((lat - lat2).abs() < 0.0001);
        assert!((lon - lon2).abs() < 0.0001);
    }

    #[test]
    fn equator_no_distortion() {
        let scale = Projection::scale_at_lat(0.0);
        assert!((scale - 1.0).abs() < 0.0001);
    }

    #[test]
    fn polar_high_distortion() {
        let scale = Projection::scale_at_lat(80.0);
        assert!(scale < 0.2); // Much less than 1
    }
}
