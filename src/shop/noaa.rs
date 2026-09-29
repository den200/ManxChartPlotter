//! Free charts from NOAA: the United States' official ENCs, one package per
//! state, republished weekly.
//!
//! A package is a zip holding an `ENC_ROOT` of S-57 cells. It is unpacked
//! into `<chart folder>/noaa/<STATE>/`, where the chart catalogue finds S-57
//! cells in subfolders — so NOAA charts sit beside o-charts cells in the one
//! folder, with nothing else to manage. A small `manx-noaa.json` beside
//! the cells records what was installed and when, which is what "Update
//! available" is worked out from.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// One downloadable package.
#[derive(Debug, Clone, Copy)]
pub struct Region {
    pub code: &'static str,
    pub name: &'static str,
}

/// The packages NOAA publishes, by state (and Puerto Rico).
pub const REGIONS: &[Region] = &[
    Region { code: "AL", name: "Alabama" },
    Region { code: "AK", name: "Alaska" },
    Region { code: "CA", name: "California" },
    Region { code: "CT", name: "Connecticut" },
    Region { code: "DE", name: "Delaware" },
    Region { code: "FL", name: "Florida" },
    Region { code: "GA", name: "Georgia" },
    Region { code: "HI", name: "Hawaii" },
    Region { code: "IL", name: "Illinois" },
    Region { code: "IN", name: "Indiana" },
    Region { code: "LA", name: "Louisiana" },
    Region { code: "ME", name: "Maine" },
    Region { code: "MD", name: "Maryland" },
    Region { code: "MA", name: "Massachusetts" },
    Region { code: "MI", name: "Michigan" },
    Region { code: "MN", name: "Minnesota" },
    Region { code: "MS", name: "Mississippi" },
    Region { code: "NH", name: "New Hampshire" },
    Region { code: "NJ", name: "New Jersey" },
    Region { code: "NY", name: "New York" },
    Region { code: "NC", name: "North Carolina" },
    Region { code: "OH", name: "Ohio" },
    Region { code: "OR", name: "Oregon" },
    Region { code: "PA", name: "Pennsylvania" },
    Region { code: "PR", name: "Puerto Rico" },
    Region { code: "RI", name: "Rhode Island" },
    Region { code: "SC", name: "South Carolina" },
    Region { code: "TX", name: "Texas" },
    Region { code: "VA", name: "Virginia" },
    Region { code: "WA", name: "Washington" },
    Region { code: "WI", name: "Wisconsin" },
];

/// Where a package comes from.
pub fn zip_url(code: &str) -> String {
    format!("https://www.charts.noaa.gov/ENCs/{code}_ENCs.zip")
}

/// What NOAA currently offers for a package.
#[derive(Debug, Clone, PartialEq)]
pub struct Remote {
    pub size: u64,
    /// The server's Last-Modified, verbatim — compared, never parsed.
    pub last_modified: String,
}

/// What is installed for a package.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Installed {
    /// The day it was downloaded, `YYYY-MM-DD`.
    pub downloaded: String,
    /// The package's Last-Modified when it was downloaded.
    pub last_modified: String,
}

const MARKER: &str = "manx-noaa.json";

/// A HEAD is small: 20 seconds for all of it.
fn probe_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(20)))
        .build()
        .into()
}

/// How long one request of a download may run. ureq has no idle timeout,
/// and its "receive response" limit keeps running through the body — a
/// 30-second one cut every package off at 30 seconds, which on a slow link
/// is a few megabytes in. So each request is given a minute and the
/// download carries on from where it stopped with a Range request: a slow
/// link finishes in several pieces, and a dead one is noticed within a
/// minute rather than never.
const DOWNLOAD_CALL: std::time::Duration = std::time::Duration::from_secs(60);

/// Requests in a row that may bring nothing before the download gives up.
const MAX_STALLS: u32 = 3;

fn download_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(20)))
        .timeout_per_call(Some(DOWNLOAD_CALL))
        .build()
        .into()
}

fn header(resp: &ureq::http::Response<ureq::Body>, name: &str) -> Option<String> {
    resp.headers().get(name)?.to_str().ok().map(str::to_string)
}

