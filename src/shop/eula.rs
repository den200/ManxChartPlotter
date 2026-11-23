//! The o-charts licence agreement (EULA): shown, accepted, and on hand.
//!
//! o-charts' conditions for programs derived from OpenCPN (October 2026):
//!
//! > The enduser has to accept the licensing conditions as presented by the
//! > EULA, which comes with every o-chart set before the install and use of
//! > the licensed chart material. The enduser shall be able to retrieve the
//! > EULA at any moment from within the system. The enduser shall explicitly
//! > understand that o-charts sets are not official charts under carriage
//! > requirements.
//!
//! Every chart set carries the agreement as an HTML file beside its cells
//! (`EN_rrc_eula_ChartSetsForOpenCPN.html`). Acceptance is recorded against
//! the text itself — a set that arrives with a changed agreement asks again
//! — and no o-charts cell is decrypted until the agreement of its set has
//! been accepted: [`check`] is called by the one function every decryption
//! goes through.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

/// Where o-charts publishes the agreement.
pub const URL: &str = "https://o-charts.org/manuals/docs/EN_rrc_eula_ChartSetsForOpenCPN.html";

/// Said with the agreement wherever it is accepted.
pub const NOT_OFFICIAL: &str = "o-charts chart sets are not official charts. They cannot be used \
     to meet carriage requirements: carry the official charts and publications your boat is \
     required to carry.";

/// What is accepted when a set came without its agreement file: the
/// agreement as published at [`URL`].
const PUBLISHED: &str = "published";

/// An agreement waiting for the user's word.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    /// The chart set's folder name, for the title.
    pub set: String,
    /// What acceptance is recorded against.
    pub id: String,
    /// The agreement, as plain text.
    pub text: String,
}

fn accepted() -> &'static RwLock<HashSet<String>> {
    static ACCEPTED: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();
    ACCEPTED.get_or_init(|| {
        let saved = settings_file()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .map(|text| ids_in_settings(&text))
            .unwrap_or_default();
        RwLock::new(saved.into_iter().collect())
    })
}

fn settings_file() -> Option<PathBuf> {
    Some(crate::paths::config_dir()?.join("settings.json"))
}

/// The accepted agreements recorded in a settings file.
pub fn ids_in_settings(text: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| {
            Some(
                v.get("ocharts_eula")?
                    .as_array()?
                    .iter()
                    .filter_map(|id| id.as_str().map(str::to_string))
                    .collect(),
            )
        })
        .unwrap_or_default()
}

/// Every agreement accepted so far, for the settings file.
pub fn accepted_ids() -> Vec<String> {
    let mut ids: Vec<String> = accepted().read().unwrap().iter().cloned().collect();
    ids.sort();
    ids
}

/// Record that the user accepted this agreement.
pub fn accept(id: &str) {
    accepted().write().unwrap().insert(id.to_string());
}

/// The agreement file in a chart set's folder, English first.
pub fn file_in(set_dir: &Path) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(set_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_lowercase();
            name.contains("eula") && (name.ends_with(".html") || name.ends_with(".htm") || name.ends_with(".txt"))
        })
        .collect();
    found.sort_by_key(|p| {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        (!name.starts_with("EN_"), name)
    });
    found.into_iter().next()
}

/// What acceptance of an agreement is recorded against: its bytes.
fn id_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().take(12).map(|b| format!("{b:02x}")).collect()
}

/// The agreement of the chart set in `set_dir`, with what it is accepted
/// against. A set without the file is covered by the published agreement.
fn agreement(set_dir: &Path) -> (String, String) {
    match file_in(set_dir).and_then(|f| std::fs::read(f).ok()) {
        Some(bytes) => (id_of(&bytes), text_from_html(&String::from_utf8_lossy(&bytes))),
        None => (
            PUBLISHED.to_string(),
            format!(
                "This chart set came without its licence agreement file. The agreement that \
                 applies is the one o-charts publishes at:\n\n{URL}\n\nRead it there before \
                 accepting."
            ),
        ),
    }
}

/// The agreement of a set, to read: its name and text.
pub fn read(set_dir: &Path) -> (String, String) {
    (set_name(set_dir), agreement(set_dir).1)
}

fn set_name(set_dir: &Path) -> String {
    set_dir.file_name().and_then(|n| n.to_str()).unwrap_or("o-charts").to_string()
}

fn holds_licensed_cells(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| entries.flatten().any(|e| is_licensed_cell(&e.path())))
}

/// An o-charts cell, by its file name.
pub fn is_licensed_cell(path: &Path) -> bool {
    path.extension().is_some_and(|x| x.eq_ignore_ascii_case("oesu"))
}

/// Whether a chart file or folder named on the command line is o-charts
/// material.
pub fn is_licensed_source(path: &Path) -> bool {
    is_licensed_cell(path) || holds_licensed_cells(path)
}

/// Why the developer tools that write chart content or a picture of the
/// chart to a file will not do it for o-charts.
///
/// o-charts' conditions: "Storing unencrypted data on any medium […] is
/// strictly prohibited", and when the program ends "no unencrypted chart
/// material or renderings shall remain on the user system". A dump or a
/// capture is exactly that, so they work on free charts only.
pub const NO_COPIES: &str = "o-charts charts are licensed on condition that no unencrypted chart \
     data or rendering of them is stored. This works with free charts (NOAA ENCs) only.";

/// The agreement still to be accepted before the charts in `dir` are used,
/// if it holds o-charts cells and theirs has not been.
pub fn pending(dir: &Path) -> Option<Pending> {
    if !holds_licensed_cells(dir) {
        return None;
    }
    let (id, text) = agreement(dir);
    if accepted().read().unwrap().contains(&id) {
        return None;
    }
    Some(Pending { set: set_name(dir), id, text })
}

