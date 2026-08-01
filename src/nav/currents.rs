//! Surface currents: Open-Meteo's marine API, serving Météo-France's SMOC
//! analysis (the Copernicus global ocean forecast, tides included) at 0.08°.
//!
//! Why not GRIB like the wind: NOMADS retired every subsetting service for
//! RTOFS (SCN 25-81) and a raw step is a 155 MB NetCDF, while Copernicus
//! itself wants an account and a key. Open-Meteo re-serves SMOC keyless and
//! subset server-side — one JSON call for the whole passage box and span —
//! and for Danish waters SMOC's tidal component is precisely the part that
//! matters. The engine never knows the difference: this module answers the
//! same [`CurrentField`] questions a GRIB source would.
//!
//! §8.5 holds here too: direction and speed are converted to U/V components
//! once, at fetch; everything after interpolates components.

use chrono::NaiveDateTime;

use super::grib::GeoBox;
use super::isochrone::CurrentField;
use crate::render::projection::Projection;

/// What to call the source wherever it is shown.
pub const CURRENT_SOURCE_LABEL: &str = "Open-Meteo Marine (SMOC, tides included)";

/// Points requested per axis. 12×12 over the passage box outresolves the
/// 0.08° model on any coastal passage and stays one polite API call.
const N_AXIS: usize = 12;

const KMH_TO_MS: f64 = 1000.0 / 3600.0;

/// A fetched current field: our own regular grid over the passage box,
/// hourly, components in m/s. Cells the model has no sea for hold NaN and
/// the sampler renormalizes around them, same as the wave grid.
pub struct CurrentForecast {
    lats: Vec<f64>,
    lons: Vec<f64>,
    /// ms UTC, ascending, hourly.
    times: Vec<i64>,
    /// Per time step, row-major `[lat][lon]`, m/s east / north.
    u: Vec<Vec<f32>>,
    v: Vec<Vec<f32>>,
    max_ms: f64,
}

