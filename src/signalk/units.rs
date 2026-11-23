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
    /// Revolutions per second (Hz) on the wire, as Signal K sends engine
    /// and shaft speed. Shown per minute.
    Revolutions,
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
    pub fn from_metres(self, m: f64) -> f64 {
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
                // Wrapped into [-180, 180): +180° and −180° are one direction,
                // dead astern, and both must land on the same value.
                let mut deg = si.to_degrees() % 360.0;
                if deg >= 180.0 {
                    deg -= 360.0;
                } else if deg < -180.0 {
                    deg += 360.0;
                }
                // Port and starboard rather than a minus sign, which is easy
                // to miss at a glance and ambiguous on a heeling boat. Dead
                // ahead and dead astern have no side.
                let side = if deg.abs() < 0.5 || 180.0 - deg.abs() < 0.5 {
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
            Quantity::Revolutions => Reading {
                value: format!("{:.0}", si * 60.0),
                unit: "",
            },
            Quantity::Count => Reading {
                value: plain_number(si),
                unit: "",
            },
        }
    }
}

/// A number of unknown kind, with the digits it has: whole numbers whole,
/// and a fraction shown rather than rounded away — an HDOP of 1.4 is not 1,
/// and a path Manx does not know reading 0.37 is not 0. Three figures at
/// most; nobody reads more off an instrument tile.
fn plain_number(v: f64) -> String {
    let decimals = if v.abs() < 10.0 {
        2
    } else if v.abs() < 100.0 {
        1
    } else {
        0
    };
    let text = format!("{v:.decimals$}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text
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

    /// The factors themselves, against the definitions rather than against
    /// the code's own arithmetic. A tenth-of-a-unit reading near ten hides a
    /// factor that is off in the third figure — 3.28 for feet, 1.94 for knots
    /// — so these use values big enough that such an error shows.
    #[test]
    fn conversion_factors_match_their_definitions() {
        let with = |speed, depth, temperature| UnitPrefs { speed, depth, temperature };
        let p = with(SpeedUnit::Knots, DepthUnit::Metres, TemperatureUnit::Celsius);
        // 1 kn = 1852 m/h exactly, so 1000 m/s = 1943.844… kn.
        assert_eq!(Quantity::Speed.format(1000.0, &p).value, "1943.8");
        let mph = with(SpeedUnit::MilesPerHour, DepthUnit::Metres, TemperatureUnit::Celsius);
        // 1 mi = 1609.344 m exactly, so 1000 m/s = 2236.936… mph.
        assert_eq!(Quantity::Speed.format(1000.0, &mph).value, "2236.9");
        assert_eq!(Quantity::Speed.format(1000.0, &mph).unit, "mph");
        let ms = with(SpeedUnit::MetresPerSecond, DepthUnit::Metres, TemperatureUnit::Celsius);
        assert_eq!(Quantity::Speed.format(3.42, &ms).value, "3.4");
        assert_eq!(Quantity::Speed.format(3.42, &ms).unit, "m/s");

        // 1 ft = 0.3048 m and 1 fathom = 6 ft = 1.8288 m, both exactly.
        let ft = with(SpeedUnit::Knots, DepthUnit::Feet, TemperatureUnit::Celsius);
        assert_eq!(Quantity::Depth.format(1000.0, &ft).value, "3280.8");
        assert_eq!(Quantity::Depth.format(1000.0, &ft).unit, "ft");
        let fm = with(SpeedUnit::Knots, DepthUnit::Fathoms, TemperatureUnit::Celsius);
        assert_eq!(Quantity::Depth.format(1000.0, &fm).value, "546.8");
        assert_eq!(Quantity::Depth.format(18.288, &fm).value, "10.0");
        assert_eq!(Quantity::Depth.format(1.0, &fm).unit, "fm");

        // Fahrenheit at two points, because a scale that is right at freezing
        // can still have its slope upside down (5/9 for 9/5 also gives 32).
        let f = with(SpeedUnit::Knots, DepthUnit::Metres, TemperatureUnit::Fahrenheit);
        assert_eq!(Quantity::Temperature.format(373.15, &f).value, "212.0");
        assert_eq!(Quantity::Temperature.format(233.15, &f).value, "-40.0");
        assert_eq!(Quantity::Temperature.format(233.15, &f).unit, "°F");
    }

    #[test]
    fn relative_angles_wrap_from_either_side() {
        // A wind angle reported as -350° is 10° to starboard, not 350P.
        assert_eq!(
            Quantity::RelativeAngle.format((-350f64).to_radians(), &prefs()).value,
            "10S"
        );
        // Under half a degree either way reads as dead ahead, with no side
        // flickering between P and S.
        assert_eq!(Quantity::RelativeAngle.format((-0.4f64).to_radians(), &prefs()).value, "0");
        // Bearings near 360 round to 000, never to "360".
        assert_eq!(Quantity::Angle.format(359.6f64.to_radians(), &prefs()).value, "000");
    }

    #[test]
    fn the_remaining_quantities_read_sensibly() {
        assert_eq!(Quantity::Pressure.format(101_325.0, &prefs()).value, "1013");
        assert_eq!(Quantity::Ratio.format(0.62, &prefs()).value, "62");
        assert_eq!(Quantity::Distance.format(1852.0, &prefs()).value, "1.00");
        assert_eq!(Quantity::Distance.format(185_200.0, &prefs()).value, "100.0");
        assert_eq!(Quantity::Duration.format(3720.0, &prefs()).value, "1:02");
        assert_eq!(Quantity::Voltage.format(14.18, &prefs()).value, "14.18");
        assert_eq!(Quantity::Current.format(-3.27, &prefs()).value, "-3.3");
        assert_eq!(Quantity::Count.format(11.0, &prefs()).value, "11");
        // Hours and minutes, not days: a 26-hour passage reads 26:00. A
        // negative duration (a clock that stepped back) reads zero.
        assert_eq!(Quantity::Duration.format(26.0 * 3600.0, &prefs()).value, "26:00");
        assert_eq!(Quantity::Duration.format(-5.0, &prefs()).value, "0:00");
    }

    #[test]
    fn a_bare_number_keeps_its_fraction() {
        let show = |v: f64| Quantity::Count.format(v, &prefs()).value;
        assert_eq!(show(11.0), "11");
        assert_eq!(show(1.4), "1.4");
        assert_eq!(show(0.37), "0.37");
        assert_eq!(show(12.34), "12.3");
        assert_eq!(show(1500.4), "1500");
        assert_eq!(show(0.0), "0");
        assert_eq!(show(-0.5), "-0.5");
    }

    /// Signal K gives `propulsion.*.revolutions` in Hz: an engine turning
    /// 1500 times a minute arrives as 25.
    #[test]
    fn engine_speed_arrives_per_second_and_reads_per_minute() {
        assert_eq!(Quantity::Revolutions.format(25.0, &prefs()).value, "1500");
        assert_eq!(Quantity::Revolutions.format(13.5, &prefs()).value, "810");
    }

    /// Distance keeps three figures: 9.99 NM, then 10.0 NM — not 10.00,
    /// which is four figures and a character wider.
    #[test]
    fn ten_miles_reads_with_one_decimal() {
        assert_eq!(Quantity::Distance.format(18_520.0, &prefs()).value, "10.0");
        assert_eq!(Quantity::Distance.format(18_500.0, &prefs()).value, "9.99");
    }

    /// "Under half a degree either way reads as dead ahead" — exactly half
    /// a degree is not under it, rounds to 1°, and has a side.
    #[test]
    fn half_a_degree_off_the_bow_has_a_side() {
        // The radian value whose degrees are exactly 0.5.
        let base = 0.5f64.to_radians().to_bits();
        let si = (0..64)
            .map(|k| f64::from_bits(base - 32 + k))
            .find(|r| r.to_degrees() == 0.5)
            .expect("some f64 is exactly half a degree");
        assert_eq!(Quantity::RelativeAngle.format(si, &prefs()).value, "1S");
        assert_eq!(Quantity::RelativeAngle.format(-si, &prefs()).value, "1P");
    }

    /// +π and −π on the wire are the same direction, dead astern, and read
    /// the same — as "180", with no side, just as dead ahead reads "0".
    #[test]
    fn dead_astern_reads_the_same_from_either_side() {
        let pi = std::f64::consts::PI;
        assert_eq!(Quantity::RelativeAngle.format(pi, &prefs()).value, "180");
        assert_eq!(Quantity::RelativeAngle.format(-pi, &prefs()).value, "180");
        // A degree off dead astern is still on a side, as a degree off the
        // bow is.
        assert_eq!(Quantity::RelativeAngle.format(179f64.to_radians(), &prefs()).value, "179S");
        assert_eq!(Quantity::RelativeAngle.format((-179f64).to_radians(), &prefs()).value, "179P");
    }

    /// Half a degree short of dead astern has a side, as half a degree off the
    /// bow does: the same boundary, at the other end of the scale.
    #[test]
    fn half_a_degree_short_of_dead_astern_has_a_side() {
        let base = 179.5f64.to_radians().to_bits();
        let si = (0..64)
            .map(|k| f64::from_bits(base - 32 + k))
            .find(|r| r.to_degrees() == 179.5)
            .expect("some f64 is exactly 179.5 degrees");
        assert_eq!(Quantity::RelativeAngle.format(si, &prefs()).value, "180S");
        assert_eq!(Quantity::RelativeAngle.format(-si, &prefs()).value, "180P");
    }

    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Each speed unit converts back by its definition: 1 kn = 1852 m
        /// per hour, 1 km/h = 1000 m per hour, 1 mph = 1609.344 m per hour.
        #[test]
        fn every_speed_unit_converts_back_to_metres_per_second(v in -200.0f64..200.0) {
            let tol = 1e-12 * v.abs().max(1.0);
            prop_assert!((SpeedUnit::Knots.from_mps(v) * 1852.0 / 3600.0 - v).abs() <= tol);
            prop_assert!((SpeedUnit::Kilometres.from_mps(v) * 1000.0 / 3600.0 - v).abs() <= tol);
            prop_assert!((SpeedUnit::MilesPerHour.from_mps(v) * 1609.344 / 3600.0 - v).abs() <= tol);
            prop_assert_eq!(SpeedUnit::MetresPerSecond.from_mps(v), v);
        }

        /// 1 ft = 0.3048 m and 1 fathom = 1.8288 m, exactly; the factors in
        /// the code are given to ten figures.
        #[test]
        fn depths_convert_back_to_metres(m in 0.0f64..12_000.0) {
            let tol = 1e-9 * m.max(1.0);
            prop_assert!((DepthUnit::Feet.from_metres(m) * 0.3048 - m).abs() <= tol);
            prop_assert!((DepthUnit::Fathoms.from_metres(m) * 1.8288 - m).abs() <= tol);
            prop_assert_eq!(DepthUnit::Metres.from_metres(m), m);
        }

        /// 0 °C = 273.15 K; a Fahrenheit degree is 5/9 of a kelvin, from 32 °F.
        #[test]
        fn temperatures_convert_back_to_kelvin(k in 0.0f64..1_000.0) {
            let c = TemperatureUnit::Celsius.from_kelvin(k);
            prop_assert!((c + 273.15 - k).abs() < 1e-9);
            let f = TemperatureUnit::Fahrenheit.from_kelvin(k);
            prop_assert!(((f - 32.0) * 5.0 / 9.0 + 273.15 - k).abs() < 1e-9);
        }

        /// What the display shows reads back to the wire value within half
        /// its last digit: a tenth of a knot, a whole degree.
        #[test]
        fn a_displayed_reading_converts_back_within_its_last_digit(
            mps in 0.0f64..50.0,
            deg in -720.0f64..720.0,
        ) {
            let kn: f64 = Quantity::Speed.format(mps, &prefs()).value.parse().unwrap();
            prop_assert!((kn * 1852.0 / 3600.0 - mps).abs() <= 0.05 * 1852.0 / 3600.0 + 1e-12);
            let shown: f64 = Quantity::Angle.format(deg.to_radians(), &prefs()).value.parse().unwrap();
            prop_assert!((0.0..360.0).contains(&shown));
            let off = (shown - deg).rem_euclid(360.0);
            prop_assert!(off.min(360.0 - off) <= 0.5 + 1e-9, "{} shown for {}", shown, deg);
        }
    }
}
