//! The boat's performance: polar diagram, VMG tables, penalties.
//!
//! §5 of the routing spec, with its two rules kept faithfully:
//!
//! - **The no-go zone is not hardcoded** (§5.4). The polar simply has no valid
//!   data below its lowest TWA row; lookup returns `None` there and the
//!   engine's candidate set always carries the VMG headings, so the search
//!   *discovers* that tacking beats pointing. No magic dead angle anywhere.
//! - **Resample once at load** (§5.2) onto a regular 181×121 grid — not for
//!   speed, but because indexing a regular grid is less code than binary
//!   search over an irregular one, and less code is fewer bugs.
//!
//! Out-of-range wind (decision, was spec §10.1): above the highest TWS band
//! the lookup steps down until it finds a valid cell — OpenCPN's behaviour —
//! which on the regular grid is a clamp to the top band. Below the lowest
//! band, speeds scale linearly toward zero: drifting in 2 kt of air is slow,
//! not forbidden.

/// TWA rows: 0..=180 degrees, one per degree.
const N_TWA: usize = 181;
/// TWS columns: 0..=60 kt, one per half knot.
const N_TWS: usize = 121;
const TWS_STEP: f64 = 0.5;

/// A boat's polar, resampled and with its derived tables built.
pub struct Polar {
    /// Boat speed in knots, row-major `[twa][tws]`. `NAN` = no data (no-go).
    grid: Vec<f32>,
    /// Lowest TWA the source polar spoke for — everything below is no-go.
    pub min_twa_deg: f64,
    /// Highest TWS band the source covered; above it lookups clamp.
    pub max_tws_kt: f64,
    /// Lowest TWS band; below it speeds scale toward zero.
    pub min_tws_kt: f64,
    /// Per TWS bin: the best beat and run, precomputed (§5.3).
    pub vmg: Vec<VmgEntry>,
}

/// The best upwind and downwind working angles at one wind speed.
#[derive(Debug, Clone, Copy, Default)]
pub struct VmgEntry {
    pub beat_twa_deg: f32,
    pub beat_vmg_kt: f32,
    pub beat_stw_kt: f32,
    pub run_twa_deg: f32,
    pub run_vmg_kt: f32,
    pub run_stw_kt: f32,
}

#[derive(Debug)]
pub enum PolarError {
    Empty,
    Malformed(String),
}

impl std::fmt::Display for PolarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolarError::Empty => write!(f, "the polar file holds no usable rows"),
            PolarError::Malformed(e) => write!(f, "unreadable polar: {e}"),
        }
    }
}

impl std::error::Error for PolarError {}

