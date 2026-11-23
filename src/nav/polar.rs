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
                    .filter_map(|f| f.parse::<f64>().ok().filter(|v| v.is_finite()))
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
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .ok_or_else(|| PolarError::Malformed(format!("row without TWA: {line:?}")))?;
            // A cell that is not a finite number is a missing value, like an
            // empty one.
            let speeds: Vec<(f64, f64)> = fields[1..]
                .iter()
                .zip(tws_cols)
                .filter_map(|(f, &tws)| f.parse::<f64>().ok().filter(|s| s.is_finite()).map(|s| (tws, s)))
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
        // A repeated angle or speed has no single answer to read from, so
        // the table is refused rather than read one of the two at random.
        let mut axis = header.clone();
        axis.sort_by(f64::total_cmp);
        if let Some(w) = axis.windows(2).find(|w| w[1] - w[0] < 1e-9) {
            return Err(PolarError::Malformed(format!("wind speed {} given twice", w[1])));
        }
        if let Some(w) = rows.windows(2).find(|w| w[1].0 - w[0].0 < 1e-9) {
            return Err(PolarError::Malformed(format!("TWA {} given twice", w[1].0)));
        }
        // …and every row's wind-speed pairs, for the same reason the angles
        // are sorted: the lookup brackets by a forward scan and assumes an
        // ascending axis. A header written strongest-first used to return the
        // speed for the wrong wind entirely, silently.
        for row in rows.iter_mut() {
            row.1.sort_by(|a, b| a.0.total_cmp(&b.0));
        }

        // The no-go zone ends at the first angle the boat actually sails
        // at. Many polars (OpenCPN's, qtVlm's) open with a row of zeros at
        // 0°: read as a real row it would put the no-go zone at 0° and ramp
        // the speed up from there, so the router could point 25° off the
        // wind at two thirds of the close-hauled speed.
        let min_twa_deg = rows
            .iter()
            .find(|r| r.1.iter().any(|(_, speed)| *speed > 0.0))
            .or(rows.first())
            .map(|r| r.0)
            .unwrap_or(0.0)
            .max(0.0);
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
                // Below the polar's lightest band the grid holds that band's
                // speeds unscaled, and `speed_kt` scales them down at lookup.
                // The VMG table must scale the same way, or a beat in 3 kn of
                // wind is timed at the 6 kn band's speed — twice too fast.
                let tws = j as f64 * TWS_STEP;
                let low_scale = if tws < self.min_tws_kt && self.min_tws_kt > 0.0 {
                    (tws / self.min_tws_kt) as f32
                } else {
                    1.0
                };
                let mut e = VmgEntry::default();
                for twa in 0..N_TWA {
                    let stw = self.grid[twa * N_TWS + j] * low_scale;
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
            // Written as a weighted sum so that past the far end the near
            // value carries weight exactly 0 and the edge value comes out
            // exact, whichever bracketing index was taken.
            let f = ((tws - t0) / (t1 - t0)).clamp(0.0, 1.0);
            s0 * (1.0 - f) + s1 * f
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
        v0 * (1.0 - f) + v1 * f
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
            // An empty cell in the middle is a missing value that keeps its
            // column; only the trailing separators are dropped.
            let mut fields: Vec<&str> = line.split(delim).map(str::trim).collect();
            while fields.last().is_some_and(|s| s.is_empty()) {
                fields.pop();
            }
            return fields;
        }
    }
    line.split_whitespace().collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A header written strongest-first is unusual but legal, and used to
    /// return the boat speed for the wrong wind without a word: the lookup
    /// brackets by a forward scan and assumes the axis ascends.
    #[test]
    fn a_descending_wind_header_still_reads_the_right_column() {
        let ascending = "twa/tws\t6\t10\t14\n90\t4.0\t6.0\t7.0\n";
        let descending = "twa/tws\t14\t10\t6\n90\t7.0\t6.0\t4.0\n";
        let a = Polar::parse(ascending).expect("valid");
        let d = Polar::parse(descending).expect("valid");
        for tws in [6.0, 8.0, 10.0, 12.0, 14.0] {
            let (x, y) = (a.speed_kt(90.0, tws), d.speed_kt(90.0, tws));
            assert_eq!(
                x.map(|v| (v * 100.0).round()),
                y.map(|v| (v * 100.0).round()),
                "column order changed the answer at {tws} kt"
            );
        }
        assert!((d.speed_kt(90.0, 10.0).unwrap() - 6.0).abs() < 1e-6);
    }

    /// A boat does not sail at 0° to the wind, and a table that says so
    /// with a row of zeros means exactly that: below its first sailing
    /// angle there is no speed to read, not a ramp up from nothing.
    #[test]
    fn a_row_of_zeros_at_the_wind_is_not_a_sailing_angle() {
        let p = Polar::parse("twa/tws\t6\t10\n0\t0\t0\n40\t3.0\t5.0\n90\t4.5\t6.0\n").expect("valid");
        assert_eq!(p.speed_kt(25.0, 10.0), None, "25° is inside the no-go zone");
        assert_eq!(p.speed_kt(39.0, 10.0), None);
        assert!((p.speed_kt(40.0, 10.0).unwrap() - 5.0).abs() < 1e-6);
        // The same table without the zero row reads the same.
        let q = Polar::parse("twa/tws\t6\t10\n40\t3.0\t5.0\n90\t4.5\t6.0\n").expect("valid");
        assert_eq!(q.speed_kt(25.0, 10.0), None);
        assert_eq!(p.speed_kt(60.0, 10.0), q.speed_kt(60.0, 10.0));
    }

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
        // Between rows and columns, bilinear: at 12 kt the 60° row reads
        // 6.2 and the 90° row 6.5, so 75° reads half way, 6.35.
        let mid = p.speed_kt(75.0, 12.0).unwrap();
        assert!((mid - 6.35).abs() < 1e-4, "{mid}");
        // Between columns only: 120° at 17 kt is half way from 7.2 to 7.9.
        assert!((p.speed_kt(120.0, 17.0).unwrap() - 7.55).abs() < 1e-4);
        // Port and starboard are the same polar: a negative TWA reads its
        // mirror.
        assert_eq!(p.speed_kt(-75.0, 12.0), p.speed_kt(75.0, 12.0));
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
        // angle for this shape — 5.0 kn at 45°, a VMG of 5·cos 45°.
        assert_eq!(v.beat_twa_deg, 45.0);
        assert!((v.beat_stw_kt - 5.0).abs() < 1e-5);
        assert!((v.beat_vmg_kt as f64 - 5.0 * 45f64.to_radians().cos()).abs() < 1e-4);
        // Running: broad, not dead down — the 150°–180° slope peaks near 163°,
        // well clear of the 4.4 kn a dead run makes.
        assert!((155.0..=170.0).contains(&v.run_twa_deg), "{}", v.run_twa_deg);
        assert!(v.run_vmg_kt > 4.6 && v.run_vmg_kt < 4.7, "{}", v.run_vmg_kt);
    }

    /// Below the lightest band the speeds scale toward zero, and the VMG
    /// table must scale with them: a beat in 3 kn timed at the 6 kn band's
    /// VMG would be twice too fast.
    #[test]
    fn vmg_scales_down_below_the_lightest_band_like_the_speeds() {
        let p = Polar::parse(TEST_POLAR).unwrap();
        let (at_3, at_6) = (p.vmg_at(3.0), p.vmg_at(6.0));
        assert!((at_3.beat_vmg_kt - 0.5 * at_6.beat_vmg_kt).abs() < 1e-4);
        assert!((at_3.run_vmg_kt - 0.5 * at_6.run_vmg_kt).abs() < 1e-4);
        assert!((at_3.beat_stw_kt as f64 - p.speed_kt(at_3.beat_twa_deg as f64, 3.0).unwrap()).abs() < 1e-4);
        // Above the top band the table clamps like the speeds do.
        assert_eq!(p.vmg_at(40.0).beat_vmg_kt, p.vmg_at(20.0).beat_vmg_kt);
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

    /// "The first cell is a label; the rest must be numbers." A header with
    /// a word among its wind speeds is not read as the speeds around it.
    #[test]
    fn a_header_with_a_word_among_its_wind_speeds_is_refused() {
        let e = Polar::parse("twa\t6\tten\t14\n90\t4.0\t5.0\t6.0\n").err().expect("refused");
        assert!(matches!(e, PolarError::Malformed(_)), "{e:?}");
    }

    /// The errors reach the status line, so they must say something — and
    /// a malformed polar must say what was wrong with it.
    #[test]
    fn errors_say_what_went_wrong() {
        assert!(!PolarError::Empty.to_string().is_empty());
        let e = PolarError::Malformed("row without TWA: \"x\"".into()).to_string();
        assert!(e.contains("row without TWA"), "{e:?}");
    }

    /// One wind speed is a polar too: it is the boat at that wind, and at
    /// that wind the table's own numbers come back.
    #[test]
    fn a_single_wind_column_reads_its_own_values() {
        let p = Polar::parse("twa\t10\n45\t5.0\n90\t6.0\n").unwrap();
        assert!((p.speed_kt(90.0, 10.0).unwrap() - 6.0).abs() < 1e-6);
        assert!((p.speed_kt(45.0, 10.0).unwrap() - 5.0).abs() < 1e-6);
        // Between the rows, linearly: 60° is a third of the way to 90°.
        assert!((p.speed_kt(60.0, 10.0).unwrap() - (5.0 + 1.0 / 3.0)).abs() < 1e-5);
    }

    /// Many polar files carry a 0° row of zeros. A boat speed of zero is
    /// the polar saying the boat does not sail there — `None`, as in the
    /// no-go zone — not a speed of 0 kn for the router to time a leg at.
    #[test]
    fn a_zero_in_the_table_is_no_speed_not_a_speed_of_zero() {
        let p = Polar::parse("twa\t6\t10\n0\t0\t0\n45\t4.0\t5.0\n90\t5.0\t6.0\n").unwrap();
        assert_eq!(p.speed_kt(0.0, 10.0), None);
        assert_eq!(p.speed_kt(0.0, 6.0), None);
        assert!((p.speed_kt(90.0, 10.0).unwrap() - 6.0).abs() < 1e-6);
    }

    /// A boat speed below zero is not a boat speed. Here every angle from
    /// 90° to 180° makes 5 kn, so the best run is dead downwind at 5 kn —
    /// not "head to wind at −5 kn", whose negated VMG merely looks like 5.
    #[test]
    fn a_negative_cell_is_not_a_run() {
        let p = Polar::parse("twa\t10\n0\t-5.0\n90\t5.0\n").unwrap();
        let v = p.vmg_at(10.0);
        assert_eq!(v.run_twa_deg, 180.0);
        assert!((v.run_stw_kt - 5.0).abs() < 1e-6, "{}", v.run_stw_kt);
        // Nor is it a sailing angle: the table's first one is 90°, and
        // nothing closer to the wind is read out of the negative cell.
        assert_eq!(p.speed_kt(60.0, 10.0), None);
        assert!((p.speed_kt(90.0, 10.0).unwrap() - 5.0).abs() < 1e-6);
    }

    /// The regular grid stops at 60 kn of wind (`N_TWS`), whatever the
    /// source covers. A polar written up to 80 kn still reads past the end
    /// of the grid without falling off it: as the 60 kn column, edge-clamped,
    /// which lies between the source's own 10 and 80 kn values.
    #[test]
    fn wind_past_the_end_of_the_grid_reads_the_last_column() {
        let p = Polar::parse("twa\t10\t80\n45\t5.0\t12.0\n180\t6.0\t13.0\n").unwrap();
        assert_eq!(p.max_tws_kt, 80.0);
        for twa in [45.0, 90.0, 180.0] {
            let top = p.speed_kt(twa, 60.0).unwrap();
            assert_eq!(p.speed_kt(twa, 70.0), Some(top), "at {twa}°");
            assert_eq!(p.speed_kt(twa, 80.0), Some(top), "at {twa}°");
            assert!((5.0..=13.0).contains(&top), "{top}");
        }
        assert_eq!(p.vmg_at(70.0).beat_vmg_kt, p.vmg_at(60.0).beat_vmg_kt);
        assert_eq!(p.vmg_at(80.0).run_vmg_kt, p.vmg_at(60.0).run_vmg_kt);
    }

    /// In a delimited file a cell's place is its column: `90;4.5;;7.0` says
    /// nothing at 10 kn and 7.0 kn at 14 kn. The empty cell must not slide
    /// the 7.0 left into the 10 kn column.
    #[test]
    fn an_empty_cell_is_a_missing_value_not_a_shift() {
        let p = Polar::parse("twa;6;10;14\n90;4.5;;7.0\n").unwrap();
        assert!((p.speed_kt(90.0, 14.0).unwrap() - 7.0).abs() < 1e-6);
        // Nothing at 10 kn, so half way between 4.5 at 6 kn and 7.0 at 14 kn.
        let at_10 = p.speed_kt(90.0, 10.0).unwrap();
        assert!((at_10 - 5.75).abs() < 1e-6, "{at_10}");
    }

    /// A polar with only the given `(twa row, tws column, speed)` cells
    /// filled, the rest no-go, so the VMG table can be checked against sums
    /// worked by hand.
    fn sparse(cells: &[(usize, usize, f32)], min_tws_kt: f64) -> Polar {
        let mut grid = vec![f32::NAN; N_TWA * N_TWS];
        for &(twa, j, stw) in cells {
            grid[twa * N_TWS + j] = stw;
        }
        let mut p = Polar { grid, min_twa_deg: 0.0, max_tws_kt: 20.0, min_tws_kt, vmg: Vec::new() };
        p.build_vmg();
        p
    }

    /// Column 20 is 10 kn. Two angles with the same VMG tie; the table keeps
    /// the first found, the angle nearer the wind. The tie is exact: 8·cos 60°
    /// is 8 times an f32, so it is the same f32 as the 0° product.
    #[test]
    fn a_tie_in_beat_vmg_keeps_the_angle_nearer_the_wind() {
        let c60 = 60f32.to_radians().cos();
        let p = sparse(&[(0, 20, 8.0 * c60), (60, 20, 8.0)], 6.0);
        let e = p.vmg_at(10.0);
        assert_eq!(e.beat_vmg_kt, 8.0 * c60);
        assert_eq!(e.beat_twa_deg, 0.0);
        assert_eq!(e.beat_stw_kt, 8.0 * c60);
    }

    /// A repeated angle or speed is refused, not read at random; an infinite
    /// or NaN entry is a missing cell, not a speed that poisons the sums.
    #[test]
    fn repeated_axes_are_refused_and_non_finite_cells_are_missing() {
        assert!(matches!(Polar::parse("twa/tws\t6\t6\n90\t4\t5\n"), Err(PolarError::Malformed(_))));
        assert!(matches!(Polar::parse("twa/tws\t6\t10\n90\t4\t5\n90\t4\t5\n"), Err(PolarError::Malformed(_))));
        assert!(matches!(Polar::parse("twa/tws\t6\tinf\n90\t4\t5\n"), Err(PolarError::Malformed(_))));
        assert!(matches!(Polar::parse("twa/tws\t6\t10\ninf\t4\t5\n"), Err(PolarError::Malformed(_))));
        let p = Polar::parse("twa/tws;6;10;14\n90;inf;5;NaN\n").unwrap();
        assert_eq!(p.speed_kt(90.0, 10.0), Some(5.0));
        assert!(p.speed_kt(90.0, 6.0).is_some_and(|v| v.is_finite()));
    }

    /// The same tie on the run side: 8·cos 120° and the 180° speed that
    /// matches it. The table keeps the first angle found, 120°.
    #[test]
    fn a_tie_in_run_vmg_keeps_the_first_angle_found() {
        let c120 = 120f32.to_radians().cos();
        let p = sparse(&[(120, 20, 8.0), (180, 20, -8.0 * c120)], 6.0);
        let e = p.vmg_at(10.0);
        assert_eq!(e.run_vmg_kt, -8.0 * c120);
        assert_eq!(e.run_twa_deg, 120.0);
        assert_eq!(e.run_stw_kt, 8.0);
    }

    use proptest::prelude::*;

    const TWA_ROWS: [f64; 6] = [45.0, 60.0, 90.0, 120.0, 150.0, 180.0];
    const TWS_COLS: [f64; 4] = [6.0, 10.0, 14.0, 20.0];
    /// `TEST_POLAR`'s cells, `[row][column]`.
    const CELLS: [[f64; 4]; 6] = [
        [3.4, 5.0, 5.8, 6.0],
        [4.0, 5.8, 6.6, 6.9],
        [4.5, 6.0, 7.0, 7.5],
        [4.2, 6.0, 7.2, 7.9],
        [3.2, 5.2, 6.4, 7.2],
        [2.6, 4.4, 5.6, 6.6],
    ];

    /// Indices of the table entries either side of `x`.
    fn around(axis: &[f64], x: f64) -> (usize, usize) {
        let hi = axis.iter().position(|&a| a >= x).unwrap();
        (if axis[hi] == x { hi } else { hi - 1 }, hi)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Interpolation invents nothing: inside the table, a reading lies
        /// between the four table values around it, and port reads as
        /// starboard.
        #[test]
        fn a_reading_stays_between_the_table_values_around_it(
            twa in 45.0f64..=180.0,
            tws in 6.0f64..=20.0,
        ) {
            let p = Polar::parse(TEST_POLAR).unwrap();
            let v = p.speed_kt(twa, tws).unwrap();
            let (r0, r1) = around(&TWA_ROWS, twa);
            let (c0, c1) = around(&TWS_COLS, tws);
            let corners = [CELLS[r0][c0], CELLS[r0][c1], CELLS[r1][c0], CELLS[r1][c1]];
            let lo = corners.iter().copied().fold(f64::MAX, f64::min);
            let hi = corners.iter().copied().fold(f64::MIN, f64::max);
            prop_assert!(lo - 1e-5 <= v && v <= hi + 1e-5, "{} not in {}..{}", v, lo, hi);
            prop_assert_eq!(p.speed_kt(-twa, tws), Some(v));
        }
    }
}
