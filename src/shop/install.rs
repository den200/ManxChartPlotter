//! Unpack a downloaded chart set onto disk.
//!
//! A package is a zip holding one top-level directory — the chart set — with a
//! `ChartList.XML` and one encrypted cell per chart. The keys arrive separately
//! and are written beside them; Manx's catalogue reads the pair exactly as
//! it reads the sets already installed.

use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};

/// What an install produced.
#[derive(Debug, Clone)]
pub struct Installed {
    /// The chart set's directory name, e.g. `oeuSENC-DK-2025-1-20-base-macbook`.
    pub set_name: String,
    pub path: PathBuf,
    /// How many cells were written.
    pub cells: usize,
}

#[derive(Debug)]
pub enum InstallError {
    Zip(String),
    Io(std::io::Error),
    /// The package was not shaped like a chart set.
    Shape(String),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallError::Zip(e) => write!(f, "the package could not be opened: {e}"),
            InstallError::Io(e) => write!(f, "writing the charts failed: {e}"),
            InstallError::Shape(e) => write!(f, "the package is not a chart set: {e}"),
        }
    }
}

impl From<std::io::Error> for InstallError {
    fn from(e: std::io::Error) -> Self {
        InstallError::Io(e)
    }
}

/// Where an archive entry may be written, relative to `root`.
///
/// `None` for anything that would escape: an absolute path, a `..`, a Windows
/// drive prefix. A zip is a file from the internet, and an entry named
/// `../../.ssh/authorized_keys` is a well-known way to turn "unpack this" into
/// "write anywhere". The archive is from o-charts over TLS, which is a reason
/// to expect it to be well formed and no reason at all to rely on it.
fn safe_join(root: &Path, entry: &str) -> Option<PathBuf> {
    let entry = entry.replace('\\', "/");
    let rel = Path::new(&entry);
    let mut out = root.to_path_buf();
    for part in rel.components() {
        match part {
            Component::Normal(p) => {
                let s = p.to_str()?;
                if s.is_empty() || s == "." {
                    continue;
                }
                out.push(s);
            }
            // A leading "./" is harmless noise; everything else is an escape
            // or a root, and neither belongs in a relative archive entry.
            Component::CurDir => {}
            _ => return None,
        }
    }
    (out != root).then_some(out)
}

/// Unpack `zip_bytes` under `root`, writing `keys_xml` beside the cells.
///
/// `progress` reports entries written and the total.
pub fn install(
    zip_bytes: &[u8],
    keys_xml: &[u8],
    root: &Path,
    mut progress: impl FnMut(usize, usize),
) -> Result<Installed, InstallError> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(zip_bytes)).map_err(|e| InstallError::Zip(e.to_string()))?;

    // The set's directory is the archive's single top-level entry; Manx's
    // catalogue keys off that name, and so does the keylist beside it.
    let mut set_name: Option<String> = None;
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| InstallError::Zip(e.to_string()))?;
        let name = entry.name().replace('\\', "/");
        let top = name.split('/').next().unwrap_or("").to_string();
        if top.is_empty() {
            continue;
        }
        match &set_name {
            None => set_name = Some(top),
            Some(existing) if *existing != top => {
                return Err(InstallError::Shape(format!(
                    "expected one chart set, found both {existing} and {top}"
                )));
            }
            _ => {}
        }
    }
    let set_name = set_name.ok_or_else(|| InstallError::Shape("the package is empty".into()))?;

    // The set name comes from the archive too, so it gets the same treatment as
    // an entry path. It is not enough to vet the entries: every entry of a
    // package whose top-level directory is `..` is refused below, and the name
    // was then still used to build the keylist's path — writing one file a
    // directory above the install root. The keylist is the last thing written
    // and the easiest to overlook.
    let set_dir = safe_join(root, &set_name)
        .filter(|p| p.parent() == Some(root))
        .ok_or_else(|| {
            InstallError::Shape(format!("unusable chart set name {set_name:?}"))
        })?;

    // Unpacked beside the live charts first, and only moved in once the
    // whole package has been read and written. Straight into the live set, a
    // zip error, a full disk or the power going half-way left a ChartList
    // with some of its cells — which then read as an installed edition, with
    // holes in it. Moved file by file rather than as one folder, because an
    // update package holds only the cells that changed and lands on top of
    // the set already there.
    let staging = root.join(format!(".{set_name}.installing"));
    let _ = std::fs::remove_dir_all(&staging);
    let unpacked = unpack(&mut archive, &staging, &set_name, keys_xml, &mut progress)
        .and_then(|cells| move_in(&staging, root).map(|()| cells));
    let _ = std::fs::remove_dir_all(&staging);
    let cells = unpacked?;

    Ok(Installed {
        set_name,
        path: set_dir,
        cells,
    })
}

