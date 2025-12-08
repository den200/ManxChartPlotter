//! S-57 feature to symbol mapping.
//!
//! Maps S-57 object classes and attributes to SymbolId for rendering.

use crate::render::symbols::{SymbolId, SymbolInstance};
use crate::senc::features::{Feature, FeatureType, AttributeValue};

/// S-57 object class codes for symbol features
pub mod s57_classes {
    pub const BCNLAT: u16 = 14;  // Lateral beacon
    pub const BOYCAR: u16 = 16;  // Cardinal buoy
    pub const BOYLAT: u16 = 17;  // Lateral buoy
    pub const UWTROC: u16 = 153; // Underwater rock
}

/// Map an S-57 feature to a symbol ID based on object class and attributes
pub fn lookup_symbol(feature: &Feature) -> Option<SymbolId> {
    // Only point features have symbols
    if feature.feature_type != FeatureType::Point {
        return None;
    }

    match feature.type_code {
        s57_classes::BOYLAT => {
            // BOYLAT - Lateral buoy
            // S-57 COLOUR: 1=white, 2=black, 3=red, 4=green, 5=yellow
            // CATLAM: 1=port, 2=starboard, 3=preferred port, 4=preferred starboard
            // IALA-A (Europe/Africa/Asia): Red=Port, Green=Starboard
            let colour = get_attr_int(feature, "COLOUR").unwrap_or(0);
            let catlam = get_attr_int(feature, "CATLAM").unwrap_or(0);

            // Priority: CATLAM > COLOUR (CATLAM is more explicit)
            let is_port = match (catlam, colour) {
                (1, _) | (3, _) => true,   // CATLAM port or preferred port
                (2, _) | (4, _) => false,  // CATLAM starboard or preferred starboard
                (_, 3) => true,            // Red = port (IALA-A)
                (_, 4) => false,           // Green = starboard (IALA-A)
                _ => true,                 // Default to port
            };

            if is_port {
                Some(SymbolId::BuoyLatPort)
            } else {
                Some(SymbolId::BuoyLatStarboard)
            }
        }

        s57_classes::BCNLAT => {
            // BCNLAT - Lateral beacon
            // S-57 COLOUR: 1=white, 2=black, 3=red, 4=green
            // CATLAM: 1=port, 2=starboard, 3=preferred port, 4=preferred starboard
            let colour = get_attr_int(feature, "COLOUR").unwrap_or(0);
            let catlam = get_attr_int(feature, "CATLAM").unwrap_or(0);

            let is_port = match (catlam, colour) {
                (1, _) | (3, _) => true,   // CATLAM port or preferred port
                (2, _) | (4, _) => false,  // CATLAM starboard or preferred starboard
                (_, 3) => true,            // Red = port (IALA-A)
                (_, 4) => false,           // Green = starboard (IALA-A)
                _ => true,                 // Default to port
            };

            if is_port {
                Some(SymbolId::BeaconLatPort)
            } else {
                Some(SymbolId::BeaconLatStarboard)
            }
        }

        s57_classes::BOYCAR => {
            // BOYCAR - Cardinal buoy
            // CATCAM: 1=north, 2=east, 3=south, 4=west
            let catcam = get_attr_int(feature, "CATCAM").unwrap_or(1);

            match catcam {
                1 => Some(SymbolId::BuoyCarNorth),
                2 => Some(SymbolId::BuoyCarEast),
                3 => Some(SymbolId::BuoyCarSouth),
                4 => Some(SymbolId::BuoyCarWest),
                _ => Some(SymbolId::BuoyCarNorth), // Default to north
            }
        }

        s57_classes::UWTROC => {
            // UWTROC - Underwater rock
            // WATLEV: 2=dry, 3=awash, 4/5=submerged
            let watlev = get_attr_int(feature, "WATLEV").unwrap_or(0);

            if watlev == 3 {
                Some(SymbolId::RockAwash)
            } else {
                Some(SymbolId::RockSubmerged)
            }
        }

        _ => None,
    }
}

/// Convert lat/lon to SM (Simple Mercator) meters relative to reference point.
///
/// Uses the same formula as OpenCPN (georef.cpp:354-375).
/// SENC area/line geometry is already in SM meters centered on (ref_lat, ref_lon).
/// Point geometry stores raw WGS84 lat/lon and needs this conversion.
fn latlon_to_sm(lat: f64, lon: f64, ref_lat: f64, ref_lon: f64) -> [f32; 2] {
    use std::f64::consts::PI;

    const DEGREE: f64 = PI / 180.0;
    const WGS84_SEMIMAJOR: f64 = 6378137.0;
    const MERCATOR_K0: f64 = 0.9996;
    const Z: f64 = WGS84_SEMIMAJOR * MERCATOR_K0;

    // X: Linear in longitude
    let dx = (lon - ref_lon) * DEGREE * Z;

    // Y: Mercator formula with reference offset
    let s = (lat * DEGREE).sin();
    let y = (0.5 * ((1.0 + s) / (1.0 - s)).ln()) * Z;

    let s0 = (ref_lat * DEGREE).sin();
    let y0 = (0.5 * ((1.0 + s0) / (1.0 - s0)).ln()) * Z;

    let dy = y - y0;

    [dx as f32, dy as f32]
}

