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

/// The signed difference `a − b` in degrees, in (−180, 180]. One
/// `wrapping_sub` and a reinterpretation — no branches, no epsilon.
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
}