/// The folder a package lives in.
pub fn region_dir(root: &Path, code: &str) -> PathBuf {
    root.join("noaa").join(code)
}

/// Ask NOAA for a package's size and date.
pub fn probe(code: &str) -> Result<Remote, String> {
    let resp = probe_agent()
        .head(&zip_url(code))
        .call()
        .map_err(|e| e.to_string())?;
    let h = resp.headers();
    let size = h
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let last_modified = h
        .get("last-modified")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    Ok(Remote { size, last_modified })
}

/// What is installed for a package under `root`, if anything.
pub fn installed(root: &Path, code: &str) -> Option<Installed> {
    let text = std::fs::read_to_string(region_dir(root, code).join(MARKER)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Download and unpack a package into `root`, replacing any earlier copy
/// only once the new one is complete. `progress` gets bytes so far and the
/// total; `cancel` stops the transfer between chunks.
pub fn install(
    root: &Path,
    code: &str,
    mut progress: impl FnMut(u64, u64),
    cancel: &AtomicBool,
) -> Result<Installed, String> {
    let base = root.join("noaa");
    std::fs::create_dir_all(&base).map_err(|e| format!("cannot create {}: {e}", base.display()))?;
    let part = base.join(format!(".{code}.zip.part"));
    let staging = base.join(format!(".{code}.new"));
    let cleanup = || {
        let _ = std::fs::remove_file(&part);
        let _ = std::fs::remove_dir_all(&staging);
    };

    // The package, to a file: some are hundreds of megabytes.
    let last_modified = match fetch(code, &part, &mut progress, cancel) {
        Ok(v) => v,
        Err(e) => {
            cleanup();
            return Err(e);
        }
    };

    // Unpack beside the old copy.
    let _ = std::fs::remove_dir_all(&staging);
    if let Err(e) = unpack(&part, &staging) {
        cleanup();
        return Err(e);
    }
    if crate::senc::find_s57_cells(&staging, 5).is_empty() {
        cleanup();
        return Err("the package held no charts".into());
    }
    let record = Installed {
        downloaded: chrono::Local::now().format("%Y-%m-%d").to_string(),
        last_modified,
    };
    let _ = std::fs::write(
        staging.join(MARKER),
        serde_json::to_string_pretty(&record).unwrap_or_default(),
    );

    // Swap it in.
    let dest = region_dir(root, code);
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::rename(&staging, &dest).map_err(|e| {
        cleanup();
        format!("cannot move the charts into place: {e}")
    })?;
    let _ = std::fs::remove_file(&part);
    Ok(record)
}

/// Download a package to `part`, resuming after a request that ends early.
/// Returns its Last-Modified.
fn fetch(
    code: &str,
    part: &Path,
    progress: &mut impl FnMut(u64, u64),
    cancel: &AtomicBool,
) -> Result<String, String> {
    let agent = download_agent();
    let mut out = std::fs::File::create(part).map_err(|e| e.to_string())?;
    let mut done = 0u64;
    let mut total = 0u64;
    let mut last_modified = String::new();
    let mut stalls = 0;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let mut req = agent.get(&zip_url(code));
        if done > 0 {
            req = req.header("Range", &format!("bytes={done}-"));
            // A package republished meanwhile comes back whole (200), not
            // as the rest of the old one.
            if !last_modified.is_empty() {
                req = req.header("If-Range", &last_modified);
            }
        }
        let before = done;
        let result = req.call();
        let mut last_error = None;
        match result {
            Err(e) => last_error = Some(e.to_string()),
            Ok(mut resp) => {
                let resumed = resp.status() == 206;
                if !resumed && done > 0 {
                    // The server sent the whole file again: start over.
                    out = std::fs::File::create(part).map_err(|e| e.to_string())?;
                    done = 0;
                }
                if !resumed {
                    total = header(&resp, "content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
                    last_modified = header(&resp, "last-modified").unwrap_or_default();
                }
                let mut reader = resp.body_mut().as_reader();
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        return Err("cancelled".into());
                    }
                    let n = match reader.read(&mut buf) {
                        Ok(n) => n,
                        Err(e) => {
                            last_error = Some(e.to_string());
                            break;
                        }
                    };
                    if n == 0 {
                        break;
                    }
                    out.write_all(&buf[..n]).map_err(|e| format!("cannot write the download: {e}"))?;
                    done += n as u64;
                    progress(done, total);
                }
            }
        }
        if total > 0 && done >= total {
            break;
        }
        if total == 0 && last_error.is_none() && done > 0 {
            // No length given: the end of the body is the end.
            break;
        }
        if done > before {
            stalls = 0;
        } else {
            stalls += 1;
            if stalls >= MAX_STALLS {
                let why = last_error.unwrap_or_else(|| "no data".into());
                return Err(format!("the connection to NOAA stalled ({why})"));
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        log::info!("NOAA {code}: resuming at {done} of {total} bytes");
    }
    if total > 0 && done != total {
        return Err(format!("download ended early ({done} of {total} bytes)"));
    }
    Ok(last_modified)
}

/// Unzip a package, keeping only what sits under `ENC_ROOT/`.
fn unpack(zip_path: &Path, dest: &Path) -> Result<(), String> {
    let file = std::fs::File::open(zip_path).map_err(|e| e.to_string())?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("the download is not a valid package: {e}"))?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        if entry.is_dir() {
            continue;
        }
        // `enclosed_name` refuses absolute paths and `..`: a package must
        // not write outside its folder.
        let Some(name) = entry.enclosed_name() else { continue };
        let parts: Vec<_> = name.components().collect();
        let Some(at) = parts.iter().position(|c| c.as_os_str() == "ENC_ROOT") else {
            continue;
        };
        let rel: PathBuf = parts[at + 1..].iter().collect();
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(rel);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut f = std::fs::File::create(&out).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut f).map_err(|e| format!("unpacking {}: {e}", out.display()))?;
    }
    Ok(())
}

