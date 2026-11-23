//! Where Manx keeps its files — and the move from the name it had before.
//!
//! Manx was called navcore, and every folder it made carries that name:
//! settings, routes, the logbook, the o-charts session, forecast caches,
//! downloaded charts. The first time a folder is asked for under the new
//! name and only the old one exists, it is renamed in place, so nobody
//! loses a route or a logbook to a rename they had no part in.

use std::path::{Path, PathBuf};

/// The folder name under each base directory.
pub const APP: &str = "manx";
/// The name the folders had before.
const OLD: &str = "navcore";

/// `<base>/manx`, first moving `<base>/navcore` there when that is the only
/// one. If the move fails, the old folder is used as it is: better the old
/// name than an empty plotter.
fn under(base: PathBuf) -> PathBuf {
    let new = base.join(APP);
    let old = base.join(OLD);
    if !new.exists() && old.is_dir() {
        match std::fs::rename(&old, &new) {
            Ok(()) => log::info!("moved {} to {}", old.display(), new.display()),
            Err(e) => {
                log::warn!("could not move {} to {}: {e}", old.display(), new.display());
                return old;
            }
        }
    }
    new
}

/// Settings, routes, logbook, caches of forecasts.
pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(under)
}

/// Charts Manx downloads itself.
pub fn data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(under)
}

/// `~/.cache/manx`: decrypted-chart cache.
pub fn cache_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| under(h.join(".cache")))
}

/// A path remembered from before the rename, pointing into one of the old
/// folders that has since moved: where it lives now. Anything else comes
/// back unchanged.
pub fn moved(path: &Path) -> PathBuf {
    for base in [dirs::config_dir(), dirs::data_dir(), dirs::home_dir().map(|h| h.join(".cache"))]
        .into_iter()
        .flatten()
    {
        if let Some(now) = moved_under(path, base) {
            return now;
        }
    }
    path.to_path_buf()
}

/// `path` if it lies in `<base>/navcore`: where that is now, moving the
/// folder first if it has not moved yet.
///
/// The move is made here, not left for whoever asks for the folder next.
/// On a Pi's first start after the rename the remembered chart folder was
/// read while the old folder still stood, the folder was then moved by a
/// later call, and the charts were looked for where they no longer were.
fn moved_under(path: &Path, base: PathBuf) -> Option<PathBuf> {
    let rest = path.strip_prefix(base.join(OLD)).ok()?.to_path_buf();
    let now = under(base).join(rest);
    Some(if now.exists() || !path.exists() { now } else { path.to_path_buf() })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chart folder remembered inside the old data folder, read before
    /// anything has moved that folder: the folder moves there and then,
    /// and the answer is where the charts are afterwards.
    #[test]
    fn a_remembered_path_moves_its_folder_before_answering() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        let old = base.join(OLD).join("charts");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("cell.000"), "x").unwrap();
        let now = moved_under(&old, base.clone()).expect("inside the old folder");
        assert_eq!(now, base.join(APP).join("charts"));
        assert!(now.join("cell.000").exists());
        assert!(!old.exists());
        // Asked again, with the move long done.
        assert_eq!(moved_under(&old, base.clone()), Some(now));
        // Somewhere else entirely is not ours to answer for.
        assert_eq!(moved_under(Path::new("/media/usb/charts"), base), None);
    }

    #[test]
    fn an_old_folder_moves_once_and_keeps_its_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        std::fs::create_dir_all(base.join(OLD).join("routes")).unwrap();
        std::fs::write(base.join(OLD).join("settings.json"), r#"{"palette":"Dusk"}"#).unwrap();
        std::fs::write(base.join(OLD).join("routes/anholt.gpx"), "<gpx/>").unwrap();

        let dir = under(base.clone());
        assert_eq!(dir, base.join(APP));
        // The contents, not merely the names: a migration that created empty
        // files where the user's data had been would pass an existence check.
        assert_eq!(
            std::fs::read_to_string(dir.join("settings.json")).unwrap(),
            r#"{"palette":"Dusk"}"#
        );
        assert_eq!(std::fs::read_to_string(dir.join("routes/anholt.gpx")).unwrap(), "<gpx/>");
        assert!(!base.join(OLD).exists());

        // Both present (someone ran an old build after the move): the new one
        // wins and the old one is left alone — not merged, not deleted.
        std::fs::create_dir_all(base.join(OLD)).unwrap();
        std::fs::write(base.join(OLD).join("settings.json"), "{}").unwrap();
        assert_eq!(under(base.clone()), base.join(APP));
        assert_eq!(std::fs::read_to_string(base.join(OLD).join("settings.json")).unwrap(), "{}");
        assert_eq!(
            std::fs::read_to_string(base.join(APP).join("settings.json")).unwrap(),
            r#"{"palette":"Dusk"}"#
        );
    }

    #[test]
    fn a_fresh_install_just_gets_the_new_name() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(under(tmp.path().to_path_buf()), tmp.path().join(APP));
        // Asking does not create anything; the caller makes the folder when
        // it first has something to write.
        assert!(!tmp.path().join(APP).exists());
        assert!(!tmp.path().join(OLD).exists());
    }

    /// When the move cannot be made, the old folder is used where it is:
    /// "better the old name than an empty plotter".
    #[cfg(unix)]
    #[test]
    fn a_move_that_fails_keeps_using_the_old_folder() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("locked");
        std::fs::create_dir_all(base.join(OLD)).unwrap();
        std::fs::write(base.join(OLD).join("logbook.json"), "[1]").unwrap();
        // A base folder nobody may write in: the rename is refused.
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o555)).unwrap();

        let dir = under(base.clone());
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(dir, base.join(OLD));
        assert_eq!(std::fs::read_to_string(dir.join("logbook.json")).unwrap(), "[1]");
        assert!(!base.join(APP).exists());
    }

    /// A path saved in settings before the rename follows its folder.
    ///
    /// `moved` reads the real base directories, but only to compare paths
    /// against them: the one path given here cannot exist, and nothing is
    /// created, read or written under the user's folders.
    #[test]
    fn a_remembered_path_follows_the_rename() {
        let Some(config) = dirs::config_dir() else { return };
        let rest = format!("routes/no-such-route-{}.gpx", std::process::id());
        assert_eq!(moved(&config.join(OLD).join(&rest)), config.join(APP).join(&rest));

        // A path that exists is left alone, wherever it is.
        let tmp = tempfile::tempdir().unwrap();
        let here = tmp.path().join(OLD).join("kept.txt");
        std::fs::create_dir_all(here.parent().unwrap()).unwrap();
        std::fs::write(&here, "x").unwrap();
        assert_eq!(moved(&here), here);

        // So is one under no folder of ours, even if it has the old name in it.
        let elsewhere = tmp.path().join(OLD).join("missing.txt");
        assert_eq!(moved(&elsewhere), elsewhere);
    }
}
