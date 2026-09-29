//! What is already on disk, and which shop chart it belongs to.
//!
//! The shop knows what an account owns; only the filesystem knows what has
//! actually been fetched. Matching the two is what lets Manx say "you hold
//! edition 2025/1-20" — and, for a lapsed subscription, what lets it ask for
//! that edition rather than one the licence never covered.

use std::path::{Path, PathBuf};

use super::types::Edition;

/// A chart set found on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct InstalledSet {
    /// The set's family, without the edition: `oeuSENC-DK`.
    pub stem: String,
    /// The edition, read from the set's own `ChartList.XML`.
    pub edition: Edition,
    pub path: PathBuf,
}

/// The family a chart set name belongs to, dropping the edition and anything
/// after it: `oeuSENC-DK-2025-1-20-base-macbook` → `oeuSENC-DK`.
///
/// o-charts names both its directories and its download URLs this way, so the
/// same rule keys an installed directory to the shop's listing. The edition
/// always starts with a four-digit year, which is the only stable boundary —
/// the trailing parts vary (`base`, `update`, the system name) and the leading
/// ones differ per product (`oeuSENC`, `oeSENC`, `oeRNC`).
pub fn set_stem(name: &str) -> Option<String> {
    let segment = name
        .rsplit('/')
        .find(|s| s.contains("SENC") || s.contains("RNC"))?;
    let mut head = Vec::new();
    for part in segment.split('-') {
        if part.len() == 4 && part.chars().all(|c| c.is_ascii_digit()) {
            break;
        }
        head.push(part);
    }
    (!head.is_empty()).then(|| head.join("-"))
}

/// Read the edition a chart set declares.
///
/// `ChartList.XML` carries `<Edition>2025/1-20</Edition>` in the shop's own
/// wording, which beats parsing the directory name: the directory can be
/// renamed, and the file cannot without breaking the set.
fn edition_of(dir: &Path) -> Option<Edition> {
    let xml = std::fs::read_to_string(dir.join("ChartList.XML")).ok()?;
    let open = xml.find("<Edition>")? + "<Edition>".len();
    let close = xml[open..].find("</Edition>")? + open;
    let edition = Edition::parse(xml[open..close].trim());
    edition.is_known().then_some(edition)
}

/// Every chart set under `root`, plus `root` itself if it is one.
///
/// Both cases are real: Manx is usually pointed at a directory of chart
/// sets, but pointing it straight at a single set is the quickest way to open
/// one chart and is what the command line invites.
pub fn scan(root: &Path) -> Vec<InstalledSet> {
    let mut out = Vec::new();
    let mut consider = |dir: &Path| {
        if let (Some(name), Some(edition)) = (
            dir.file_name().and_then(|n| n.to_str()),
            edition_of(dir),
        ) {
            if let Some(stem) = set_stem(name) {
                out.push(InstalledSet {
                    stem,
                    edition,
                    path: dir.to_path_buf(),
                });
            }
        }
    };

    consider(root);
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                consider(&path);
            }
        }
    }
    out
}

/// The newest edition installed for a given family.
pub fn newest_for(sets: &[InstalledSet], stem: &str) -> Option<Edition> {
    sets.iter()
        .filter(|s| s.stem.eq_ignore_ascii_case(stem))
        .map(|s| s.edition)
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_name_reduces_to_its_family() {
        assert_eq!(
            set_stem("oeuSENC-DK-2025-1-20-base-macbook").as_deref(),
            Some("oeuSENC-DK")
        );
        // The same rule on a shop URL, which is why it is one rule.
        assert_eq!(
            set_stem("https://o-charts.org/x/oeSENC-PL-2020/44-2-base.zip").as_deref(),
            Some("oeSENC-PL")
        );
        assert_eq!(set_stem("oeRNC-CRBeast-2021").as_deref(), Some("oeRNC-CRBeast"));
        // Already bare.
        assert_eq!(set_stem("oeuSENC-DK").as_deref(), Some("oeuSENC-DK"));
        // Not a chart set at all.
        assert_eq!(set_stem("Downloads"), None);
        assert_eq!(set_stem(""), None);
    }

    fn write_set(root: &Path, name: &str, edition: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("ChartList.XML"),
            format!("<chartList><Chart><Edition>{edition}</Edition></Chart></chartList>"),
        )
        .unwrap();
        dir
    }

    #[test]
    fn sets_are_found_under_a_root_and_as_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        write_set(tmp.path(), "oeuSENC-DK-2025-1-20-base-macbook", "2025/1-20");
        write_set(tmp.path(), "oeuSENC-NO-2026-2-3-base-macbook", "2026/2-3");
        std::fs::create_dir_all(tmp.path().join("not-a-chart-set")).unwrap();

        let found = scan(tmp.path());
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(
            newest_for(&found, "oeuSENC-DK"),
            Some(Edition::parse("2025/1-20"))
        );
        assert_eq!(newest_for(&found, "oeuSENC-XX"), None);

        // Pointed straight at one set, as the command line invites.
        let one = scan(&tmp.path().join("oeuSENC-DK-2025-1-20-base-macbook"));
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].edition, Edition::parse("2025/1-20"));
    }

    #[test]
    fn the_newest_edition_wins_when_two_are_installed() {
        let tmp = tempfile::tempdir().unwrap();
        write_set(tmp.path(), "oeuSENC-DK-2025-1-20-base-macbook", "2025/1-20");
        write_set(tmp.path(), "oeuSENC-DK-2025-1-24-base-macbook", "2025/1-24");
        let found = scan(tmp.path());
        assert_eq!(
            newest_for(&found, "oeuSENC-DK"),
            Some(Edition::parse("2025/1-24"))
        );
    }

    #[test]
    fn a_directory_without_a_chart_list_is_not_a_chart_set() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("oeuSENC-DK-2025-1-20-base")).unwrap();
        assert!(scan(tmp.path()).is_empty());
    }
}
