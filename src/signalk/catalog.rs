//! What the well-known Signal K paths mean, for display.
//!
//! Signal K names a great many paths and a boat sends a handful of them. This
//! table gives the ones a plotter shows a short label and, more importantly, a
//! [`Quantity`] — without which a number cannot be turned into a reading a
//! mariner can act on.
//!
//! An unlisted path is not an error. It gets its last segment as a label and
//! is shown as a bare number, which is still useful; the table only makes the
//! common case read properly.

use super::units::Quantity;

/// A path Manx knows how to present.
pub struct Known {
    pub path: &'static str,
    /// Short enough for a tile on a phone-sized bar.
    pub label: &'static str,
    pub quantity: Quantity,
}

macro_rules! known {
    ($($path:literal => $label:literal, $q:ident;)*) => {
        pub const KNOWN: &[Known] = &[
            $(Known { path: $path, label: $label, quantity: Quantity::$q },)*
        ];
    };
}

known! {
    // The four a helm looks at, and the order they are usually read in.
    "navigation.speedOverGround"        => "SOG",        Speed;
    "navigation.courseOverGroundTrue"   => "COG",        Angle;
    "navigation.headingTrue"            => "HDG",        Angle;
    "navigation.headingMagnetic"        => "HDG M",      Angle;
    "environment.depth.belowTransducer" => "Depth",      Depth;
    "environment.depth.belowKeel"       => "Below keel", Depth;
    "environment.depth.belowSurface"    => "Depth surf", Depth;

    // Sailing.
    "environment.wind.speedApparent"    => "AWS",        Speed;
    "environment.wind.angleApparent"    => "AWA",        RelativeAngle;
    "environment.wind.speedTrue"        => "TWS",        Speed;
    "environment.wind.angleTrueWater"   => "TWA",        RelativeAngle;
    "environment.wind.directionTrue"    => "TWD",        Angle;
    "navigation.speedThroughWater"      => "STW",        Speed;
    "navigation.rateOfTurn"             => "ROT",        RelativeAngle;
    "steering.rudderAngle"              => "Rudder",     RelativeAngle;
    "navigation.attitude.roll"          => "Heel",       RelativeAngle;
    "navigation.attitude.pitch"         => "Pitch",      RelativeAngle;
    "navigation.magneticVariation"      => "Variation",  RelativeAngle;

    // Water and weather.
    "environment.water.temperature"     => "Sea temp",   Temperature;
    "environment.outside.temperature"   => "Air temp",   Temperature;
    "environment.outside.pressure"      => "Baro",       Pressure;
    "environment.outside.humidity"      => "Humidity",   Ratio;
    "environment.inside.temperature"    => "Cabin",      Temperature;

    // Log.
    "navigation.log"                    => "Log",        Distance;
    "navigation.trip.log"               => "Trip",       Distance;

    // The boat's own systems, which matter most when they are wrong.
    "electrical.batteries.house.voltage"        => "House V",  Voltage;
    "electrical.batteries.house.current"        => "House A",  Current;
    "electrical.batteries.house.capacity.stateOfCharge" => "House %", Ratio;
    "electrical.batteries.start.voltage"        => "Start V",  Voltage;
    "tanks.fuel.0.currentLevel"                 => "Fuel",     Ratio;
    "tanks.freshWater.0.currentLevel"           => "Water",    Ratio;
    "tanks.blackWater.0.currentLevel"           => "Waste",    Ratio;
    "propulsion.port.revolutions"               => "RPM",      Count;
    "propulsion.port.coolantTemperature"        => "Coolant",  Temperature;

    // Fix quality — worth a tile when the position looks wrong.
    "navigation.gnss.satellites"                => "Sats",     Count;
    "navigation.gnss.horizontalDilution"        => "HDOP",     Count;
}

/// Not a Signal K path: the tile that draws apparent and true wind on a
/// dial. It lives in the bar's list of paths so that it is saved, ordered
/// and removed like any other tile; the `manx.` prefix is one no server
/// sends.
pub const WIND_ROSE: &str = "manx.windRose";