impl CurrentForecast {
    /// Fetch currents over `area` out to `hours_ahead`. Blocking; run it on
    /// a worker, and treat failure as a warning — like waves, currents are a
    /// refinement.
    pub fn fetch(area: GeoBox, hours_ahead: u32) -> Result<Self, String> {
        let (lats, lons) = grid_axes(area);
        let mut lat_list = String::new();
        let mut lon_list = String::new();
        for la in &lats {
            for lo in &lons {
                if !lat_list.is_empty() {
                    lat_list.push(',');
                    lon_list.push(',');
                }
                lat_list.push_str(&format!("{la:.3}"));
                lon_list.push_str(&format!("{lo:.3}"));
            }
        }
        // forecast_days counts calendar days from TODAY 00:00, so an
        // afternoon departure needs a day of padding to cover its horizon.
        let days = ((hours_ahead as usize).div_ceil(24) + 1).clamp(1, 8);
        let url = format!(
            "https://marine-api.open-meteo.com/v1/marine?latitude={lat_list}&longitude={lon_list}\
             &hourly=ocean_current_velocity,ocean_current_direction&forecast_days={days}"
        );
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(60)))
            .build()
            .into();
        let text = agent
            .get(&url)
            .call()
            .map_err(|e| format!("current fetch failed: {e}"))?
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("current fetch failed: {e}"))?;
        let f = Self::from_json(&text, lats, lons)?;
        log::info!(
            "currents: {} — {} steps, {}×{} grid, max {:.1} kt",
            CURRENT_SOURCE_LABEL,
            f.times.len(),
            f.lats.len(),
            f.lons.len(),
            f.max_ms * 3600.0 / 1852.0
        );
        Ok(f)
    }

    /// Parse the API's answer — an array of per-point series in the order
    /// the points were asked. Split out as the test seam.
    fn from_json(text: &str, lats: Vec<f64>, lons: Vec<f64>) -> Result<Self, String> {
        let points: Vec<serde_json::Value> =
            serde_json::from_str(text).map_err(|e| format!("current answer unreadable: {e}"))?;
        if points.len() != lats.len() * lons.len() {
            return Err(format!(
                "current answer has {} points for {}×{}",
                points.len(),
                lats.len(),
                lons.len()
            ));
        }
        let hourly = |p: &serde_json::Value, key: &str| -> Option<Vec<serde_json::Value>> {
            Some(p.get("hourly")?.get(key)?.as_array()?.clone())
        };
        let first = &points[0];
        let time_strs = hourly(first, "time").ok_or("current answer lacks hourly.time")?;
        let times: Vec<i64> = time_strs
            .iter()
            .filter_map(|t| {
                let s = t.as_str()?;
                let naive = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M").ok()?;
                Some(naive.and_utc().timestamp_millis())
            })
            .collect();
        if times.len() != time_strs.len() || times.is_empty() {
            return Err("current answer has unreadable times".into());
        }

        let n_cells = lats.len() * lons.len();
        let mut u = vec![vec![f32::NAN; n_cells]; times.len()];
        let mut v = vec![vec![f32::NAN; n_cells]; times.len()];
        let mut max_ms = 0.0f64;
        for (cell, p) in points.iter().enumerate() {
            let (Some(vel), Some(dir)) = (
                hourly(p, "ocean_current_velocity"),
                hourly(p, "ocean_current_direction"),
            ) else {
                continue; // a landlocked point answers without the series
            };
            for t in 0..times.len() {
                let (Some(spd), Some(d)) = (
                    vel.get(t).and_then(|x| x.as_f64()),
                    dir.get(t).and_then(|x| x.as_f64()),
                ) else {
                    continue; // null = no sea in the model here
                };
                let ms = spd * KMH_TO_MS;
                // "Direction following the flow": TOWARD, so the components
                // are direct — no meteorological negation.
                let rad = d.to_radians();
                u[t][cell] = (ms * rad.sin()) as f32;
                v[t][cell] = (ms * rad.cos()) as f32;
                max_ms = max_ms.max(ms);
            }
        }
        Ok(Self { lats, lons, times, u, v, max_ms })
    }

    /// U/V in m/s — bilinear over the grid with NaN cells dropped and the
    /// weights renormalized, linear in time, clamped at the edges. A point
    /// with no data at all answers still water: the honest default when the
    /// model is silent.
    fn uv(&self, lat: f64, lon: f64, time_ms: i64) -> (f64, f64) {
        let (i0, i1, fy) = bracket(&self.lats, lat);
        let (j0, j1, fx) = bracket(&self.lons, lon);
        let (t0, t1, ft) = bracket_i64(&self.times, time_ms);

        let w = self.lons.len();
        let sample_t = |t: usize| -> Option<(f64, f64)> {
            let corners = [
                (i0 * w + j0, (1.0 - fy) * (1.0 - fx)),
                (i0 * w + j1, (1.0 - fy) * fx),
                (i1 * w + j0, fy * (1.0 - fx)),
                (i1 * w + j1, fy * fx),
            ];
            let (mut au, mut av, mut wsum) = (0.0, 0.0, 0.0);
            for (idx, wt) in corners {
                let cu = self.u[t][idx] as f64;
                let cv = self.v[t][idx] as f64;
                if cu.is_finite() && cv.is_finite() {
                    au += cu * wt;
                    av += cv * wt;
                    wsum += wt;
                }
            }
            (wsum > 1e-9).then(|| (au / wsum, av / wsum))
        };
        match (sample_t(t0), sample_t(t1)) {
            (Some((u0, v0)), Some((u1, v1))) => {
                (u0 + (u1 - u0) * ft, v0 + (v1 - v0) * ft)
            }
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => (0.0, 0.0),
        }
    }

    #[cfg(test)]
    fn synthetic(
        lats: Vec<f64>,
        lons: Vec<f64>,
        times: Vec<i64>,
        u: Vec<Vec<f32>>,
        v: Vec<Vec<f32>>,
        max_ms: f64,
    ) -> Self {
        Self { lats, lons, times, u, v, max_ms }
    }
}

impl CurrentField for CurrentForecast {
    fn current(&self, pos_merc: [f64; 2], time_ms: i64) -> (f64, f64) {
        let (lat, lon) = Projection::to_wgs84(pos_merc[0], pos_merc[1]);
        self.uv(lat, lon, time_ms)
    }
    fn max_speed_ms(&self) -> f64 {
        self.max_ms
    }
}

/// The regular axes requested over a box, ascending.
fn grid_axes(area: GeoBox) -> (Vec<f64>, Vec<f64>) {
    let axis = |a: f64, b: f64| -> Vec<f64> {
        let (lo, hi) = (a.min(b), a.max(b));
        let step = (hi - lo).max(1e-6) / (N_AXIS - 1) as f64;
        (0..N_AXIS).map(|i| lo + i as f64 * step).collect()
    };
    (axis(area.south, area.north), axis(area.west, area.east))
}

/// Bracketing on an ascending axis, clamped — the wave sampler's twin, local
/// because the axes here are always ascending (we made them).
fn bracket(axis: &[f64], x: f64) -> (usize, usize, f64) {
    if axis.len() < 2 {
        return (0, 0, 0.0);
    }
    let n = axis.len();
    for i in 0..n - 1 {
        if x <= axis[i + 1] {
            let span = axis[i + 1] - axis[i];
            let f = if span <= 1e-12 {
                0.0
            } else {
                ((x - axis[i]) / span).clamp(0.0, 1.0)
            };
            return (i, i + 1, f);
        }
    }
    (n - 2, n - 1, 1.0)
}

