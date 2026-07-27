//! Signal K speaks SI. Mariners do not.
//!
//! Every number on the wire is SI: speeds in metres per second, **angles in
//! radians**, temperatures in **kelvin**, pressures in pascals. A plotter that
//! forgets this shows a boat doing 3.4 knots as "3.4" and a wind angle of 38°
//! as "0.7", both of which look plausible enough to be believed. Conversion
//! happens once, here, at the point of display — never in the parser, so the
//! stored value always means exactly what Signal K said.

/// What a number *is*, which decides how it is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quantity {
    /// m/s on the wire.
    Speed,
    /// Radians on the wire, 0..2π. Shown as a compass bearing.
    Angle,
    /// Radians on the wire, signed. Shown as ±180°, for wind and rudder where
    /// "40° to port" is the useful reading and "320°" is not.
    RelativeAngle,
    /// Metres.
    Depth,
    /// Metres, for longer distances.
    Distance,
    /// Kelvin on the wire.
    Temperature,
    /// Pascals.
    Pressure,
    Voltage,
    Current,
    /// 0..1 on the wire, shown as a percentage.
    Ratio,
    /// Seconds.
    Duration,
    /// A count, shown as-is.
    Count,
}

/// Which units the display prefers. Depth and speed are the ones crews
/// genuinely disagree about; the rest have a settled convention at sea.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct UnitPrefs {
    pub depth: DepthUnit,
    pub speed: SpeedUnit,
    pub temperature: TemperatureUnit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum DepthUnit {
    #[default]
    Metres,
    Feet,
    Fathoms,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum SpeedUnit {
    #[default]
    Knots,
    Kilometres,
    MilesPerHour,
    MetresPerSecond,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum TemperatureUnit {
    #[default]
    Celsius,
    Fahrenheit,
}

impl DepthUnit {
    pub fn label(self) -> &'static str {
        match self {
            DepthUnit::Metres => "m",
            DepthUnit::Feet => "ft",
            DepthUnit::Fathoms => "fm",
        }
    }
    fn from_metres(self, m: f64) -> f64 {
        match self {
            DepthUnit::Metres => m,
            DepthUnit::Feet => m * 3.280_839_895,
            DepthUnit::Fathoms => m * 0.546_806_649,
        }
    }
}

impl SpeedUnit {
    pub fn label(self) -> &'static str {
        match self {
            SpeedUnit::Knots => "kn",
            SpeedUnit::Kilometres => "km/h",
            SpeedUnit::MilesPerHour => "mph",
            SpeedUnit::MetresPerSecond => "m/s",
        }
    }
    fn from_mps(self, v: f64) -> f64 {
        match self {
            // Exactly 1852 m per nautical mile, by definition.
            SpeedUnit::Knots => v * 3600.0 / 1852.0,
            SpeedUnit::Kilometres => v * 3.6,
            SpeedUnit::MilesPerHour => v * 3600.0 / 1609.344,
            SpeedUnit::MetresPerSecond => v,
        }
    }
}

impl TemperatureUnit {
    pub fn label(self) -> &'static str {
        match self {
            TemperatureUnit::Celsius => "°C",
            TemperatureUnit::Fahrenheit => "°F",
        }
    }
    fn from_kelvin(self, k: f64) -> f64 {
        match self {
            TemperatureUnit::Celsius => k - 273.15,
            TemperatureUnit::Fahrenheit => (k - 273.15) * 9.0 / 5.0 + 32.0,
        }
    }
}

/// A value ready to draw: the number and its unit, kept apart so the two can
/// be set in different sizes.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub value: String,
    pub unit: &'static str,
}