/// Write every entry of the package, and the keylist, under `staging`.
/// Answers how many chart cells there were.
fn unpack(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    staging: &Path,
    set_name: &str,
    keys_xml: &[u8],
    progress: &mut impl FnMut(usize, usize),
) -> Result<usize, InstallError> {
    let total = archive.len();
    let mut cells = 0usize;
    for i in 0..total {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| InstallError::Zip(e.to_string()))?;
        progress(i + 1, total);
        let Some(path) = safe_join(staging, entry.name()) else {
            // Refused, not fatal: skip it and keep the rest of the set.
            log::warn!("chart package: refusing entry {:?}", entry.name());
            continue;
        };
        if entry.is_dir() {
            std::fs::create_dir_all(&path)?;
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut bytes)?;
        std::fs::write(&path, &bytes)?;
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("oesu") || e.eq_ignore_ascii_case("oernc"))
        {
            cells += 1;
        }
    }

    // The keylist. Manx finds it by scanning the set directory for any XML
    // that is not the ChartList, so the name only has to differ from that.
    let set_dir = staging.join(set_name);
    std::fs::create_dir_all(&set_dir)?;
    std::fs::write(set_dir.join(format!("{set_name}.XML")), keys_xml)?;
    Ok(cells)
}

/// Move everything under `from` to the same place under `to`, replacing
/// files already there and leaving the others alone.
fn move_in(from: &Path, to: &Path) -> Result<(), InstallError> {
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&dest)?;
            move_in(&entry.path(), &dest)?;
        } else {
            std::fs::rename(entry.path(), &dest)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_refuses_everything_that_escapes() {
        let root = Path::new("/charts");
        assert_eq!(
            safe_join(root, "set/cell.oesu"),
            Some(PathBuf::from("/charts/set/cell.oesu"))
        );
        // The classic zip-slip, in both slash flavours.
        assert_eq!(safe_join(root, "../evil"), None);
        assert_eq!(safe_join(root, "set/../../evil"), None);
        assert_eq!(safe_join(root, "..\\evil"), None);
        // Absolute paths are not relative entries.
        assert_eq!(safe_join(root, "/etc/passwd"), None);
        // Nothing at all is not a path either.
        assert_eq!(safe_join(root, ""), None);
        assert_eq!(safe_join(root, "."), None);
        // A harmless "./" prefix is fine.
        assert_eq!(
            safe_join(root, "./set/cell.oesu"),
            Some(PathBuf::from("/charts/set/cell.oesu"))
        );
    }

    fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            for (name, bytes) in entries {
                w.start_file(*name, opts).unwrap();
                w.write_all(bytes).unwrap();
            }
            w.finish().unwrap();
        }
        buf
    }

    #[test]
    fn a_package_unpacks_with_its_keylist_beside_the_cells() {
        let dir = tempfile::tempdir().unwrap();
        let zip = zip_with(&[
            ("oeuSENC-DK-2025-1-20/ChartList.XML", b"<chartList/>"),
            ("oeuSENC-DK-2025-1-20/DK5HRBOL.oesu", b"cell-one"),
            ("oeuSENC-DK-2025-1-20/DK5HRBOM.oesu", b"cell-two"),
        ]);
        let mut reported = Vec::new();
        let done = install(&zip, b"<keyList/>", dir.path(), |n, total| reported.push((n, total))).unwrap();
        assert_eq!(reported, [(1, 3), (2, 3), (3, 3)], "progress counts every entry");

        assert_eq!(done.set_name, "oeuSENC-DK-2025-1-20");
        assert_eq!(done.path, dir.path().join("oeuSENC-DK-2025-1-20"));
        assert_eq!(done.cells, 2);
        assert!(done.path.join("ChartList.XML").exists());
        assert_eq!(
            std::fs::read(done.path.join("DK5HRBOL.oesu")).unwrap(),
            b"cell-one"
        );
        // The keylist is named after the set, which is how the loader tells it
        // apart from the ChartList.
        assert_eq!(
            std::fs::read(done.path.join("oeuSENC-DK-2025-1-20.XML")).unwrap(),
            b"<keyList/>"
        );
    }

    /// A package that cannot be read to the end installs nothing: the set
    /// already on the disk is exactly as it was, with no new cell beside its
    /// old ChartList and nothing left over. (The second entry's bytes are
    /// altered after zipping, so its checksum fails.)
    #[test]
    fn a_package_that_fails_half_way_leaves_the_installed_set_alone() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("charts");
        let set = root.join("oeuSENC-X");
        std::fs::create_dir_all(&set).unwrap();
        std::fs::write(set.join("ChartList.XML"), b"<old/>").unwrap();
        std::fs::write(set.join("old.oesu"), b"old-cell").unwrap();

        let mut zip = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut zip));
            let opts: zip::write::FileOptions<'_, ()> =
                zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
            for (name, bytes) in [
                ("oeuSENC-X/ChartList.XML", &b"<new/>"[..]),
                ("oeuSENC-X/new.oesu", &b"FIRST-NEW-CELL"[..]),
                ("oeuSENC-X/broken.oesu", &b"SECOND-NEW-CELL"[..]),
            ] {
                w.start_file(name, opts).unwrap();
                w.write_all(bytes).unwrap();
            }
            w.finish().unwrap();
        }
        let at = zip.windows(15).position(|w| w == b"SECOND-NEW-CELL").expect("stored as written");
        zip[at] ^= 0xff;

        assert!(install(&zip, b"<keyList/>", &root, |_, _| {}).is_err());
        assert_eq!(names_in(&root), ["oeuSENC-X"], "nothing left beside the set");
        assert_eq!(names_in(&set), ["ChartList.XML", "old.oesu"]);
        assert_eq!(std::fs::read(set.join("ChartList.XML")).unwrap(), b"<old/>");
    }

    /// An update package holds only what changed. It lands on top of the
    /// set: changed files replaced, the rest still there.
    #[test]
    fn an_update_package_lands_on_top_of_the_set_already_there() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("charts");
        let set = root.join("oeuSENC-X");
        std::fs::create_dir_all(&set).unwrap();
        std::fs::write(set.join("ChartList.XML"), b"<old/>").unwrap();
        std::fs::write(set.join("kept.oesu"), b"kept").unwrap();
        std::fs::write(set.join("changed.oesu"), b"before").unwrap();
        let zip = zip_with(&[
            ("oeuSENC-X/ChartList.XML", b"<new/>"),
            ("oeuSENC-X/changed.oesu", b"after"),
        ]);
        install(&zip, b"<keyList/>", &root, |_, _| {}).unwrap();
        assert_eq!(names_in(&root), ["oeuSENC-X"]);
        assert_eq!(names_in(&set), ["ChartList.XML", "changed.oesu", "kept.oesu", "oeuSENC-X.XML"]);
        assert_eq!(std::fs::read(set.join("changed.oesu")).unwrap(), b"after");
        assert_eq!(std::fs::read(set.join("kept.oesu")).unwrap(), b"kept");
        assert_eq!(std::fs::read(set.join("ChartList.XML")).unwrap(), b"<new/>");
    }

    /// Everything directly inside `dir`, sorted, so a stray write beside the
    /// install root shows up as an extra name.
    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn an_escaping_entry_is_skipped_and_the_rest_still_installs() {
        // The install root sits one level down inside the temporary
        // directory, so an escape lands where this test can see it — and not
        // in the system's shared temp folder, which is where `root/../..`
        // pointed before.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("charts");
        std::fs::create_dir_all(&root).unwrap();
        let zip = zip_with(&[
            ("oeuSENC-X/cell.oesu", b"good"),
            ("oeuSENC-X/../../escaped.oesu", b"bad"),
            ("oeuSENC-X\\..\\..\\escaped-too.oesu", b"bad"),
            ("/oeuSENC-X/absolute.oesu", b"bad"),
        ]);
        let done = install(&zip, b"<keyList/>", &root, |_, _| {}).unwrap();
        assert_eq!(done.cells, 1);
        assert_eq!(names_in(dir.path()), ["charts"]);
        assert_eq!(names_in(&root), ["oeuSENC-X"]);
        assert_eq!(
            names_in(&done.path),
            ["cell.oesu", "oeuSENC-X.XML"],
            "only the good cell and the keylist"
        );
    }

    /// A symlink in the archive is written as an ordinary file holding the
    /// link's target, never recreated as a link: a link to `/` followed by
    /// an entry "through" it is the other well-known zip-slip.
    #[test]
    fn an_archived_symlink_is_not_followed_out_of_the_root() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("charts");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            w.start_file("oeuSENC-X/cell.oesu", opts).unwrap();
            w.write_all(b"good").unwrap();
            w.add_symlink("oeuSENC-X/link", outside.display().to_string(), opts)
                .unwrap();
            w.start_file("oeuSENC-X/link/pwned.oesu", opts).unwrap();
            w.write_all(b"bad").unwrap();
            w.finish().unwrap();
        }
        // Whether the install then fails (it cannot make a directory where it
        // wrote the link's file) matters less than where nothing went.
        let _ = install(&buf, b"<keyList/>", &root, |_, _| {});
        assert!(names_in(&outside).is_empty(), "wrote through the link: {:?}", names_in(&outside));
        let link = root.join("oeuSENC-X/link");
        // (A package that fails installs nothing at all, so there may be
        // no such entry to look at.)
        assert!(
            std::fs::symlink_metadata(&link).map_or(true, |m| !m.file_type().is_symlink()),
            "the archive's symlink was recreated as a link"
        );
    }

    #[test]
    fn a_package_whose_set_name_escapes_is_refused_outright() {
        // The entries are refused by `safe_join`, but the *set name* derived
        // from them was once used unchecked to place the keylist — one file,
        // one directory above the install root.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("charts");
        std::fs::create_dir_all(&root).unwrap();
        let zip = zip_with(&[("../pwned.oesu", b"x")]);
        let err = install(&zip, b"<keyList/>", &root, |_, _| {}).unwrap_err();
        assert!(matches!(err, InstallError::Shape(_)), "{err}");
        // Nothing was written beside the install root.
        let siblings: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(siblings, vec![std::ffi::OsString::from("charts")], "{siblings:?}");
    }

    #[test]
    fn two_chart_sets_in_one_package_is_refused_rather_than_half_installed() {
        let dir = tempfile::tempdir().unwrap();
        let zip = zip_with(&[("setA/cell.oesu", b"a"), ("setB/cell.oesu", b"b")]);
        let err = install(&zip, b"<keyList/>", dir.path(), |_, _| {}).unwrap_err();
        assert!(matches!(err, InstallError::Shape(_)), "{err}");
        // Refused before a byte was written, which is the "rather than half
        // installed" — not merely an error after setA went down.
        assert!(names_in(dir.path()).is_empty(), "{:?}", names_in(dir.path()));
    }

    #[test]
    fn something_that_is_not_a_zip_is_refused_as_such() {
        let dir = tempfile::tempdir().unwrap();
        let err = install(b"<html>502</html>", b"<keyList/>", dir.path(), |_, _| {}).unwrap_err();
        assert!(matches!(err, InstallError::Zip(_)), "{err}");
        let err = install(&zip_with(&[]), b"<keyList/>", dir.path(), |_, _| {}).unwrap_err();
        assert!(matches!(err, InstallError::Shape(_)), "{err}");
        assert!(names_in(dir.path()).is_empty());
    }
}
