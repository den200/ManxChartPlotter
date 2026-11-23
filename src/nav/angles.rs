//! Binary angles: the u16 where 65536 is a full circle.
//!
//! §7.4's correctness measure. Every 0/360 wraparound bug dies by
//! construction because wraparound *is* u16 arithmetic: `heading − wind` is
//! `wrapping_sub`, and the sign of the difference is the sign of the `i16`
//! reinterpretation. Resolution is 360/65536 ≈ 0.0055°, two orders finer than
//! the polar grid cares about.

pub type Bam = u16;

pub fn deg_to_bam(deg: f64) -> Bam {
    ((deg.rem_euclid(360.0) / 360.0) * 65536.0).round() as u32 as u16
}

pub fn bam_to_deg(bam: Bam) -> f64 {
    bam as f64 * 360.0 / 65536.0
}

/// The signed difference `a − b` in degrees, in [−180, 180). One
/// `wrapping_sub` and a reinterpretation — no branches, no epsilon. Exactly
/// opposite reads −180: a dead run has no natural side, and the two's-
/// complement reading is the one that costs nothing.
pub fn signed_diff_deg(a: Bam, b: Bam) -> f64 {
    (a.wrapping_sub(b) as i16) as f64 * 360.0 / 65536.0
}

/// Radians clockwise from north, for building step vectors.
pub fn bam_to_rad(bam: Bam) -> f64 {
    bam_to_deg(bam).to_radians()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_wraps() {
        for deg in [0.0, 45.0, 90.0, 179.9, 180.0, 270.0, 359.9] {
            let back = bam_to_deg(deg_to_bam(deg));
            assert!((back - deg).abs() < 0.01, "{deg} -> {back}");
        }
        // 361° is 1°.
        assert!((bam_to_deg(deg_to_bam(361.0)) - 1.0).abs() < 0.01);
        // −10° is 350°.
        assert!((bam_to_deg(deg_to_bam(-10.0)) - 350.0).abs() < 0.01);
        // A hair under 360° rounds to a full circle, which is north — not a
        // saturated 65535 a hair west of it.
        assert_eq!(deg_to_bam(359.999), 0);
        assert_eq!(deg_to_bam(360.0), 0);
    }

    /// Step vectors are built as (sin, cos) of this: east is a quarter turn
    /// clockwise from north, in radians.
    #[test]
    fn radians_run_clockwise_from_north() {
        assert!(bam_to_rad(deg_to_bam(0.0)).abs() < 1e-12);
        assert!((bam_to_rad(deg_to_bam(90.0)) - std::f64::consts::FRAC_PI_2).abs() < 1e-4);
        assert!((bam_to_rad(deg_to_bam(270.0)) - 1.5 * std::f64::consts::PI).abs() < 1e-4);
    }

    /// The class of bug this type exists to kill: differences across north.
    #[test]
    fn differences_across_north_are_small_not_359() {
        let a = deg_to_bam(5.0);
        let b = deg_to_bam(355.0);
        assert!((signed_diff_deg(a, b) - 10.0).abs() < 0.02);
        assert!((signed_diff_deg(b, a) + 10.0).abs() < 0.02);
        // And the antipode lands on ±180, not on garbage.
        assert!(signed_diff_deg(deg_to_bam(0.0), deg_to_bam(180.0)).abs() >= 179.99);
    }

    /// Exactly opposite reads −180, as documented: [−180, 180). Pinned so
    /// that a caller deciding a side from the sign — port or starboard on
    /// a dead run — sees the same answer every time.
    #[test]
    fn the_antipode_reads_minus_180_as_documented() {
        assert_eq!(signed_diff_deg(0, 32768), -180.0);
        assert_eq!(signed_diff_deg(deg_to_bam(0.0), deg_to_bam(180.0)), -180.0);
        // One step either side of the antipode lands on the expected side:
        // 0 − 179.99° is −179.99°, 0 − 180.01° wraps to +179.99°.
        assert!(signed_diff_deg(0, 32767) < -179.99);
        assert!(signed_diff_deg(0, 32769) > 179.99);
    }

    use proptest::prelude::*;

    /// One step of the binary circle in degrees.
    const STEP: f64 = 360.0 / 65536.0;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn every_bam_is_an_angle_in_one_turn(b in any::<u16>()) {
            let deg = bam_to_deg(b);
            prop_assert!((0.0..360.0).contains(&deg), "{deg}");
            let rad = bam_to_rad(b);
            prop_assert!((0.0..std::f64::consts::TAU).contains(&rad), "{rad}");
        }

        /// Wrapping a wrapped angle changes nothing.
        #[test]
        fn wrapping_is_idempotent(b in any::<u16>(), deg in -1.0e6..1.0e6_f64) {
            prop_assert_eq!(deg_to_bam(bam_to_deg(b)), b);
            let once = deg_to_bam(deg);
            prop_assert_eq!(deg_to_bam(bam_to_deg(once)), once);
            prop_assert_eq!(deg_to_bam(deg.rem_euclid(360.0)), once);
        }

        /// Any angle lands on the nearest step of the circle: within half a
        /// step, measured the short way round.
        #[test]
        fn an_angle_lands_within_half_a_step(deg in -1.0e4..1.0e4_f64) {
            let err = (bam_to_deg(deg_to_bam(deg)) - deg).rem_euclid(360.0);
            let err = err.min(360.0 - err);
            prop_assert!(err <= STEP / 2.0 + 1e-9, "{deg}: off by {err}");
        }

        /// a − b is minus b − a, except at the antipode where both are a half
        /// turn; and stepping b by the difference arrives at a.
        #[test]
        fn differences_are_antisymmetric_and_add_back_up(a in any::<u16>(), b in any::<u16>()) {
            let ab = signed_diff_deg(a, b);
            let ba = signed_diff_deg(b, a);
            prop_assert!((-180.0..=180.0).contains(&ab), "{ab}");
            if a.wrapping_sub(b) == 32768 {
                prop_assert_eq!(ab.abs(), 180.0);
                prop_assert_eq!(ba.abs(), 180.0);
            } else {
                prop_assert!(ab > -180.0 && ab < 180.0, "{ab}");
                prop_assert_eq!(ab, -ba);
            }
            prop_assert_eq!(b.wrapping_add(deg_to_bam(ab)), a);
            prop_assert_eq!(signed_diff_deg(a, a), 0.0);
        }
    }
}
