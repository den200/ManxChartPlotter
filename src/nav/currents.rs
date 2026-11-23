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


use super::grib::GeoBox;
use super::isochrone::CurrentField;
use crate::render::projection::Projection;

/// What to call the source wherever it is shown.
pub const CURRENT_SOURCE_LABEL: &str = "Open-Meteo Marine (SMOC, tides included)";

/// The spacing asked for along each axis, degrees — close to the source
/// model's own 0.08°, so a tidal stream round a headland is not smeared
/// across a whole sound.
const TARGET_STEP_DEG: f64 = 0.1;
/// Points per axis, at least and at most. 16×16 = 256 points keeps the
/// request one polite API call; a long passage gets coarser spacing.
const MIN_AXIS: usize = 6;
const MAX_AXIS: usize = 16;

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
        let url = marine_url(&lats, &lons, hours_ahead);
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
        // Borrowed, never cloned: the series are the bulk of the answer and
        // there are two of them per point.
        fn hourly<'j>(
            p: &'j serde_json::Value,
            key: &str,
        ) -> Option<&'j Vec<serde_json::Value>> {
            p.get("hourly")?.get(key)?.as_array()
        }
        let first = &points[0];
        let stamps = hourly(first, "time").ok_or("current answer lacks hourly.time")?;
        // Seconds from `timeformat=unixtime`. An answer that came back in some
        // other shape is refused rather than half-read: a current field
        // silently shifted an hour is worse than none.
        let times: Vec<i64> = stamps.iter().filter_map(|t| Some(t.as_i64()? * 1000)).collect();
        if times.len() != stamps.len() || times.is_empty() {
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

    /// The hours the field covers, ascending — what a time cursor moves over.
    pub fn steps(&self) -> &[i64] {
        &self.times
    }

    /// Is this position inside the fetched box?
    ///
    /// The sampler clamps at the edges, which is right for a passage fetched
    /// *for* those endpoints and quite wrong for a display: clamping would
    /// paint the edge's set across the whole ocean beyond it. Same reasoning,
    /// and same shape, as the wind field's `covers`.
    pub fn covers(&self, lat: f64, lon: f64) -> bool {
        let within = |axis: &[f64], x: f64| match (axis.first(), axis.last()) {
            (Some(&lo), Some(&hi)) => x >= lo && x <= hi,
            _ => false,
        };
        within(&self.lats, lat) && within(&self.lons, lon)
    }

    /// Current at a position and time for display: `(set towards °T, knots)`.
    ///
    /// `None` outside the box, and `None` where the model has no sea — the
    /// two cases a router may treat as still water and a display may not. An
    /// arrow drawn over dry land is a bug the reader cannot see through.
    pub fn sample(&self, lat: f64, lon: f64, time_ms: i64) -> Option<(f64, f64)> {
        if !self.covers(lat, lon) {
            return None;
        }
        let (u, v) = self.uv_opt(lat, lon, time_ms)?;
        let kt = (u * u + v * v).sqrt() * 3600.0 / 1852.0;
        // Set is quoted the way the water goes, so no negation here — the
        // opposite of the wind's convention, deliberately kept apart.
        let to_deg = u.atan2(v).to_degrees().rem_euclid(360.0);
        Some((to_deg, kt))
    }

    /// U/V in m/s — bilinear over the grid with NaN cells dropped and the
    /// weights renormalized, linear in time, clamped at the edges. A point
    /// with no data at all answers still water: the honest default when the
    /// model is silent, and what the router needs.
    fn uv(&self, lat: f64, lon: f64, time_ms: i64) -> (f64, f64) {
        self.uv_opt(lat, lon, time_ms).unwrap_or((0.0, 0.0))
    }

    /// The same lookup, but saying so when the model had nothing to say.
    fn uv_opt(&self, lat: f64, lon: f64, time_ms: i64) -> Option<(f64, f64)> {
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
                Some((u0 + (u1 - u0) * ft, v0 + (v1 - v0) * ft))
            }
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
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
        let n = (((hi - lo) / TARGET_STEP_DEG).ceil() as usize + 1).clamp(MIN_AXIS, MAX_AXIS);
        let step = (hi - lo).max(1e-6) / (n - 1) as f64;
        (0..n).map(|i| lo + i as f64 * step).collect()
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

/// The Open-Meteo marine request for the grid `lats` × `lons` out to
/// `hours_ahead`: every grid point as a pair in the two comma lists, row by
/// row, and enough forecast days to cover the horizon. Split out of
/// [`CurrentForecast::fetch`] so what is asked for can be tested without
/// asking.
fn marine_url(lats: &[f64], lons: &[f64], hours_ahead: u32) -> String {
    let mut lat_list = String::new();
    let mut lon_list = String::new();
    for la in lats {
        for lo in lons {
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
    // `timeformat=unixtime` is not a detail. The answer repeats the whole
    // hour axis once per point, so 144 points over three days is 10 000
    // timestamps; as ISO strings that is megabytes to move, allocate and
    // parse, and on a Pi it is the most expensive thing about a current
    // fetch. As integers it is a fraction of that.
    format!(
        "https://marine-api.open-meteo.com/v1/marine?latitude={lat_list}&longitude={lon_list}\
         &hourly=ocean_current_velocity,ocean_current_direction&forecast_days={days}\
         &timeformat=unixtime&timezone=UTC"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The request names every grid point as a pair, row by row, and asks
    /// for enough calendar days: the API counts them from today's midnight,
    /// so a horizon of h hours needs ⌈h/24⌉ days plus the one already under
    /// way — and never more than the eight it serves.
    #[test]
    fn the_request_asks_for_every_point_and_enough_days() {
        let query = |url: &str, key: &str| -> String {
            let at = url.find(&format!("{key}=")).expect(key) + key.len() + 1;
            url[at..].split('&').next().unwrap().to_string()
        };
        let url = marine_url(&[55.0, 55.5], &[10.0, 10.25, 10.5], 0);
        assert!(url.starts_with("https://marine-api.open-meteo.com/v1/marine?"));
        assert_eq!(query(&url, "latitude"), "55.000,55.000,55.000,55.500,55.500,55.500");
        assert_eq!(query(&url, "longitude"), "10.000,10.250,10.500,10.000,10.250,10.500");
        assert_eq!(query(&url, "hourly"), "ocean_current_velocity,ocean_current_direction");
        assert_eq!(query(&url, "timeformat"), "unixtime");
        for (hours, days) in [(0, "1"), (1, "2"), (24, "2"), (25, "3"), (48, "3"), (167, "8"), (500, "8")] {
            let url = marine_url(&[55.0], &[10.0], hours);
            assert_eq!(query(&url, "forecast_days"), days, "{hours} h ahead");
        }
    }

    /// A 2×2 grid, two hours, in the API's own JSON shape: the parse must
    /// turn "1 km/h toward east" into u = +0.278 m/s and keep the point
    /// order we asked for.
    #[test]
    fn parsing_converts_toward_direction_to_components() {
        let point = |vel: f64, dir: f64| {
            format!(
                r#"{{"latitude":56.0,"longitude":11.0,"hourly":{{
                    "time":[1785801600,1785805200],
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
        assert!((f.u[0][1] as f64).abs() < 1e-6);
        // Cell 2 (56.5, 11.0): south.
        assert!((f.v[0][2] as f64 + kmh).abs() < 1e-6);
        // Cell 3 (56.5, 11.5): west.
        assert!((f.u[0][3] as f64 + kmh).abs() < 1e-6);
        assert!((f.v[0][3] as f64).abs() < 1e-6);
        assert!((f.max_ms - kmh).abs() < 1e-9);
    }

    /// Bilinear in space on the right axes: the stream here strengthens
    /// eastward in u and northward in v, so a lat/lon mix-up or a wrong row
    /// order reads a different number.
    #[test]
    fn space_interpolation_follows_latitude_and_longitude() {
        // Rows are latitude (south first), columns longitude (west first).
        let f = CurrentForecast::synthetic(
            vec![56.0, 57.0],
            vec![11.0, 12.0],
            vec![0],
            // u: 0 on the west column, 1 on the east.
            vec![vec![0.0, 1.0, 0.0, 1.0]],
            // v: 0 on the south row, 2 on the north.
            vec![vec![0.0, 0.0, 2.0, 2.0]],
            2.0,
        );
        let at = |lat: f64, lon: f64| {
            let (x, y) = Projection::to_mercator(lat, lon);
            f.current([x, y], 0)
        };
        let (u, v) = at(56.25, 11.75);
        assert!((u - 0.75).abs() < 1e-6, "u {u}");
        assert!((v - 0.5).abs() < 1e-6, "v {v}");
        // The display reads the same water: 0.75 east, 0.5 north sets
        // towards 056°.
        let (to, kt) = f.sample(56.25, 11.75, 0).expect("inside the box");
        assert!((to - 0.75f64.atan2(0.5).to_degrees()).abs() < 1e-3, "{to}");
        assert!((kt - 0.75f64.hypot(0.5) * 3600.0 / 1852.0).abs() < 1e-4);
    }

    /// The display sampler must refuse what the router is happy to clamp.
    /// A field fetched for a passage across the Sound says nothing about the
    /// Atlantic, and an arrow there would be an invention.
    #[test]
    fn the_display_sampler_stops_at_the_edge_of_the_box() {
        let f = CurrentForecast::synthetic(
            vec![56.0, 57.0],
            vec![11.0, 12.0],
            vec![0],
            // 1 knot toward the east everywhere.
            vec![vec![0.514444; 4]],
            vec![vec![0.0; 4]],
            0.514444,
        );
        assert!(f.covers(56.5, 11.5));
        assert!(!f.covers(56.5, 20.0));
        assert!(!f.covers(40.0, 11.5));
        let (to, kt) = f.sample(56.5, 11.5, 0).expect("inside the box");
        assert!((to - 90.0).abs() < 1e-3, "set east, got {to}");
        assert!((kt - 1.0).abs() < 1e-3, "one knot, got {kt}");
        assert_eq!(f.sample(56.5, 20.0, 0), None);
    }

    /// Land answers nothing rather than answering slack water: a field of
    /// still-water arrows over a headland reads as a fact about the tide.
    #[test]
    fn the_display_sampler_reports_nothing_where_the_model_has_no_sea() {
        let f = CurrentForecast::synthetic(
            vec![56.0, 57.0],
            vec![11.0, 12.0],
            vec![0],
            vec![vec![f32::NAN; 4]],
            vec![vec![f32::NAN; 4]],
            0.0,
        );
        assert_eq!(f.sample(56.5, 11.5, 0), None);
        // …while the router still gets its still water.
        let (x, y) = Projection::to_mercator(56.5, 11.5);
        assert_eq!(f.current([x, y], 0), (0.0, 0.0));
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
        // A degree of latitude at ~0.1° is 11 points; two of longitude hit
        // the cap. Both axes reach the edges exactly.
        assert_eq!(lats.len(), 11);
        assert_eq!(lons.len(), MAX_AXIS);
        assert!((lats[0] - 56.0).abs() < 1e-9 && (lats[lats.len() - 1] - 57.0).abs() < 1e-9);
        assert!((lons[0] - 10.0).abs() < 1e-9 && (lons[lons.len() - 1] - 12.0).abs() < 1e-9);

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

    /// One kilometre an hour is 1000 m in 3600 s: 5/18 m/s. Checked against
    /// that arithmetic rather than against the constant, and through the
    /// parser: 3.6 km/h toward the east is exactly 1 m/s of u, and that is
    /// also the fastest water in the field.
    #[test]
    fn a_kilometre_an_hour_is_five_eighteenths_of_a_metre_a_second() {
        assert!((KMH_TO_MS - 5.0 / 18.0).abs() < 1e-12, "{KMH_TO_MS}");
        let json = r#"[{"hourly":{"time":[1785801600],
            "ocean_current_velocity":[3.6],"ocean_current_direction":[90.0]}}]"#;
        let f = CurrentForecast::from_json(json, vec![56.0], vec![11.0]).expect("parses");
        assert!((f.u[0][0] as f64 - 1.0).abs() < 1e-6, "u {}", f.u[0][0]);
        assert!((f.v[0][0] as f64).abs() < 1e-6, "v {}", f.v[0][0]);
        assert!((f.max_speed_ms() - 1.0).abs() < 1e-9, "{}", f.max_speed_ms());
    }

    /// The answer has one point per grid node: rows × columns. On a 2×2 box
    /// a sum and a product agree, so this asks for 2×3 = 6 (a sum would say
    /// 5). The points come back in the order asked — row by row, south row
    /// first — and the unix seconds become ms.
    #[test]
    fn a_two_by_three_answer_is_six_points_in_row_order() {
        let point = |kmh: f64| {
            format!(
                r#"{{"hourly":{{"time":[1785801600,1785805200],
                    "ocean_current_velocity":[{kmh},{kmh}],
                    "ocean_current_direction":[0.0,0.0]}}}}"#
            )
        };
        // Point k sets north at 0.36·(k+1) km/h, which is (k+1)/10 m/s.
        let points: Vec<String> = (0..6).map(|k| point(0.36 * (k + 1) as f64)).collect();
        let lats = vec![56.0, 56.5];
        let lons = vec![11.0, 11.5, 12.0];
        let f = CurrentForecast::from_json(&format!("[{}]", points.join(",")), lats.clone(), lons.clone())
            .expect("six points for a 2×3 grid");
        assert_eq!(f.steps(), &[1_785_801_600_000i64, 1_785_805_200_000][..]);
        for k in 0..6 {
            let want = (k + 1) as f64 / 10.0;
            assert!((f.v[1][k] as f64 - want).abs() < 1e-6, "cell {k}: {}", f.v[1][k]);
            assert!((f.u[1][k] as f64).abs() < 1e-6);
        }
        // The fastest is the last point: 2.16 km/h = 0.6 m/s.
        assert!((f.max_speed_ms() - 0.6).abs() < 1e-9, "{}", f.max_speed_ms());
        // Row 1, column 2 — the north-east node — read through the display
        // sampler: 0.6 m/s toward north.
        let (to, kt) = f.sample(56.5, 12.0, 1_785_801_600_000).expect("a node of the box");
        assert!(to < 1e-3 || to > 360.0 - 1e-3, "set {to}");
        assert!((kt - 0.6 * 3600.0 / 1852.0).abs() < 1e-5, "{kt} kt");

        // Five or seven points for six nodes is a different question's answer.
        let five = format!("[{}]", points[..5].join(","));
        assert!(CurrentForecast::from_json(&five, lats.clone(), lons.clone()).is_err());
        let seven = format!("[{},{}]", points.join(","), points[0]);
        assert!(CurrentForecast::from_json(&seven, lats, lons).is_err());
    }

    /// Any time that is not unix seconds refuses the whole answer — one ISO
    /// string among integers included, since dropping it would shift every
    /// later hour onto the wrong stamp. An empty time axis is refused too:
    /// there is no hour to put the field at.
    #[test]
    fn an_answer_with_any_unreadable_time_is_refused() {
        let with_times = |times: &str| {
            format!(
                r#"[{{"hourly":{{"time":{times},
                    "ocean_current_velocity":[1.0,1.0],
                    "ocean_current_direction":[0.0,0.0]}}}}]"#
            )
        };
        let ok = with_times("[1785801600,1785805200]");
        assert!(CurrentForecast::from_json(&ok, vec![56.0], vec![11.0]).is_ok());
        for bad in [
            r#"[1785801600,"2026-08-04T01:00"]"#,
            r#"["2026-08-04T00:00","2026-08-04T01:00"]"#,
            "[]",
        ] {
            let json = with_times(bad);
            assert!(
                CurrentForecast::from_json(&json, vec![56.0], vec![11.0]).is_err(),
                "accepted times {bad}"
            );
        }
    }

    /// A field that is bilinear in latitude and longitude over the whole box,
    /// `a + b·dy + c·dx + d·dy·dx`, is reproduced exactly by bilinear
    /// interpolation in every cell — so the expected value anywhere is the
    /// formula itself, not a number read off the code.
    fn bilinear_truth(lat: f64, lon: f64) -> (f64, f64) {
        let (dy, dx) = (lat - 56.0, lon - 11.0);
        (
            0.2 + 0.8 * dy - 1.2 * dx + 1.6 * dy * dx,
            -0.5 + 0.3 * dy + 2.0 * dx - 0.9 * dy * dx,
        )
    }

    /// Three nodes per axis, so a sample can sit in a cell away from row 0
    /// and column 0 — where a wrong flat index (row × width + column) reads
    /// a different node — and spacings of 0.5° and 0.25° rather than 1°, so
    /// a fraction not divided by its span shows.
    fn three_by_three() -> CurrentForecast {
        let lats = vec![56.0, 56.5, 57.0];
        let lons = vec![11.0, 11.25, 11.5];
        let (mut u, mut v) = (Vec::new(), Vec::new());
        for &la in &lats {
            for &lo in &lons {
                let (cu, cv) = bilinear_truth(la, lo);
                u.push(cu as f32);
                v.push(cv as f32);
            }
        }
        CurrentForecast::synthetic(lats, lons, vec![0], vec![u], vec![v], 3.0)
    }

    #[test]
    fn interpolation_reproduces_a_bilinear_field_in_every_cell() {
        let f = three_by_three();
        // One point in each of the four cells, none at a cell's middle.
        for (lat, lon) in [(56.2, 11.1), (56.3, 11.45), (56.6, 11.05), (56.8, 11.4)] {
            let (u, v) = f.uv_opt(lat, lon, 0).expect("all sea");
            let (tu, tv) = bilinear_truth(lat, lon);
            assert!((u - tu).abs() < 1e-5, "u at ({lat}, {lon}): {u} vs {tu}");
            assert!((v - tv).abs() < 1e-5, "v at ({lat}, {lon}): {v} vs {tv}");
        }
    }

    /// Beyond the box the router's sampler clamps: the water north of the
    /// box is the water on its north edge at the same longitude, and so on
    /// round the compass. (The display sampler refuses these points; the
    /// router must still get an answer, and not an out-of-range index.)
    #[test]
    fn the_router_sampler_clamps_to_the_nearest_edge_outside_the_box() {
        let f = three_by_three();
        for ((lat, lon), (elat, elon)) in [
            ((58.0, 11.1), (57.0, 11.1)),   // north
            ((56.7, 13.0), (56.7, 11.5)),   // east
            ((55.0, 11.3), (56.0, 11.3)),   // south
            ((56.4, 10.0), (56.4, 11.0)),   // west
            ((60.0, 15.0), (57.0, 11.5)),   // north-east corner
            ((50.0, 5.0), (56.0, 11.0)),    // south-west corner
        ] {
            let (u, v) = f.uv(lat, lon, 0);
            let (tu, tv) = bilinear_truth(elat, elon);
            assert!((u - tu).abs() < 1e-5, "u at ({lat}, {lon}): {u} vs {tu}");
            assert!((v - tv).abs() < 1e-5, "v at ({lat}, {lon}): {v} vs {tv}");
        }
    }

    /// A corner is sea only when it has both components. One with u but no
    /// v (or the reverse) is not half a current; it drops out like land and
    /// the other three are renormalized — in v as much as in u.
    #[test]
    fn a_corner_missing_either_component_drops_out_and_v_renormalizes_too() {
        // Three sea corners agree on 0.4 east, 0.3 north. The north-east
        // corner has a wild u and no v; the next field has the reverse.
        let half_u = CurrentForecast::synthetic(
            vec![56.0, 57.0],
            vec![11.0, 12.0],
            vec![0],
            vec![vec![0.4, 0.4, 0.4, 5.0]],
            vec![vec![0.3, 0.3, 0.3, f32::NAN]],
            0.5,
        );
        let half_v = CurrentForecast::synthetic(
            vec![56.0, 57.0],
            vec![11.0, 12.0],
            vec![0],
            vec![vec![0.4, 0.4, 0.4, f32::NAN]],
            vec![vec![0.3, 0.3, 0.3, 5.0]],
            0.5,
        );
        for f in [half_u, half_v] {
            // Off-centre too, so every corner weighs something different.
            for (lat, lon) in [(56.5, 11.5), (56.3, 11.8)] {
                let (u, v) = f.uv_opt(lat, lon, 0).expect("three sea corners");
                assert!((u - 0.4).abs() < 1e-6, "u {u}");
                assert!((v - 0.3).abs() < 1e-6, "v {v}");
            }
        }
    }

    /// Linear in time between real epoch hours (an axis starting at 0 hides
    /// an offset error), with both components moving and neither starting
    /// at zero: a quarter of the way from (0.2, −0.4) to (1.0, 0.4) is
    /// (0.4, −0.2).
    #[test]
    fn time_interpolation_is_linear_between_real_hours_in_both_components() {
        const T: i64 = 1_785_801_600_000;
        let f = CurrentForecast::synthetic(
            vec![56.0, 57.0],
            vec![11.0, 12.0],
            vec![T, T + 3_600_000],
            vec![vec![0.2; 4], vec![1.0; 4]],
            vec![vec![-0.4; 4], vec![0.4; 4]],
            1.1,
        );
        let (u, v) = f.uv(56.5, 11.5, T + 900_000);
        assert!((u - 0.4).abs() < 1e-6, "u {u}");
        assert!((v + 0.2).abs() < 1e-6, "v {v}");
    }

    /// Outside the forecast's hours the field holds its nearest hour:
    /// before the first, the first; after the last, the last.
    #[test]
    fn time_clamps_to_the_first_and_last_hour() {
        const T: i64 = 1_785_801_600_000;
        const H: i64 = 3_600_000;
        let f = CurrentForecast::synthetic(
            vec![56.0, 57.0],
            vec![11.0, 12.0],
            vec![T, T + H, T + 2 * H],
            vec![vec![0.1; 4], vec![0.2; 4], vec![0.3; 4]],
            vec![vec![-0.1; 4], vec![-0.2; 4], vec![-0.3; 4]],
            0.5,
        );
        let (u, v) = f.uv(56.5, 11.5, T + 10 * H);
        assert!((u - 0.3).abs() < 1e-6 && (v + 0.3).abs() < 1e-6, "late ({u}, {v})");
        let (u, v) = f.uv(56.5, 11.5, T - 10 * H);
        assert!((u - 0.1).abs() < 1e-6 && (v + 0.1).abs() < 1e-6, "early ({u}, {v})");
    }

    /// After the last hour, when that hour is empty, the nearest hour that
    /// has data stands in — the same rule as an empty hour inside the span.
    /// Answering still water there would invent slack tide.
    #[test]
    fn after_an_empty_last_hour_the_previous_hour_stands_in() {
        const T: i64 = 1_785_801_600_000;
        let f = CurrentForecast::synthetic(
            vec![56.0, 57.0],
            vec![11.0, 12.0],
            vec![T, T + 3_600_000],
            vec![vec![0.25; 4], vec![f32::NAN; 4]],
            vec![vec![-0.5; 4], vec![f32::NAN; 4]],
            0.6,
        );
        let (u, v) = f.uv_opt(56.5, 11.5, T + 5 * 3_600_000).expect("the first hour has data");
        assert!((u - 0.25).abs() < 1e-6 && (v + 0.5).abs() < 1e-6, "({u}, {v})");
    }

    /// §8.5, the rule this module exists to keep: directions interpolate as
    /// vectors. Halfway between a stream setting 350° and one setting 010°
    /// the water sets 000° — averaging the degrees would say 180°, dead
    /// against it — at cos 10° of the speed.
    #[test]
    fn halfway_between_350_and_010_the_set_is_north_not_south() {
        let f = two_streams(350.0, 10.0, 3.6);
        let (to, kt) = f.sample(56.5, 11.5, 1_785_801_600_000).expect("inside the box");
        assert!(to < 1e-3 || to > 360.0 - 1e-3, "set {to}");
        let want_kt = 10f64.to_radians().cos() * 3600.0 / 1852.0;
        assert!((kt - want_kt).abs() < 1e-5, "{kt} kt, want {want_kt}");
    }

    /// A 2×2 field parsed from the API's JSON: the west column sets `west`,
    /// the east column `east`, both at `kmh`.
    fn two_streams(west: f64, east: f64, kmh: f64) -> CurrentForecast {
        let point = |dir: f64| {
            format!(
                r#"{{"hourly":{{"time":[1785801600],
                    "ocean_current_velocity":[{kmh}],
                    "ocean_current_direction":[{dir}]}}}}"#
            )
        };
        // Row-major, south row first: SW, SE, NW, NE.
        let json = format!("[{},{},{},{}]", point(west), point(east), point(west), point(east));
        CurrentForecast::from_json(&json, vec![56.0, 57.0], vec![11.0, 12.0]).expect("parses")
    }

    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        /// Bilinear weights are non-negative and, after renormalizing around
        /// land, sum to one: the sample is a convex mix of the sea corners
        /// and can never leave their range.
        #[test]
        fn a_sample_never_leaves_the_range_of_its_sea_corners(
            us in prop::array::uniform4(-3.0f32..3.0),
            vs in prop::array::uniform4(-3.0f32..3.0),
            land in 0u8..15, // bit k set = corner k is land; never all four
            fy in 0.0f64..=1.0,
            fx in 0.0f64..=1.0,
            dlat in 0.05f64..2.0,
            dlon in 0.05f64..2.0,
        ) {
            let (mut u, mut v) = (us.to_vec(), vs.to_vec());
            for k in 0..4 {
                if land & (1 << k) != 0 {
                    u[k] = f32::NAN;
                    v[k] = f32::NAN;
                }
            }
            let f = CurrentForecast::synthetic(
                vec![56.0, 56.0 + dlat],
                vec![11.0, 11.0 + dlon],
                vec![0],
                vec![u.clone()],
                vec![v.clone()],
                5.0,
            );
            let range = |c: &[f32]| {
                c.iter().filter(|x| x.is_finite()).fold((f64::MAX, f64::MIN), |(lo, hi), &x| {
                    (lo.min(x as f64), hi.max(x as f64))
                })
            };
            // A point whose only sea corners carry no weight has no sample.
            if let Some((su, sv)) = f.uv_opt(56.0 + fy * dlat, 11.0 + fx * dlon, 0) {
                let ((ulo, uhi), (vlo, vhi)) = (range(&u), range(&v));
                prop_assert!(su >= ulo - 1e-6 && su <= uhi + 1e-6, "u {su} outside [{ulo}, {uhi}]");
                prop_assert!(sv >= vlo - 1e-6 && sv <= vhi + 1e-6, "v {sv} outside [{vlo}, {vhi}]");
            }
        }

        /// Sampled exactly on a node, the field is that node: the weight
        /// there is one and every other corner's is zero.
        #[test]
        fn a_sample_exactly_on_a_node_is_that_node(
            us in prop::collection::vec(-3.0f32..3.0, 9),
            vs in prop::collection::vec(-3.0f32..3.0, 9),
            i in 0usize..3,
            j in 0usize..3,
        ) {
            let lats = vec![56.0, 56.5, 57.0];
            let lons = vec![11.0, 11.25, 11.5];
            let (lat, lon) = (lats[i], lons[j]);
            let f = CurrentForecast::synthetic(lats, lons, vec![0], vec![us.clone()], vec![vs.clone()], 5.0);
            let (u, v) = f.uv_opt(lat, lon, 0).expect("all sea");
            prop_assert!((u - us[i * 3 + j] as f64).abs() < 1e-12, "u {u} vs {}", us[i * 3 + j]);
            prop_assert!((v - vs[i * 3 + j] as f64).abs() < 1e-12, "v {v} vs {}", vs[i * 3 + j]);
        }

        /// Two streams of equal speed, `delta` apart, mixed half and half:
        /// the vector mean sets exactly between them along the short arc,
        /// at cos(delta/2) of the speed — wherever the pair sits on the
        /// compass, including straddling north where degrees wrap.
        #[test]
        fn directions_are_averaged_as_vectors_across_the_compass(
            a in 0.0f64..360.0,
            delta in -170.0f64..170.0,
            kmh in 0.5f64..10.0,
        ) {
            let b = (a + delta).rem_euclid(360.0);
            let f = two_streams(a, b, kmh);
            let (to, kt) = f.sample(56.5, 11.5, 1_785_801_600_000).expect("inside the box");
            let want_to = (a + delta / 2.0).rem_euclid(360.0);
            let off = ((to - want_to + 540.0).rem_euclid(360.0) - 180.0).abs();
            prop_assert!(off < 1e-3, "set {to}, want {want_to}");
            let want_kt = kmh / 3.6 * (delta / 2.0).to_radians().cos() * 3600.0 / 1852.0;
            prop_assert!((kt - want_kt).abs() < 1e-5, "{kt} kt, want {want_kt}");
        }
    }
}
