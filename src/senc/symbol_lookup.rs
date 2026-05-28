//! S-57 feature to symbol mapping.
//!
//! Maps S-57 object classes and attributes to SymbolId for rendering.

use crate::render::symbols::{
    symbol_id_from_s52_name, symbol_name_from_id, SymbolId, SymbolInstance,
};
use crate::s52::cs::lights06_symbol;
use crate::s52::MarinerSettings;
use crate::senc::features::{AttributeValue, Feature, FeatureType};

/// S-57 object class codes for symbol features
/// (From IHO S-57 spec, matching records.rs ObjectClass::from_code)
pub mod s57_classes {
    pub const BCNCAR: u16 = 5; // Cardinal beacon
    pub const BCNISD: u16 = 6; // Isolated danger beacon
    pub const BCNLAT: u16 = 7; // Lateral beacon
    pub const BCNSAW: u16 = 8; // Safe water beacon
    pub const BCNSPP: u16 = 9; // Special purpose beacon
    pub const BOYCAR: u16 = 14; // Cardinal buoy
    pub const BOYISD: u16 = 16; // Isolated danger buoy
    pub const BOYLAT: u16 = 17; // Lateral buoy
    pub const BOYSAW: u16 = 18; // Safe water buoy
    pub const BOYSPP: u16 = 19; // Special purpose buoy
    pub const LIGHTS: u16 = 75; // Light
    pub const UWTROC: u16 = 153; // Underwater rock
    pub const WRECKS: u16 = 159; // Wreck
}

