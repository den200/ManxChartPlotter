//! Boat polars from ORC certificates.
//!
//! The Offshore Racing Congress publishes every current-year certificate,
//! VPP included, through a public per-country endpoint — no key, plain
//! JSON. A certificate's allowances are seconds per mile at 9 wind speeds
//! across 8 true wind angles, plus the beat and run optima with their
//! angles and the deep downwind rows: everything a routing polar needs,
//! measured rather than guessed. This module fetches a country's list,
//! lets the user search it by class or name, and converts the pick into
//! the same `.pol` grid the router already eats.
//!
//! The certificate also carries LOA, beam and draft — so finding the polar
//! can fill in the boat's specs at the same time.

use std::path::Path;

/// One certificate, converted and ready to use.
#[derive(Debug, Clone)]
pub struct OrcHit {
    pub name: String,
    pub class: String,
    pub builder: String,
    pub year: String,
    pub loa_m: f64,
    pub beam_m: f64,
    pub draft_m: f64,
    /// The `.pol` text, converted from the allowances.
    pub pol: String,
}

/// Countries offered in the picker: the major sailing nations ORC lists.
/// "ALL" is not a server option — the endpoint answers it with an empty
/// list — so sweeping means one request per entry here.
pub const COUNTRIES: &[&str] = &[
    "DEN", "GER", "SWE", "NOR", "NED", "GBR", "FRA", "ITA", "ESP", "FIN", "EST", "POL", "USA",
    "AUS", "GRE", "CRO", "TUR", "POR",
];

const ENDPOINT: &str = "https://data.orc.org/public/WPub.dll";

/// Search a country's current certificates (or every listed country for
/// `"ALL"`) for a class or boat name. Blocking; run it on a worker. The
/// per-country answer is cached for the day — certificates change yearly.
pub fn search(
    country: &str,
    query: &str,
    cache_dir: &Path,
    mut progress: impl FnMut(String),
) -> Result<Vec<OrcHit>, String> {
    let countries: Vec<&str> = if country.is_empty() || country == "ALL" {
        COUNTRIES.to_vec()
    } else {
        vec![country]
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(60)))
        .build()
        .into();
    std::fs::create_dir_all(cache_dir).map_err(|e| e.to_string())?;

    let needle = query.trim().to_lowercase();
    let mut hits = Vec::new();
    for c in countries {
        progress(format!("searching {c}"));
        let text = match fetch_country(&agent, c, cache_dir) {
            Ok(t) => t,
            Err(e) => {
                // One nation down must not sink a sweep.
                log::warn!("orc: {c}: {e}");
                continue;
            }
        };
        hits.extend(parse_and_filter(&text, &needle));
        if hits.len() >= 60 {
            break;
        }
    }
    hits.truncate(60);
    Ok(hits)
}

fn fetch_country(agent: &ureq::Agent, country: &str, cache_dir: &Path) -> Result<String, String> {
    let day = chrono::Utc::now().format("%Y%m%d");
    let path = cache_dir.join(format!("rms-{country}-{day}.json"));
    if let Ok(text) = std::fs::read_to_string(&path) {
        if !text.is_empty() {
            return Ok(text);
        }
    }
    let url = format!("{ENDPOINT}?action=DownRMS&CountryId={country}&ext=json");
    let text = agent
        .get(&url)
        .call()
        .map_err(|e| e.to_string())?
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())?;
    // The server prepends a UTF-8 BOM; serde_json refuses it.
    let text = text.trim_start_matches('\u{feff}').to_string();
    if !text.contains("\"rms\"") {
        return Err("unexpected answer (not an RMS list)".into());
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, &text).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
    Ok(text)
}

fn parse_and_filter(text: &str, needle: &str) -> Vec<OrcHit> {
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let Some(list) = doc.get("rms").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    list.iter()
        .filter(|b| {
            if needle.is_empty() {
                return true;
            }
            ["Class", "YachtName", "Builder"].iter().any(|k| {
                b.get(*k)
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_lowercase().contains(needle))
                    .unwrap_or(false)
            })
        })
        .filter_map(hit_from_cert)
        .collect()
}

fn hit_from_cert(b: &serde_json::Value) -> Option<OrcHit> {
    let s = |k: &str| b.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let f = |k: &str| b.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let pol = allowances_to_pol(b.get("Allowances")?)?;
    Some(OrcHit {
        name: s("YachtName"),
        class: s("Class"),
        builder: s("Builder"),
        year: b
            .get("Age_Year")
            .map(|v| v.to_string().trim_matches('"').to_string())
            .unwrap_or_default(),
        loa_m: f("LOA"),
        beam_m: f("MB"),
        draft_m: f("Draft"),
        pol,
    })
}