/// Tiles fed by the route being followed rather than by the boat. Like the
/// wind rose they are `manx.` ids no server sends: id, tile label, and what
/// the settings window says about each.
pub const ROUTE_TILES: &[(&str, &str, &str)] = &[
    ("manx.route.wpDistance", "WP DTW", "distance to the next waypoint"),
    ("manx.route.wpTime", "WP TTG", "time to the next waypoint, at VMG"),
    ("manx.route.wpEta", "WP ETA", "arrival at the next waypoint, at VMG"),
    ("manx.route.destDistance", "Dest DTG", "distance to the end of the route"),
    ("manx.route.destTime", "Dest TTG", "time to the end of the route, at SOG or by the plan"),
    ("manx.route.destEta", "Dest ETA", "arrival at the end of the route, at SOG or by the plan"),
    ("manx.route.planDelta", "vs Plan", "ahead of or behind a weather route's plan"),
];

/// Whether a tile id is one of [`ROUTE_TILES`].
pub fn is_route_tile(id: &str) -> bool {
    ROUTE_TILES.iter().any(|t| t.0 == id)
}

/// A tile id as saved by an older build, brought up to date: the wind rose
/// was saved as `navcore.windRose` before the rename.
pub fn current_tile(path: String) -> String {
    if path == "navcore.windRose" {
        WIND_ROSE.to_string()
    } else {
        path
    }
}

/// The entry for a path, if Manx knows it.
pub fn lookup(path: &str) -> Option<&'static Known> {
    KNOWN.iter().find(|k| k.path == path)
}

/// How to label a path, known or not.
///
/// An unknown path falls back to its last segment, which is nearly always the
/// quantity's own name — `propulsion.starboard.oilPressure` becomes
/// `oilPressure`. Ugly, but honest and immediately recognisable.
pub fn label_for(path: &str) -> &str {
    if path == WIND_ROSE {
        return "Wind rose";
    }
    if let Some(t) = ROUTE_TILES.iter().find(|t| t.0 == path) {
        return t.1;
    }
    match lookup(path) {
        Some(k) => k.label,
        None => path.rsplit('.').next().unwrap_or(path),
    }
}

/// How to present a path's number. Unknown paths are shown as a bare count,
/// because guessing a unit is worse than showing none.
pub fn quantity_for(path: &str) -> Quantity {
    lookup(path).map(|k| k.quantity).unwrap_or(Quantity::Count)
}

/// What a bar shows before anyone has configured it.
///
/// Depth, speed, course and heading: the four a plotter is expected to answer
/// without being asked. Any that the boat does not send are simply absent, so
/// this is a preference rather than a promise.
pub const DEFAULT_BAR: &[&str] = &[
    "environment.depth.belowTransducer",
    "navigation.speedOverGround",
    "navigation.courseOverGroundTrue",
    "navigation.headingTrue",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_paths_carry_a_label_and_a_quantity() {
        let sog = lookup("navigation.speedOverGround").expect("SOG");
        assert_eq!(sog.label, "SOG");
        assert_eq!(sog.quantity, Quantity::Speed);
        // Apparent wind angle is signed, so it must not be a compass bearing.
        assert_eq!(
            quantity_for("environment.wind.angleApparent"),
            Quantity::RelativeAngle
        );
        assert_eq!(quantity_for("navigation.headingTrue"), Quantity::Angle);
    }

    #[test]
    fn unknown_paths_degrade_to_their_last_segment() {
        assert_eq!(
            label_for("propulsion.starboard.oilPressure"),
            "oilPressure"
        );
        assert_eq!(quantity_for("propulsion.starboard.oilPressure"), Quantity::Count);
        assert_eq!(label_for("odd"), "odd");
    }

    #[test]
    fn the_table_has_no_duplicate_paths() {
        // A duplicate would make lookup order decide the label, silently.
        let mut seen = std::collections::HashSet::new();
        for k in KNOWN {
            assert!(seen.insert(k.path), "duplicate path {}", k.path);
        }
    }

    #[test]
    fn every_default_bar_entry_is_a_known_path() {
        for path in DEFAULT_BAR {
            assert!(lookup(path).is_some(), "{path} is not in the table");
        }
    }

    #[test]
    fn labels_stay_short_enough_for_a_tile() {
        for k in KNOWN {
            assert!(k.label.len() <= 10, "{} is too long for a tile", k.label);
        }
    }
}