/// Map an S-57 feature to a symbol ID based on object class and attributes
pub fn lookup_symbol(feature: &Feature) -> Option<SymbolId> {
    // Only point features have symbols
    if feature.feature_type != FeatureType::Point {
        return None;
    }

    match feature.type_code {
        // LIGHTS uses dynamic symbol selection via CS procedure
        s57_classes::LIGHTS => {
            // Lights are handled by lookup_symbol_dynamic for S-52 CS support
            None
        }
        s57_classes::BOYLAT => {
            // BOYLAT - Lateral buoy
            // S-57 COLOUR: 1=white, 2=black, 3=red, 4=green, 5=yellow
            // CATLAM: 1=port, 2=starboard, 3=preferred port, 4=preferred starboard
            // IALA-A (Europe/Africa/Asia): Red=Port, Green=Starboard
            let colour = get_attr_int(feature, "COLOUR").unwrap_or(0);
            let catlam = get_attr_int(feature, "CATLAM").unwrap_or(0);

            // Priority: CATLAM > COLOUR (CATLAM is more explicit)
            let is_port = match (catlam, colour) {
                (1, _) | (3, _) => true,  // CATLAM port or preferred port
                (2, _) | (4, _) => false, // CATLAM starboard or preferred starboard
                (_, 3) => true,           // Red = port (IALA-A)
                (_, 4) => false,          // Green = starboard (IALA-A)
                _ => true,                // Default to port
            };

            if is_port {
                Some(SymbolId::BuoyLatPort)
            } else {
                Some(SymbolId::BuoyLatStarboard)
            }
        }

        s57_classes::BCNCAR => {
            // BCNCAR - Cardinal beacon (same logic as BOYCAR)
            // CATCAM: 1=north, 2=east, 3=south, 4=west
            let catcam = get_attr_int(feature, "CATCAM").unwrap_or(1);

            match catcam {
                1 => Some(SymbolId::BuoyCarNorth), // Using buoy symbol for now
                2 => Some(SymbolId::BuoyCarEast),
                3 => Some(SymbolId::BuoyCarSouth),
                4 => Some(SymbolId::BuoyCarWest),
                _ => Some(SymbolId::BuoyCarNorth),
            }
        }

        s57_classes::BCNLAT => {
            // BCNLAT - Lateral beacon
            // S-57 COLOUR: 1=white, 2=black, 3=red, 4=green
            // CATLAM: 1=port, 2=starboard, 3=preferred port, 4=preferred starboard
            let colour = get_attr_int(feature, "COLOUR").unwrap_or(0);
            let catlam = get_attr_int(feature, "CATLAM").unwrap_or(0);

            let is_port = match (catlam, colour) {
                (1, _) | (3, _) => true,  // CATLAM port or preferred port
                (2, _) | (4, _) => false, // CATLAM starboard or preferred starboard
                (_, 3) => true,           // Red = port (IALA-A)
                (_, 4) => false,          // Green = starboard (IALA-A)
                _ => true,                // Default to port
            };

            if is_port {
                Some(SymbolId::BeaconLatPort)
            } else {
                Some(SymbolId::BeaconLatStarboard)
            }
        }

        s57_classes::BCNSPP => {
            // BCNSPP - Special purpose beacon - render as port beacon (default)
            Some(SymbolId::BeaconLatPort)
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

/// Map an S-57 feature to a symbol atlas index using S-52 CS procedures.
///
/// This is the preferred lookup function for dynamic symbol selection.
/// Returns the atlas index directly (u32) instead of a SymbolId enum.
///
/// # Arguments
/// * `feature` - S-57 feature to lookup
/// * `settings` - Optional mariner settings for CS procedures
///
/// # Returns
/// Atlas index for the symbol, or None if no symbol
pub fn lookup_symbol_dynamic(feature: &Feature, settings: Option<&MarinerSettings>) -> Option<u32> {
    // Only point features have symbols
    if feature.feature_type != FeatureType::Point {
        return None;
    }

    match feature.type_code {
        s57_classes::LIGHTS => {
            // Use LIGHTS05/06 CS procedure to select symbol
            let default_settings = MarinerSettings::default();
            let s = settings.unwrap_or(&default_settings);
            let symbol_name = lights06_symbol(feature, s);
            symbol_id_from_s52_name(symbol_name)
        }

        // For other types, fall back to static lookup
        _ => lookup_symbol(feature).and_then(|id| symbol_id_from_s52_name(symbol_name_from_id(id))),
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
pub fn features_to_instances(
    features: &[Feature],
    ref_lat: f64,
    ref_lon: f64,
) -> Vec<SymbolInstance> {
    println!(
        "DEBUG: features_to_instances called with ref_lat={}, ref_lon={}",
        ref_lat, ref_lon
    );

    // Debug: show ALL point feature type codes to identify what's available
    let point_features: Vec<_> = features
        .iter()
        .filter(|f| f.feature_type == FeatureType::Point)
        .collect();
    println!("DEBUG: Point features type codes:");
    for f in &point_features {
        println!("  type_code={} (symbols: BCNCAR=5, BCNLAT=7, BCNSPP=9, BOYCAR=14, BOYLAT=17, UWTROC=153)", f.type_code);
    }
    println!(
        "DEBUG: Total {} point features, checking for matching symbols...",
        point_features.len()
    );

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
                disp_prio: 8,
                scale: 1.0,
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
            0 => port_count += 1,       // BuoyLatPort
            1 => starboard_count += 1,  // BuoyLatStarboard
            2 => beacon_port += 1,      // BeaconLatPort
            3 => beacon_starboard += 1, // BeaconLatStarboard
            4 => rock_awash += 1,       // RockAwash
            5 => rock_submerged += 1,   // RockSubmerged
            6 => car_north += 1,        // BuoyCarNorth
            7 => car_east += 1,         // BuoyCarEast
            8 => car_south += 1,        // BuoyCarSouth
            9 => car_west += 1,         // BuoyCarWest
            _ => {}
        }
    }

    println!("DEBUG: Symbol breakdown:");
    println!(
        "  Lateral Buoys - Port(red): {}, Starboard(green): {}",
        port_count, starboard_count
    );
    println!(
        "  Cardinal Buoys - N: {}, E: {}, S: {}, W: {}",
        car_north, car_east, car_south, car_west
    );
    println!(
        "  Beacons - Port: {}, Starboard: {}",
        beacon_port, beacon_starboard
    );
    println!(
        "  Rocks - Awash: {}, Submerged: {}",
        rock_awash, rock_submerged
    );
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

use crate::render::text::SoundingInstance;
use crate::s52::sndfrm02;

/// SOUNDG object class code (S-57)
pub const SOUNDG: u16 = 129;

/// Convert SOUNDG features to sounding instances with SNDFRM02 formatting.
///
/// SOUNDG features use multipoint geometry where each point is (x, y, z)
/// with z being the depth in meters. The coordinates are already in SM (Simple Mercator).
///
/// Uses SNDFRM02 to determine:
/// - Safety depth coloring (SNDG1 gray for safe, SNDG2 black for shallow)
/// - QUASOU/TECSOU/QUAPOS uncertainty indicators
/// - Drying height handling
pub fn soundings_to_instances(
    features: &[Feature],
    settings: &MarinerSettings,
) -> Vec<SoundingInstance> {
    let mut instances = Vec::new();

    for feature in features {
        // Only process SOUNDG features with multipoint geometry
        if feature.type_code != SOUNDG {
            continue;
        }

        if let Some(ref mp) = feature.multipoint_geometry {
            for point in &mp.points {
                // Multipoint geometry stores SM coords directly (x, y, depth)
                let x = point[0] as f32;
                let y = point[1] as f32;
                let depth = point[2] as f32;

                // Use SNDFRM02 to compute flags based on safety depth and attributes
                let render_info = sndfrm02(depth as f64, feature, settings);

                instances.push(SoundingInstance {
                    position: [x, y],
                    depth: render_info.whole_part as f32,
                    flags: render_info.to_flags(),
                    scale: 1.0,
                    color_index: 0,
                });
            }
        }
    }

    instances
}