/// ORC allowances → `.pol` grid.
///
/// Allowances are seconds per nautical mile, so speed is 3600/t. The R-rows
/// give the fixed angles; the beat optimum becomes one row at the mean beat
/// angle with each column's own VMG divided out (vmg/cos θ — the boat's
/// actual speed through the water on that angle), and the deep rows DW165
/// and DW180 land as-is. R150 and DW150 disagree by sail choice; the boat
/// carries the better sail, so the row keeps the faster of the two.
fn allowances_to_pol(al: &serde_json::Value) -> Option<String> {
    let arr = |k: &str| -> Option<Vec<f64>> {
        Some(
            al.get(k)?
                .as_array()?
                .iter()
                .filter_map(|v| v.as_f64())
                .collect(),
        )
    };
    let ws = arr("WindSpeeds")?;
    let n = ws.len();
    if n == 0 {
        return None;
    }
    let kt = |spm: &f64| if *spm > 1.0 { 3600.0 / spm } else { 0.0 };

    let beat_t = arr("Beat")?;
    let beat_a = arr("BeatAngle")?;
    if beat_t.len() != n || beat_a.len() != n {
        return None;
    }
    let beat_angle = (beat_a.iter().sum::<f64>() / n as f64).round();
    let beat_row: Vec<f64> = beat_t
        .iter()
        .zip(&beat_a)
        .map(|(t, a)| kt(t) / a.to_radians().cos())
        .collect();

    let mut rows: Vec<(f64, Vec<f64>)> = vec![(beat_angle, beat_row)];
    for angle in [52.0, 60.0, 75.0, 90.0, 110.0, 120.0, 135.0] {
        let r = arr(&format!("R{angle:.0}"))?;
        if r.len() != n {
            return None;
        }
        rows.push((angle, r.iter().map(kt).collect()));
    }
    let r150 = arr("R150")?;
    let dw150 = arr("DW150").unwrap_or_else(|| r150.clone());
    rows.push((
        150.0,
        r150.iter()
            .zip(&dw150)
            .map(|(a, b)| kt(a).max(kt(b)))
            .collect(),
    ));
    for (angle, key) in [(165.0, "DW165"), (180.0, "DW180")] {
        if let Some(r) = arr(key) {
            if r.len() == n {
                rows.push((angle, r.iter().map(kt).collect()));
            }
        }
    }
    rows.sort_by(|a, b| a.0.total_cmp(&b.0));
    rows.dedup_by(|a, b| (a.0 - b.0).abs() < 0.5);

    let mut out = String::from("twa/tws");
    for w in &ws {
        out.push_str(&format!("\t{w:.0}"));
    }
    out.push('\n');
    for (angle, speeds) in rows {
        out.push_str(&format!("{angle:.0}"));
        for s in speeds {
            out.push_str(&format!("\t{s:.2}"));
        }
        out.push('\n');
    }
    Some(out)
}

/// A file-system-friendly name for a class: "X-99" → "x-99".
pub fn slug(class: &str) -> String {
    let mut s: String = class
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    s.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real (trimmed) Danish certificate: a Shogun 50.
    const FIXTURE: &str = include_str!("../../tests/fixtures/orc-shogun50.json");

    #[test]
    fn a_certificate_converts_to_a_polar_the_parser_accepts() {
        let doc: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        let hits = parse_and_filter(FIXTURE, "shogun");
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.class, "Shogun 50");
        assert!((hit.loa_m - 15.245).abs() < 1e-9);
        assert!((hit.draft_m - 3.33).abs() < 1e-9);

        // The parser navcore ships must accept the converted text.
        let polar = crate::nav::polar::Polar::parse(&hit.pol).expect("valid .pol");
        // R90 at 10 kt was 378.5 s/nm → 9.51 kt.
        let v = polar.speed_kt(90.0, 10.0).unwrap();
        assert!((v - 3600.0 / 378.5).abs() < 0.05, "{v}");
        // Deep downwind comes from DW180: 673.1 s/nm at 10 kt → 5.35 kt.
        let v = polar.speed_kt(180.0, 10.0).unwrap();
        assert!((v - 3600.0 / 673.1).abs() < 0.05, "{v}");
        // Inside the no-go cone there is no number.
        assert!(polar.speed_kt(20.0, 10.0).is_none());

        // The beat row is boat speed, not VMG: faster than VMG by 1/cos.
        let al = &doc["rms"][0]["Allowances"];
        let vmg10 = 3600.0 / al["Beat"][3].as_f64().unwrap();
        let beat10 = polar.vmg_at(10.0).beat_vmg_kt as f64;
        assert!(
            (beat10 - vmg10).abs() < 0.35,
            "recovered beat VMG {beat10:.2} vs certificate {vmg10:.2}"
        );

        // The search is case-insensitive and misses politely.
        assert_eq!(parse_and_filter(FIXTURE, "SHOGUN".to_lowercase().as_str()).len(), 1);
        assert!(parse_and_filter(FIXTURE, "bavaria").is_empty());
    }

    #[test]
    fn slugs_stay_on_the_filesystem() {
        assert_eq!(slug("X-99"), "x-99");
        assert_eq!(slug("Shogun 50"), "shogun-50");
        assert_eq!(slug("Luffe 40.04 (mod)"), "luffe-40-04-mod");
    }
}
