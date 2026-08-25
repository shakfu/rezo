//! Per-platform application directories.
//!
//! Consolidates the layout that `dotenv.rs` and `model_catalog.rs` both
//! need. The shells still have their own resolvers — `rezon-tui` via
//! `directories`, `rezon-web` via Tauri's `AppHandle` — and those are
//! not touched here; this is the core-side answer for code that has
//! neither.
//!
//! The application id is `rezon-tui` for historical reasons: it is the
//! `ProjectDirs` name the TUI has always used, and the keyring service
//! name besides. Changing it would orphan existing config and secrets.

use std::path::PathBuf;

/// Application identifier used as the directory (and keyring service)
/// name. Stable across versions.
pub const APP_ID: &str = "rezon-tui";

/// Config directory for the current user, or `None` when no home /
/// config location can be determined.
///
/// Not created as a side effect — callers that need to write should
/// `create_dir_all` first, and callers that only read should tolerate
/// its absence.
pub fn config_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| {
            PathBuf::from(h)
                .join("Library")
                .join("Application Support")
                .join(APP_ID)
        })
    } else if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join(APP_ID))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|c| c.join(APP_ID))
    }
}

/// `<config dir>/<name>`, or `None` when there is no config dir.
pub fn config_file(name: &str) -> Option<PathBuf> {
    config_dir().map(|d| d.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_file_sits_under_an_app_named_directory() {
        if let Some(p) = config_file("thing.json") {
            assert_eq!(p.file_name().unwrap(), "thing.json");
            assert!(p.to_string_lossy().contains(APP_ID), "got {p:?}");
        }
    }

    #[test]
    fn app_id_is_the_keyring_service_name() {
        // Both derive from the same historical `ProjectDirs` name.
        // Divergence would orphan either config or saved secrets.
        assert_eq!(APP_ID, "rezon-tui");
    }
}