impl Polar {
    /// Parse the standard grid format: first row TWS headers, first column
    /// TWA, cells boat speed in knots. Delimiter — tab, semicolon, comma or
    /// runs of spaces — is detected, not configured (§5.1).
    pub fn parse(text: &str) -> Result<Self, PolarError> {
        let mut rows: Vec<(f64, Vec<(f64, f64)>)> = Vec::new(); // twa -> [(tws, stw)]
        let mut header: Option<Vec<f64>> = None;

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
                continue;
            }
            let fields = split_fields(line);
            if fields.is_empty() {
                continue;
            }
            if header.is_none() {
                // The first cell is a label ("TWA", "twa/tws", empty…); the
                // rest must be numbers.
                let tws: Vec<f64> = fields[1..]
                    .iter()
                    .filter_map(|f| f.parse().ok())
                    .collect();
                if tws.len() != fields.len() - 1 || tws.is_empty() {
                    return Err(PolarError::Malformed(format!(
                        "header row not TWS values: {line:?}"
                    )));
                }
                header = Some(tws);
                continue;
            }
            let tws_cols = header.as_ref().unwrap();
            let twa: f64 = fields[0]
                .parse()
                .map_err(|_| PolarError::Malformed(format!("row without TWA: {line:?}")))?;
            let speeds: Vec<(f64, f64)> = fields[1..]
                .iter()
                .zip(tws_cols)
                .filter_map(|(f, &tws)| f.parse::<f64>().ok().map(|s| (tws, s)))
                .collect();
            if !speeds.is_empty() {
                rows.push((twa, speeds));
            }
        }
        let Some(header) = header else {
            return Err(PolarError::Empty);
        };
        if rows.is_empty() {
            return Err(PolarError::Empty);
        }
        rows.sort_by(|a, b| a.0.total_cmp(&b.0));

        let min_twa_deg = rows.first().map(|r| r.0).unwrap_or(0.0).max(0.0);
        let min_tws_kt = header.iter().cloned().fold(f64::MAX, f64::min);
        let max_tws_kt = header.iter().cloned().fold(f64::MIN, f64::max);

        // Bilinear resample onto the regular grid. Outside the source TWA
        // range the cell is NAN — that IS the no-go zone. Outside the source
        // TWS range the cell clamps to the edge band; the low-wind scaling
        // happens at lookup so the stored band stays the source's own.
        let mut grid = vec![f32::NAN; N_TWA * N_TWS];
        for (ti, cell) in grid.iter_mut().enumerate() {
            let twa = (ti / N_TWS) as f64;
            let tws = (ti % N_TWS) as f64 * TWS_STEP;
            if twa + 1e-9 < min_twa_deg {
                continue;
            }
            *cell = sample(&rows, &header, twa, tws) as f32;
        }

        let mut polar = Self {
            grid,
            min_twa_deg,
            max_tws_kt,
            min_tws_kt,
            vmg: Vec::new(),
        };
        polar.build_vmg();
        Ok(polar)
    }

    fn build_vmg(&mut self) {
        self.vmg = (0..N_TWS)
            .map(|j| {
                let mut e = VmgEntry::default();
                for twa in 0..N_TWA {
                    let stw = self.grid[twa * N_TWS + j];
                    if !stw.is_finite() || stw <= 0.0 {
                        continue;
                    }
                    let vmg = stw * (twa as f32).to_radians().cos();
                    if vmg > e.beat_vmg_kt {
                        e.beat_vmg_kt = vmg;
                        e.beat_twa_deg = twa as f32;
                        e.beat_stw_kt = stw;
                    }
                    if -vmg > e.run_vmg_kt {
                        e.run_vmg_kt = -vmg;
                        e.run_twa_deg = twa as f32;
                        e.run_stw_kt = stw;
                    }
                }
                e
            })
            .collect();
    }

    /// Boat speed for an absolute TWA in degrees (0..=180) and TWS in knots.
    /// `None` in the no-go zone or where the polar is silent.
    pub fn speed_kt(&self, twa_deg: f64, tws_kt: f64) -> Option<f64> {
        let twa = twa_deg.abs();
        if !(0.0..=180.0).contains(&twa) || twa + 1e-9 < self.min_twa_deg {
            return None;
        }
        // Above the top band: clamp (the step-down decision); below the
        // bottom band: scale toward zero.
        let (tws_eff, low_scale) = if tws_kt >= self.min_tws_kt {
            (tws_kt.min(self.max_tws_kt), 1.0)
        } else {
            (self.min_tws_kt, (tws_kt / self.min_tws_kt).max(0.0))
        };
        let ti = (twa.round() as usize).min(N_TWA - 1);
        let tj = ((tws_eff / TWS_STEP).round() as usize).min(N_TWS - 1);
        let v = self.grid[ti * N_TWS + tj];
        (v.is_finite() && v > 0.0).then(|| v as f64 * low_scale)
    }

    /// The VMG entry for a wind speed, edge-clamped like the grid.
    pub fn vmg_at(&self, tws_kt: f64) -> &VmgEntry {
        let tws = tws_kt.clamp(0.0, self.max_tws_kt);
        let j = ((tws / TWS_STEP).round() as usize).min(self.vmg.len() - 1);
        &self.vmg[j]
    }
}

/// Bilinear sample of the irregular source grid, clamped at its edges.
fn sample(rows: &[(f64, Vec<(f64, f64)>)], header: &[f64], twa: f64, tws: f64) -> f64 {
    // TWA rows bracketing.
    let (r0, r1) = bracket(rows.iter().map(|r| r.0), twa);
    let interp_row = |ri: usize| -> f64 {
        let (_, cells) = &rows[ri];
        let (c0, c1) = bracket(cells.iter().map(|c| c.0), tws);
        let (t0, s0) = cells[c0];
        let (t1, s1) = cells[c1];
        if (t1 - t0).abs() < 1e-9 {
            s0
        } else {
            let f = ((tws - t0) / (t1 - t0)).clamp(0.0, 1.0);
            s0 + (s1 - s0) * f
        }
    };
    let v0 = interp_row(r0);
    let v1 = interp_row(r1);
    let (a0, a1) = (rows[r0].0, rows[r1].0);
    let _ = header;
    if (a1 - a0).abs() < 1e-9 {
        v0
    } else {
        let f = ((twa - a0) / (a1 - a0)).clamp(0.0, 1.0);
        v0 + (v1 - v0) * f
    }
}