fn bracket_i64(axis: &[i64], x: i64) -> (usize, usize, f64) {
    if axis.len() < 2 {
        return (0, 0, 0.0);
    }
    let n = axis.len();
    for i in 0..n - 1 {
        if x <= axis[i + 1] {
            let span = (axis[i + 1] - axis[i]) as f64;
            let f = if span <= 0.0 {
                0.0
            } else {
                ((x - axis[i]) as f64 / span).clamp(0.0, 1.0)
            };
            return (i, i + 1, f);
        }
    }
    (n - 2, n - 1, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2×2 grid, two hours, in the API's own JSON shape: the parse must
    /// turn "1 km/h toward east" into u = +0.278 m/s and keep the point
    /// order we asked for.
    #[test]
    fn parsing_converts_toward_direction_to_components() {
        let point = |vel: f64, dir: f64| {
            format!(
                r#"{{"latitude":56.0,"longitude":11.0,"hourly":{{
                    "time":["2026-08-01T00:00","2026-08-01T01:00"],
                    "ocean_current_velocity":[{vel},{vel}],
                    "ocean_current_direction":[{dir},{dir}]}}}}"#
            )
        };
        let json = format!(
            "[{},{},{},{}]",
            point(1.0, 90.0),  // 1 km/h toward east
            point(1.0, 0.0),   // toward north
            point(1.0, 180.0), // toward south
            point(1.0, 270.0), // toward west
        );
        let f = CurrentForecast::from_json(
            &json,
            vec![56.0, 56.5],
            vec![11.0, 11.5],
        )
        .expect("parses");
        assert_eq!(f.times.len(), 2);
        let kmh = KMH_TO_MS;
        // Cell 0 (56.0, 11.0): east.
        assert!((f.u[0][0] as f64 - kmh).abs() < 1e-6);
        assert!((f.v[0][0] as f64).abs() < 1e-6);
        // Cell 1 (56.0, 11.5): north.
        assert!((f.v[0][1] as f64 - kmh).abs() < 1e-6);
        // Cell 2 (56.5, 11.0): south.
        assert!((f.v[0][2] as f64 + kmh).abs() < 1e-6);
        assert!((f.max_ms - kmh).abs() < 1e-9);
    }

    /// Nulls — the model's land — drop out of the sample, and a spot with
    /// no data at all is still water, not NaN water.
    #[test]
    fn null_cells_renormalize_and_no_data_is_still_water() {
        let f = CurrentForecast::synthetic(
            vec![56.0, 57.0],
            vec![11.0, 12.0],
            vec![0, 3_600_000],
            // Three sea corners agree on 0.5 m/s east; the NW corner is land.
            vec![
                vec![0.5, 0.5, 0.5, f32::NAN],
                vec![f32::NAN; 4],
            ],
            vec![vec![0.0, 0.0, 0.0, f32::NAN], vec![f32::NAN; 4]],
            0.5,
        );
        let (x, y) = Projection::to_mercator(56.5, 11.5);
        let (u, v) = f.current([x, y], 0);
        assert!((u - 0.5).abs() < 1e-6, "{u}");
        assert!(v.abs() < 1e-6);
        // The all-NaN hour: still water.
        let (u, _) = f.current([x, y], 3_600_000);
        assert!((u - 0.5).abs() < 1e-6, "t0 stands in for the empty hour");
        let g = CurrentForecast::synthetic(
            vec![56.0, 57.0],
            vec![11.0, 12.0],
            vec![0],
            vec![vec![f32::NAN; 4]],
            vec![vec![f32::NAN; 4]],
            0.0,
        );
        let (u, v) = g.current([x, y], 0);
        assert_eq!((u, v), (0.0, 0.0));
    }

    #[test]
    fn the_axes_cover_the_box_and_time_interpolates() {
        let (lats, lons) = grid_axes(GeoBox { south: 56.0, north: 57.0, west: 10.0, east: 12.0 });
        assert_eq!(lats.len(), N_AXIS);
        assert!((lats[0] - 56.0).abs() < 1e-9 && (lats[N_AXIS - 1] - 57.0).abs() < 1e-9);
        assert!((lons[0] - 10.0).abs() < 1e-9 && (lons[N_AXIS - 1] - 12.0).abs() < 1e-9);

        let f = CurrentForecast::synthetic(
            vec![56.0, 57.0],
            vec![11.0, 12.0],
            vec![0, 3_600_000],
            vec![vec![0.0; 4], vec![1.0; 4]],
            vec![vec![0.0; 4], vec![0.0; 4]],
            1.0,
        );
        let (x, y) = Projection::to_mercator(56.5, 11.5);
        let (u, _) = f.current([x, y], 1_800_000);
        assert!((u - 0.5).abs() < 1e-6, "{u}");
    }
}