/// Whether an o-charts cell may be decrypted: only once the agreement of
/// its set has been accepted. `Err` names the set.
pub fn check(cell: &Path) -> Result<(), String> {
    let dir = cell.parent().unwrap_or(Path::new("."));
    // One answer per folder: a set is several hundred cells.
    static CHECKED: OnceLock<RwLock<HashSet<PathBuf>>> = OnceLock::new();
    let checked = CHECKED.get_or_init(Default::default);
    if checked.read().unwrap().contains(dir) {
        return Ok(());
    }
    let (id, _) = agreement(dir);
    if accepted().read().unwrap().contains(&id) {
        checked.write().unwrap().insert(dir.to_path_buf());
        Ok(())
    } else {
        Err(set_name(dir))
    }
}

/// An HTML page as text to read: paragraphs and headings on their own
/// lines, list items marked, everything else unwrapped.
pub fn text_from_html(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    // Scripts and styles are not text.
    let mut skip_until: Option<&str> = None;
    while !rest.is_empty() {
        if let Some(end) = skip_until {
            match rest.to_ascii_lowercase().find(end) {
                Some(at) => {
                    rest = &rest[at + end.len()..];
                    skip_until = None;
                }
                None => break,
            }
            continue;
        }
        let Some(open) = rest.find('<') else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('>') else { break };
        let tag = rest[open + 1..open + close].trim().to_ascii_lowercase();
        rest = &rest[open + close + 1..];
        let name: String = tag.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
        match name.as_str() {
            "script" if !tag.starts_with('/') => skip_until = Some("</script>"),
            "style" if !tag.starts_with('/') => skip_until = Some("</style>"),
            "head" if !tag.starts_with('/') => skip_until = Some("</head>"),
            "li" if !tag.starts_with('/') => out.push_str("\n• "),
            "br" => out.push('\n'),
            "p" | "div" | "h1" | "h2" | "h3" | "h4" | "h5" | "tr" | "ul" | "ol" | "table" => out.push_str("\n\n"),
            _ => {}
        }
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&copy;", "©");
    // One space between words, one blank line between paragraphs.
    let mut text = String::new();
    let mut blank = 0;
    for line in out.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank += 1;
            continue;
        }
        if !text.is_empty() {
            text.push_str(if blank > 0 { "\n\n" } else { " " });
        }
        // A list item starts its own line even inside a paragraph.
        if line.starts_with('•') && !text.is_empty() && !text.ends_with("\n\n") {
            text.truncate(text.trim_end().len());
            text.push('\n');
        }
        text.push_str(&line);
        blank = 0;
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_becomes_text_to_read() {
        let html = "<html><head><title>EULA</title><style>p { x: 1 }</style></head><body>\
            <h2>1. Purpose</h2>\n<p>You are   licensed to use\nthis Chart&nbsp;Set &amp; no more.</p>\
            <ul><li>One install</li><li>One back-up</li></ul><script>alert(1)</script><p>End.</p></body></html>";
        assert_eq!(
            text_from_html(html),
            "1. Purpose\n\nYou are licensed to use this Chart Set & no more.\n\n• One install\n• One back-up\n\nEnd."
        );
    }

    /// A folder of o-charts cells asks for its agreement until that text is
    /// accepted, and again when a set arrives with a different text. A
    /// folder with no o-charts cells asks for nothing.
    #[test]
    fn a_set_asks_until_its_own_agreement_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let set = dir.path().join("oeuSENC-XX-2026-1");
        std::fs::create_dir_all(&set).unwrap();
        assert_eq!(pending(&set), None, "no cells, nothing to accept");

        let cell = set.join("OC-1-A.oesu");
        std::fs::write(&cell, b"x").unwrap();
        // Unique text, so this test's acceptance cannot meet another's.
        let eula = set.join("EN_rrc_eula_ChartSetsForOpenCPN.html");
        std::fs::write(&eula, format!("<p>Terms {:?}</p>", dir.path())).unwrap();
        std::fs::write(set.join("DE_rrc_eula.html"), "<p>Bedingungen</p>").unwrap();
        assert!(is_licensed_source(&set) && is_licensed_source(&cell));
        assert!(!is_licensed_source(dir.path()) && !is_licensed_source(&set.join("US5BOSBE.000")));
        assert_eq!(file_in(&set), Some(eula.clone()), "English first");

        let asked = pending(&set).expect("the agreement");
        assert_eq!(asked.set, "oeuSENC-XX-2026-1");
        assert!(asked.text.starts_with("Terms "));
        assert_eq!(check(&cell), Err("oeuSENC-XX-2026-1".to_string()));

        accept(&asked.id);
        assert_eq!(pending(&set), None);
        assert_eq!(check(&cell), Ok(()));
        assert!(accepted_ids().contains(&asked.id));

        // A new edition with changed terms, in its own folder.
        let next = dir.path().join("oeuSENC-XX-2027-1");
        std::fs::create_dir_all(&next).unwrap();
        std::fs::write(next.join("OC-1-A.oesu"), b"x").unwrap();
        std::fs::write(next.join("EN_eula.html"), format!("<p>New terms {:?}</p>", dir.path())).unwrap();
        assert!(pending(&next).is_some());
        assert!(check(&next.join("OC-1-A.oesu")).is_err());
    }

    #[test]
    fn accepted_agreements_are_read_back_from_the_settings() {
        assert_eq!(
            ids_in_settings(r#"{"display":{},"ocharts_eula":["abc","def"]}"#),
            vec!["abc".to_string(), "def".to_string()]
        );
        assert!(ids_in_settings(r#"{"display":{}}"#).is_empty());
        assert!(ids_in_settings("not json").is_empty());
    }
}