impl Quantity {
    /// Render an SI value for display.
    pub fn format(self, si: f64, prefs: &UnitPrefs) -> Reading {
        match self {
            Quantity::Speed => {
                let v = prefs.speed.from_mps(si);
                Reading {
                    // A tenth of a knot is the finest reading anyone steers to.
                    value: format!("{v:.1}"),
                    unit: prefs.speed.label(),
                }
            }
            Quantity::Angle => {
                let deg = si.to_degrees().rem_euclid(360.0);
                Reading {
                    // Whole degrees, zero-padded: a heading swinging between
                    // 9° and 351° must not change width and jitter the layout.
                    value: format!("{:03.0}", deg.round() % 360.0),
                    unit: "°",
                }
            }
            Quantity::RelativeAngle => {
                let mut deg = si.to_degrees() % 360.0;
                if deg > 180.0 {
                    deg -= 360.0;
                } else if deg < -180.0 {
                    deg += 360.0;
                }
                // Port and starboard rather than a minus sign, which is easy
                // to miss at a glance and ambiguous on a heeling boat.
                let side = if deg.abs() < 0.5 {
                    ""
                } else if deg < 0.0 {
                    "P"
                } else {
                    "S"
                };
                Reading {
                    value: format!("{:.0}{}", deg.abs().round(), side),
                    unit: "°",
                }
            }
            Quantity::Depth => {
                let v = prefs.depth.from_metres(si);
                Reading {
                    value: format!("{v:.1}"),
                    unit: prefs.depth.label(),
                }
            }
            Quantity::Distance => {
                let nm = si / 1852.0;
                Reading {
                    value: if nm < 10.0 {
                        format!("{nm:.2}")
                    } else {
                        format!("{nm:.1}")
                    },
                    unit: "NM",
                }
            }
            Quantity::Temperature => Reading {
                value: format!("{:.1}", prefs.temperature.from_kelvin(si)),
                unit: prefs.temperature.label(),
            },
            Quantity::Pressure => Reading {
                // Hectopascals, which are millibars, which is what a barometer
                // has always been read in.
                value: format!("{:.0}", si / 100.0),
                unit: "hPa",
            },
            Quantity::Voltage => Reading {
                value: format!("{si:.2}"),
                unit: "V",
            },
            Quantity::Current => Reading {
                value: format!("{si:.1}"),
                unit: "A",
            },
            Quantity::Ratio => Reading {
                value: format!("{:.0}", si * 100.0),
                unit: "%",
            },
            Quantity::Duration => {
                let total = si.max(0.0) as u64;
                let (h, m) = (total / 3600, (total % 3600) / 60);
                Reading {
                    value: format!("{h}:{m:02}"),
                    unit: "h",
                }
            }
            Quantity::Count => Reading {
                value: format!("{si:.0}"),
                unit: "",
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefs() -> UnitPrefs {
        UnitPrefs::default()
    }

    #[test]
    fn speed_converts_from_metres_per_second() {
        // The demo server's own reading: 3.42 m/s is 6.6 knots, not 3.4.
        assert_eq!(Quantity::Speed.format(3.42, &prefs()).value, "6.6");
        assert_eq!(Quantity::Speed.format(3.42, &prefs()).unit, "kn");
        let kmh = UnitPrefs {
            speed: SpeedUnit::Kilometres,
            ..Default::default()
        };
        assert_eq!(Quantity::Speed.format(10.0, &kmh).value, "36.0");
        // One knot is exactly 1852 m/h.
        assert_eq!(Quantity::Speed.format(1852.0 / 3600.0, &prefs()).value, "1.0");
    }

    #[test]
    fn headings_are_degrees_and_keep_their_width() {
        // 3.475 rad from the demo = 199°.
        assert_eq!(Quantity::Angle.format(3.475, &prefs()).value, "199");
        // Zero-padded so the layout cannot shift as the boat swings.
        assert_eq!(Quantity::Angle.format(0.157, &prefs()).value, "009");
        assert_eq!(Quantity::Angle.format(0.0, &prefs()).value, "000");
        // Wraps rather than reading 360.
        assert_eq!(Quantity::Angle.format(std::f64::consts::TAU, &prefs()).value, "000");
        // Negative radians still land on a compass bearing.
        assert_eq!(Quantity::Angle.format(-0.5, &prefs()).value, "331");
    }

    #[test]
    fn relative_angles_read_port_and_starboard() {
        // 0.6635 rad apparent wind from the demo = 38° to starboard.
        assert_eq!(Quantity::RelativeAngle.format(0.6635, &prefs()).value, "38S");
        assert_eq!(Quantity::RelativeAngle.format(-0.6635, &prefs()).value, "38P");
        // Dead ahead has no side.
        assert_eq!(Quantity::RelativeAngle.format(0.0, &prefs()).value, "0");
        // Past 180° it comes back round the other side, not to 350S.
        assert_eq!(
            Quantity::RelativeAngle.format(350f64.to_radians(), &prefs()).value,
            "10P"
        );
    }

    #[test]
    fn depth_and_temperature_convert() {
        assert_eq!(Quantity::Depth.format(32.75, &prefs()).value, "32.8");
        let feet = UnitPrefs {
            depth: DepthUnit::Feet,
            ..Default::default()
        };
        assert_eq!(Quantity::Depth.format(10.0, &feet).value, "32.8");
        // The demo's water temperature is 313.15 K — 40 °C, not 313.
        assert_eq!(Quantity::Temperature.format(313.15, &prefs()).value, "40.0");
        let f = UnitPrefs {
            temperature: TemperatureUnit::Fahrenheit,
            ..Default::default()
        };
        assert_eq!(Quantity::Temperature.format(273.15, &f).value, "32.0");
    }

    #[test]
    fn the_remaining_quantities_read_sensibly() {
        assert_eq!(Quantity::Pressure.format(101_325.0, &prefs()).value, "1013");
        assert_eq!(Quantity::Ratio.format(0.62, &prefs()).value, "62");
        assert_eq!(Quantity::Distance.format(1852.0, &prefs()).value, "1.00");
        assert_eq!(Quantity::Distance.format(185_200.0, &prefs()).value, "100.0");
        assert_eq!(Quantity::Duration.format(3720.0, &prefs()).value, "1:02");
        assert_eq!(Quantity::Voltage.format(14.18, &prefs()).value, "14.18");
    }
}