/// Indices of the two entries bracketing `x` in an ascending sequence,
/// clamped to the ends.
fn bracket(values: impl Iterator<Item = f64>, x: f64) -> (usize, usize) {
    let vals: Vec<f64> = values.collect();
    if vals.len() == 1 {
        return (0, 0);
    }
    for i in 0..vals.len() - 1 {
        if x <= vals[i + 1] {
            return (i, i + 1);
        }
    }
    (vals.len() - 2, vals.len() - 1)
}

fn split_fields(line: &str) -> Vec<&str> {
    for delim in [';', '\t', ','] {
        if line.contains(delim) {
            return line.split(delim).map(str::trim).filter(|s| !s.is_empty()).collect();
        }
    }
    line.split_whitespace().collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A tidy cruising polar: no-go below 45°, best speed on the beam.
    pub(crate) const TEST_POLAR: &str = "\
twa/tws\t6\t10\t14\t20
45\t3.4\t5.0\t5.8\t6.0
60\t4.0\t5.8\t6.6\t6.9
90\t4.5\t6.0\t7.0\t7.5
120\t4.2\t6.0\t7.2\t7.9
150\t3.2\t5.2\t6.4\t7.2
180\t2.6\t4.4\t5.6\t6.6
";

    #[test]
    fn the_grid_reproduces_the_source_points_and_interpolates_between() {
        let p = Polar::parse(TEST_POLAR).unwrap();
        // Exact source cells come back exactly.
        assert!((p.speed_kt(90.0, 10.0).unwrap() - 6.0).abs() < 1e-6);
        assert!((p.speed_kt(180.0, 20.0).unwrap() - 6.6).abs() < 1e-6);
        // Between rows and columns, between the neighbours.
        let mid = p.speed_kt(75.0, 12.0).unwrap();
        assert!((5.8..7.0).contains(&mid), "{mid}");
    }

    #[test]
    fn the_no_go_zone_is_the_absence_of_data() {
        let p = Polar::parse(TEST_POLAR).unwrap();
        assert_eq!(p.speed_kt(30.0, 10.0), None, "inside the no-go");
        assert_eq!(p.speed_kt(44.0, 10.0), None, "just inside");
        assert!(p.speed_kt(45.0, 10.0).is_some(), "the edge itself sails");
        assert!((p.min_twa_deg - 45.0).abs() < 1e-9);
    }

    #[test]
    fn out_of_range_wind_follows_the_decisions() {
        let p = Polar::parse(TEST_POLAR).unwrap();
        // Above the top band: clamped — 30 kt sails like 20 kt.
        assert_eq!(p.speed_kt(90.0, 30.0), p.speed_kt(90.0, 20.0));
        // Below the bottom band: scaled toward zero, reaching zero at calm.
        let at_3 = p.speed_kt(90.0, 3.0).unwrap();
        let at_6 = p.speed_kt(90.0, 6.0).unwrap();
        assert!((at_3 - at_6 * 0.5).abs() < 1e-6, "{at_3} vs half of {at_6}");
        assert!(p.speed_kt(90.0, 0.0).unwrap_or(0.0) < 1e-9);
    }

    #[test]
    fn vmg_tables_find_the_working_angles() {
        let p = Polar::parse(TEST_POLAR).unwrap();
        let v = p.vmg_at(10.0);
        // Beating: the best cos-weighted speed is at the lowest sailable
        // angle for this shape.
        assert!((44.0..=50.0).contains(&v.beat_twa_deg), "{}", v.beat_twa_deg);
        assert!(v.beat_vmg_kt > 3.0 && v.beat_vmg_kt < v.beat_stw_kt);
        // Running: broad, not dead down — 150° beats 180° here.
        assert!((135.0..=165.0).contains(&v.run_twa_deg), "{}", v.run_twa_deg);
        assert!(v.run_vmg_kt > 4.0, "{}", v.run_vmg_kt);
    }

    #[test]
    fn delimiters_are_detected_not_configured() {
        let semis = TEST_POLAR.replace('\t', ";");
        let commas = TEST_POLAR.replace('\t', ",");
        let spaces = TEST_POLAR.replace('\t', "   ");
        for text in [semis, commas, spaces] {
            let p = Polar::parse(&text).unwrap();
            assert!((p.speed_kt(90.0, 10.0).unwrap() - 6.0).abs() < 1e-6);
        }
    }

    #[test]
    fn junk_is_an_error_not_a_polar() {
        assert!(Polar::parse("").is_err());
        assert!(Polar::parse("just some words\nnot a polar\n").is_err());
        // A header alone has nothing to sail on.
        assert!(Polar::parse("twa\t6\t10\n").is_err());
    }
}