/// Remove an installed package.
pub fn remove(root: &Path, code: &str) -> Result<(), String> {
    std::fs::remove_dir_all(region_dir(root, code)).map_err(|e| e.to_string())
}

/// Whether NOAA has a newer package than the one installed.
pub fn update_available(installed: &Installed, remote: &Remote) -> bool {
    !remote.last_modified.is_empty() && remote.last_modified != installed.last_modified
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A package is unpacked from its ENC_ROOT down, and nothing outside
    /// ENC_ROOT — or outside the folder — is written.
    #[test]
    fn a_package_unpacks_its_enc_root_only() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("t.zip");
        {
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut z = zip::ZipWriter::new(f);
            let opt = zip::write::SimpleFileOptions::default();
            z.start_file("ENC_ROOT/US5XX1AA/US5XX1AA.000", opt).unwrap();
            z.write_all(b"cell").unwrap();
            z.start_file("ENC_ROOT/US5XX1AA/US5XX1AA.001", opt).unwrap();
            z.write_all(b"update").unwrap();
            z.start_file("readme.txt", opt).unwrap();
            z.write_all(b"not a chart").unwrap();
            z.finish().unwrap();
        }
        let dest = dir.path().join("out");
        unpack(&zip_path, &dest).unwrap();
        assert!(dest.join("US5XX1AA/US5XX1AA.000").exists());
        assert!(dest.join("US5XX1AA/US5XX1AA.001").exists());
        assert!(!dest.join("readme.txt").exists());
    }

    #[test]
    fn update_is_offered_only_for_a_newer_package() {
        let inst = Installed { downloaded: "2026-09-01".into(), last_modified: "A".into() };
        assert!(!update_available(&inst, &Remote { size: 1, last_modified: "A".into() }));
        assert!(update_available(&inst, &Remote { size: 1, last_modified: "B".into() }));
        assert!(!update_available(&inst, &Remote { size: 1, last_modified: String::new() }));
    }

    #[test]
    fn regions_are_unique_and_sorted_by_name() {
        let names: Vec<_> = REGIONS.iter().map(|r| r.name).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        let mut codes: Vec<_> = REGIONS.iter().map(|r| r.code).collect();
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), REGIONS.len());
    }
}
