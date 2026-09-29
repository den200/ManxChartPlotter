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
    if path.exists() {
        return path.to_path_buf();
    }
    for base in [dirs::config_dir(), dirs::data_dir(), dirs::home_dir().map(|h| h.join(".cache"))]
        .into_iter()
        .flatten()
    {
        if let Ok(rest) = path.strip_prefix(base.join(OLD)) {
            return base.join(APP).join(rest);
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_old_folder_moves_once_and_keeps_its_contents() {
        let base = std::env::temp_dir().join(format!("manx-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join(OLD).join("routes")).unwrap();
        std::fs::write(base.join(OLD).join("settings.json"), "{}").unwrap();

        let dir = under(base.clone());
        assert_eq!(dir, base.join(APP));
        assert!(dir.join("settings.json").is_file());
        assert!(dir.join("routes").is_dir());
        assert!(!base.join(OLD).exists());

        // Both present (someone ran an old build after the move): the new one
        // wins and the old one is left alone.
        std::fs::create_dir_all(base.join(OLD)).unwrap();
        assert_eq!(under(base.clone()), base.join(APP));
        assert!(base.join(OLD).is_dir());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_fresh_install_just_gets_the_new_name() {
        let base = std::env::temp_dir().join(format!("manx-paths-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        assert_eq!(under(base.clone()), base.join(APP));
        let _ = std::fs::remove_dir_all(&base);
    }
}