/// Get symbol position from feature geometry, converted to SM meters
fn get_symbol_position_sm(feature: &Feature, ref_lat: f64, ref_lon: f64) -> Option<[f32; 2]> {
    feature.point_geometry.as_ref().map(|pg| {
        // OSENC point geometry: pg.x = latitude, pg.y = longitude (WGS84 degrees)
        // (opposite of typical lon/lat convention)
        latlon_to_sm(pg.x, pg.y, ref_lat, ref_lon)
    })
}

/// Convert features to symbol instances with coordinate conversion
pub fn features_to_instances(features: &[Feature], ref_lat: f64, ref_lon: f64) -> Vec<SymbolInstance> {
    println!("DEBUG: features_to_instances called with ref_lat={}, ref_lon={}", ref_lat, ref_lon);

    // Debug: show ALL point feature type codes to identify what's available
    let point_features: Vec<_> = features.iter()
        .filter(|f| f.feature_type == FeatureType::Point)
        .collect();
    println!("DEBUG: Point features type codes:");
    for f in &point_features {
        println!("  type_code={} (looking for BOYLAT=17, BCNLAT=14, UWTROC=153)", f.type_code);
    }
    println!("DEBUG: Total {} point features, checking for matching symbols...", point_features.len());

    let instances: Vec<SymbolInstance> = features
        .iter()
        .filter_map(|f| {
            let symbol_id = lookup_symbol(f)?;
            let position = get_symbol_position_sm(f, ref_lat, ref_lon)?;

            // Debug: show raw WGS84 coords and converted SM coords for first few symbols
            if let Some(pg) = &f.point_geometry {
                println!("DEBUG: Symbol type_code={} raw WGS84: lat={:.6}, lon={:.6} -> SM: ({:.0}, {:.0})",
                    f.type_code, pg.x, pg.y, position[0], position[1]);
                // Also show attributes for buoys
                if f.type_code == 17 {  // BOYLAT
                    let colour = get_attr_int(f, "COLOUR");
                    let catlam = get_attr_int(f, "CATLAM");
                    println!("  BOYLAT attributes: COLOUR={:?}, CATLAM={:?}", colour, catlam);
                }
            }

            Some(SymbolInstance {
                position,
                symbol_id: symbol_id as u32,
                rotation: 0.0,
            })
        })
        .collect();

    // Debug: show symbol breakdown by type
    let mut port_count = 0;
    let mut starboard_count = 0;
    let mut beacon_port = 0;
    let mut beacon_starboard = 0;
    let mut rock_awash = 0;
    let mut rock_submerged = 0;
    let mut car_north = 0;
    let mut car_east = 0;
    let mut car_south = 0;
    let mut car_west = 0;

    for inst in &instances {
        match inst.symbol_id {
            0 => port_count += 1,           // BuoyLatPort
            1 => starboard_count += 1,      // BuoyLatStarboard
            2 => beacon_port += 1,          // BeaconLatPort
            3 => beacon_starboard += 1,     // BeaconLatStarboard
            4 => rock_awash += 1,           // RockAwash
            5 => rock_submerged += 1,       // RockSubmerged
            6 => car_north += 1,            // BuoyCarNorth
            7 => car_east += 1,             // BuoyCarEast
            8 => car_south += 1,            // BuoyCarSouth
            9 => car_west += 1,             // BuoyCarWest
            _ => {}
        }
    }

    println!("DEBUG: Symbol breakdown:");
    println!("  Lateral Buoys - Port(red): {}, Starboard(green): {}", port_count, starboard_count);
    println!("  Cardinal Buoys - N: {}, E: {}, S: {}, W: {}", car_north, car_east, car_south, car_west);
    println!("  Beacons - Port: {}, Starboard: {}", beacon_port, beacon_starboard);
    println!("  Rocks - Awash: {}, Submerged: {}", rock_awash, rock_submerged);
    println!("  Total symbols: {}", instances.len());

    instances
}

/// Helper to get integer attribute value
fn get_attr_int(feature: &Feature, name: &str) -> Option<i32> {
    match feature.attributes.get(name) {
        Some(AttributeValue::Integer(v)) => Some(*v),
        Some(AttributeValue::Float(v)) => Some(*v as i32),
        _ => None,
    }
}
