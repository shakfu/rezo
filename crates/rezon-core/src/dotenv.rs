//! `.env` loading, for development launches.
//!
//! Environment variables remain a first-class way to supply API keys —
//! they are how you run from a terminal, how CI supplies credentials,
//! and how `make dev` picks up whatever is already exported. What they
//! cannot do is serve a *packaged* GUI: an app started from Finder, the
//! dock, or a `.desktop` entry never runs your shell profile, so
//! nothing exported in `~/.zshrc` is visible to it. That is why the
//! keychain sits ahead of the environment in the resolution chain, and
//! why this module exists rather than replacing it.
//!
//! Two locations are consulted, in order, and both are optional:
//!
//!   1. `.env` walking up from the current directory — the development
//!      case, where cwd is the repo.
//!   2. `<app config dir>/.env` — the packaged case, where cwd is
//!      unpredictable and there is no shell profile to read.
//!
//! Existing environment variables always win: `dotenvy`'s non-override
//! load means an explicitly exported key beats a stale `.env`, which is
//! the behaviour anyone debugging a key would expect.

use std::path::{Path, PathBuf};

/// Where a packaged build looks for its `.env`.
///
/// Mirrors the `ProjectDirs` layout the rest of rezon uses. Returns
/// `None` when no config directory can be determined, which is not an
/// error — it just means only the cwd lookup applies.
pub fn config_env_path() -> Option<PathBuf> {
    crate::paths::config_file(".env")
}

/// Report which `.env` files were loaded. Both entries are optional;
/// an empty vector means the environment is whatever the launcher
/// provided, which is the normal case for a terminal launch.
#[derive(Debug, Default, Clone)]
pub struct Loaded {
    pub paths: Vec<PathBuf>,
}

/// Load `.env` files into the process environment.
///
/// Call once, early, before anything reads an API key. Never fails:
/// a missing or malformed `.env` is not a reason to refuse to start.
pub fn load() -> Loaded {
    let mut out = Loaded::default();

    // 1. Nearest `.env` walking up from cwd (development).
    if let Ok(p) = dotenvy::dotenv() {
        out.paths.push(p);
    }

    // 2. App config dir (packaged builds). Loaded second so a repo
    //    `.env` takes precedence while developing.
    if let Some(p) = config_env_path() {
        if p.is_file() && dotenvy::from_path(&p).is_ok() {
            out.paths.push(p);
        }
    }

    out
}

/// Load a specific file. Used by tests and by callers that know
/// exactly which file they want.
pub fn load_path(path: &Path) -> Result<(), String> {
    dotenvy::from_path(path).map_err(|e| format!("load {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn load_path_populates_the_environment() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join(".env");
        let mut fh = std::fs::File::create(&f).unwrap();
        // A name no other test touches, so this stays order-independent.
        writeln!(fh, "REZON_DOTENV_PROBE_A=from-dotenv").unwrap();
        drop(fh);

        assert!(std::env::var("REZON_DOTENV_PROBE_A").is_err());
        load_path(&f).unwrap();
        assert_eq!(
            std::env::var("REZON_DOTENV_PROBE_A").unwrap(),
            "from-dotenv"
        );
    }

    #[test]
    fn an_already_exported_variable_wins_over_the_file() {
        // The debugging case: someone exports a key to override a stale
        // `.env`. Silently preferring the file would be maddening.
        let dir = TempDir::new().unwrap();
        let f = dir.path().join(".env");
        let mut fh = std::fs::File::create(&f).unwrap();
        writeln!(fh, "REZON_DOTENV_PROBE_B=from-dotenv").unwrap();
        drop(fh);

        std::env::set_var("REZON_DOTENV_PROBE_B", "from-shell");
        load_path(&f).unwrap();
        assert_eq!(std::env::var("REZON_DOTENV_PROBE_B").unwrap(), "from-shell");
        std::env::remove_var("REZON_DOTENV_PROBE_B");
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_panic() {
        let dir = TempDir::new().unwrap();
        assert!(load_path(&dir.path().join("nope.env")).is_err());
    }

    #[test]
    fn load_never_fails_even_with_nothing_to_load() {
        // `load` is called unconditionally at startup; it must be inert
        // when there is no `.env` anywhere.
        let _ = load();
    }

    #[test]
    fn config_env_path_ends_in_dotenv_under_an_app_dir() {
        if let Some(p) = config_env_path() {
            assert_eq!(p.file_name().unwrap(), ".env");
            assert!(p.to_string_lossy().contains("rezon-tui"), "got {p:?}");
        }
    }
}
