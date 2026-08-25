// Vault commands: filesystem operations scoped to a user-chosen
// vault root. The frontend passes absolute paths; every one is
// validated as contained inside the supplied vault root before any
// filesystem call. Containment is decided on resolved paths, so both
// `..` traversal and symlinks pointing out of the vault are rejected
// — see `within`.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum VaultEntry {
    File {
        name: String,
        path: String,
    },
    Dir {
        name: String,
        path: String,
        children: Vec<VaultEntry>,
    },
}

/// Resolve `p` the way the kernel would, walking it left to right and
/// following any symlink as it is encountered, but tolerating
/// components that do not exist yet.
///
/// Plain `canonicalize` fails outright on a path whose tail is missing,
/// and several callers here legitimately name a file they are about to
/// create (`vault_write`, `vault_create`, `vault_mkdir`, the
/// destination of `vault_rename`).
///
/// Walking forwards rather than normalizing the string first is what
/// makes `..` correct in the presence of links. For
/// `vault/link/../x` where `link` -> `/elsewhere`, text-first
/// normalization cancels `link` against `..` and concludes `vault/x`
/// — inside the vault. The kernel instead resolves `link` to
/// `/elsewhere` and applies `..` to *that*, landing on `/x`. Resolving
/// as we go gives the same answer the filesystem will.
fn resolve_path(p: &Path) -> Result<PathBuf, String> {
    let mut real = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::Prefix(prefix) => real.push(prefix.as_os_str()),
            Component::RootDir => real.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                real.pop();
            }
            Component::Normal(name) => {
                real.push(name);
                // Only an existing symlink needs resolving; a missing
                // component cannot be one, and a real directory or file
                // is already where it says it is.
                match fs::symlink_metadata(&real) {
                    Ok(md) if md.file_type().is_symlink() => {
                        real = real
                            .canonicalize()
                            .map_err(|e| format!("cannot resolve symlink {:?}: {e}", real))?;
                    }
                    _ => {}
                }
            }
        }
    }
    if real.as_os_str().is_empty() {
        return Err(format!("cannot resolve path {:?}", p));
    }
    Ok(real)
}

/// Collapse `.` and `..` textually, without touching the filesystem.
/// Used only to decide whether an error message should bother showing
/// the resolved path — never for a containment decision.
fn normalize_for_display(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Reject any path that does not land inside `vault`.
///
/// Containment is decided on *canonicalized* paths. A purely lexical
/// check (which is what this used to do) says nothing about symlinks:
/// a link inside the vault pointing at `~/.ssh/authorized_keys` passes
/// a `starts_with` test on the literal path and is then happily
/// followed by `fs::write`. Since `write_note` and friends are
/// reachable by prompt injection through note content, "the string
/// looks contained" is not a strong enough answer.
///
/// Canonicalizing the root as well is not optional: on macOS a home
/// directory is commonly reached through a symlink
/// (`/Users/me` -> `/System/Volumes/Data/Users/me`), so comparing a
/// resolved target against an unresolved root would reject every path
/// in a perfectly ordinary vault.
fn within(vault: &Path, target: &Path) -> Result<(), String> {
    let v = vault
        .canonicalize()
        .map_err(|e| format!("vault root {:?} is not resolvable: {e}", vault))?;
    let t = resolve_path(target)?;
    if !t.starts_with(&v) {
        // Report the *resolved* path when it differs from what was
        // asked for. The interesting case is a symlink, where the
        // literal path sits happily inside the vault and only the
        // resolved one shows the escape — an error naming just the
        // original reads as nonsense ("that path IS in the vault").
        if t != normalize_for_display(target) {
            return Err(format!(
                "path {:?} resolves to {:?}, which is outside vault {:?}",
                target, t, vault
            ));
        }
        return Err(format!("path {:?} is outside vault {:?}", target, vault));
    }
    Ok(())
}

fn read_tree(dir: &Path) -> Result<Vec<VaultEntry>, String> {
    let mut entries: Vec<VaultEntry> = Vec::new();
    let read = fs::read_dir(dir).map_err(|e| format!("read_dir {:?}: {e}", dir))?;
    for ent in read.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let path = ent.path();
        let kind = ent.file_type().map_err(|e| e.to_string())?;
        let path_str = path.to_string_lossy().to_string();
        if kind.is_dir() {
            let children = read_tree(&path)?;
            entries.push(VaultEntry::Dir {
                name,
                path: path_str,
                children,
            });
        } else if kind.is_file() {
            // Only surface markdown files in the tree. Other files are
            // ignored for now to keep the UI focused.
            let lower = name.to_lowercase();
            if lower.ends_with(".md") || lower.ends_with(".markdown") {
                entries.push(VaultEntry::File {
                    name,
                    path: path_str,
                });
            }
        }
    }
    entries.sort_by(|a, b| {
        let (ka, na) = match a {
            VaultEntry::Dir { name, .. } => (0u8, name.to_lowercase()),
            VaultEntry::File { name, .. } => (1u8, name.to_lowercase()),
        };
        let (kb, nb) = match b {
            VaultEntry::Dir { name, .. } => (0u8, name.to_lowercase()),
            VaultEntry::File { name, .. } => (1u8, name.to_lowercase()),
        };
        ka.cmp(&kb).then(na.cmp(&nb))
    });
    Ok(entries)
}

pub fn vault_list_tree(vault: String) -> Result<Vec<VaultEntry>, String> {
    let root = PathBuf::from(&vault);
    if !root.is_dir() {
        return Err(format!("vault root is not a directory: {vault}"));
    }
    read_tree(&root)
}

pub fn vault_read(vault: String, path: String) -> Result<String, String> {
    let v = PathBuf::from(&vault);
    let p = PathBuf::from(&path);
    within(&v, &p)?;
    fs::read_to_string(&p).map_err(|e| format!("read {path}: {e}"))
}

pub fn vault_write(vault: String, path: String, content: String) -> Result<(), String> {
    let v = PathBuf::from(&vault);
    let p = PathBuf::from(&path);
    within(&v, &p)?;
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {:?}: {e}", parent))?;
    }
    fs::write(&p, content).map_err(|e| format!("write {path}: {e}"))
}

pub fn vault_create(vault: String, path: String) -> Result<(), String> {
    let v = PathBuf::from(&vault);
    let p = PathBuf::from(&path);
    within(&v, &p)?;
    if p.exists() {
        return Err(format!("already exists: {path}"));
    }
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {:?}: {e}", parent))?;
    }
    fs::write(&p, "").map_err(|e| format!("create {path}: {e}"))
}

pub fn vault_mkdir(vault: String, path: String) -> Result<(), String> {
    let v = PathBuf::from(&vault);
    let p = PathBuf::from(&path);
    within(&v, &p)?;
    if p.exists() {
        return Err(format!("already exists: {path}"));
    }
    fs::create_dir_all(&p).map_err(|e| format!("mkdir {path}: {e}"))
}

pub fn vault_delete(vault: String, path: String) -> Result<(), String> {
    let v = PathBuf::from(&vault);
    let p = PathBuf::from(&path);
    within(&v, &p)?;
    if p.is_dir() {
        fs::remove_dir_all(&p).map_err(|e| format!("rmdir {path}: {e}"))
    } else {
        fs::remove_file(&p).map_err(|e| format!("rm {path}: {e}"))
    }
}

pub fn vault_rename(vault: String, from: String, to: String) -> Result<(), String> {
    let v = PathBuf::from(&vault);
    let a = PathBuf::from(&from);
    let b = PathBuf::from(&to);
    within(&v, &a)?;
    within(&v, &b)?;
    fs::rename(&a, &b).map_err(|e| format!("rename: {e}"))
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ResolvedLink {
    pub path: String,
    pub created: bool,
}

// Resolve a wikilink target ([[foo]] or [[folder/foo]]) to an absolute
// path inside the vault. If `create_if_missing` is true and no match
// exists, create the file in the vault root with a ".md" extension.
//
// Resolution order:
//   1. exact relative path under vault root (with or without .md)
//   2. first file in the tree whose stem matches case-insensitively
pub fn vault_resolve_wikilink(
    vault: String,
    target: String,
    create_if_missing: bool,
) -> Result<ResolvedLink, String> {
    let root = PathBuf::from(&vault);
    if !root.is_dir() {
        return Err("vault root is not a directory".into());
    }
    let mut t = target.trim().to_string();
    if t.is_empty() {
        return Err("empty target".into());
    }
    // Strip a leading "/" so callers don't accidentally escape root.
    while t.starts_with('/') {
        t.remove(0);
    }

    // 1. exact relative path
    let with_ext = if t.to_lowercase().ends_with(".md") {
        t.clone()
    } else {
        format!("{t}.md")
    };
    let direct = root.join(&with_ext);
    if direct.is_file() {
        within(&root, &direct)?;
        return Ok(ResolvedLink {
            path: direct.to_string_lossy().to_string(),
            created: false,
        });
    }

    // 2. recursive stem match (case-insensitive)
    let stem = Path::new(&t)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&t)
        .to_lowercase();
    if let Some(found) = find_by_stem(&root, &stem) {
        return Ok(ResolvedLink {
            path: found.to_string_lossy().to_string(),
            created: false,
        });
    }

    if !create_if_missing {
        return Err(format!("not found: {t}"));
    }

    let target_path = root.join(&with_ext);
    within(&root, &target_path)?;
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    fs::write(&target_path, "").map_err(|e| format!("create: {e}"))?;
    Ok(ResolvedLink {
        path: target_path.to_string_lossy().to_string(),
        created: true,
    })
}

fn find_by_stem(dir: &Path, stem_lower: &str) -> Option<PathBuf> {
    let read = fs::read_dir(dir).ok()?;
    for ent in read.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let path = ent.path();
        let kind = ent.file_type().ok()?;
        if kind.is_dir() {
            if let Some(hit) = find_by_stem(&path, stem_lower) {
                return Some(hit);
            }
        } else if kind.is_file() {
            let lower = name.to_lowercase();
            if !(lower.ends_with(".md") || lower.ends_with(".markdown")) {
                continue;
            }
            if let Some(s) = Path::new(&name).file_stem().and_then(|s| s.to_str()) {
                if s.to_lowercase() == stem_lower {
                    return Some(path);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn touch(dir: &Path, rel: &str, body: &str) -> PathBuf {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn read_write_roundtrip() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path().to_string_lossy().to_string();
        let path = touch(dir.path(), "note.md", "hello")
            .to_string_lossy()
            .to_string();
        assert_eq!(vault_read(vault.clone(), path.clone()).unwrap(), "hello");
        vault_write(vault.clone(), path.clone(), "world".to_string()).unwrap();
        assert_eq!(vault_read(vault, path).unwrap(), "world");
    }

    #[test]
    fn write_outside_vault_rejected() {
        let inside = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let escape = outside.path().join("evil.md");
        let err = vault_write(
            inside.path().to_string_lossy().to_string(),
            escape.to_string_lossy().to_string(),
            "pwned".to_string(),
        )
        .unwrap_err();
        assert!(err.contains("outside vault"), "got: {err}");
        assert!(!escape.exists(), "escape path should not have been written");
    }

    #[test]
    fn parentdir_traversal_normalized() {
        // `vault/sub/../../escape` normalises to `escape`, which is
        // outside the vault. The `within` check should catch it even
        // though the literal path contains the vault prefix.
        let inside = TempDir::new().unwrap();
        let target = inside
            .path()
            .join("sub")
            .join("..")
            .join("..")
            .join("escape.md");
        let err = vault_write(
            inside.path().to_string_lossy().to_string(),
            target.to_string_lossy().to_string(),
            "x".to_string(),
        )
        .unwrap_err();
        assert!(err.contains("outside vault"), "got: {err}");
    }

    // ---- Symlink containment ------------------------------------
    //
    // The lexical check these replaced passed every one of these: the
    // literal path really does start with the vault root. Only the
    // resolved path reveals where the write would land.

    #[test]
    #[cfg(unix)]
    fn write_through_a_symlink_pointing_outside_is_rejected() {
        let vault = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let secret = outside.path().join("authorized_keys");
        std::fs::write(&secret, "original").unwrap();

        // A link sitting inside the vault, aimed out of it.
        let link = vault.path().join("innocent.md");
        std::os::unix::fs::symlink(&secret, &link).unwrap();

        let err = vault_write(
            vault.path().to_string_lossy().to_string(),
            link.to_string_lossy().to_string(),
            "pwned".to_string(),
        )
        .unwrap_err();
        assert!(err.contains("outside vault"), "got: {err}");
        assert_eq!(
            std::fs::read_to_string(&secret).unwrap(),
            "original",
            "the file outside the vault was modified"
        );
    }

    #[test]
    #[cfg(unix)]
    fn read_through_a_symlink_pointing_outside_is_rejected() {
        let vault = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let secret = outside.path().join("secret.md");
        std::fs::write(&secret, "classified").unwrap();

        let link = vault.path().join("note.md");
        std::os::unix::fs::symlink(&secret, &link).unwrap();

        let err = vault_read(
            vault.path().to_string_lossy().to_string(),
            link.to_string_lossy().to_string(),
        )
        .unwrap_err();
        assert!(err.contains("outside vault"), "got: {err}");
    }

    #[test]
    #[cfg(unix)]
    fn write_into_a_symlinked_directory_pointing_outside_is_rejected() {
        // The link is a path *component*, not the leaf, and the leaf
        // does not exist yet — the create-a-new-file case.
        let vault = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let link = vault.path().join("subdir");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();

        let target = link.join("new.md");
        let err = vault_write(
            vault.path().to_string_lossy().to_string(),
            target.to_string_lossy().to_string(),
            "x".to_string(),
        )
        .unwrap_err();
        assert!(err.contains("outside vault"), "got: {err}");
        assert!(!outside.path().join("new.md").exists());
    }

    #[test]
    #[cfg(unix)]
    fn parentdir_after_a_symlink_resolves_against_the_link_target() {
        // `vault/link/../escape.md` where link -> /outside/sub.
        // Cancelling `link` against `..` textually would land on
        // `vault/escape.md` and be allowed; the kernel lands on
        // `/outside/escape.md`, which must not be.
        let vault = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let sub = outside.path().join("sub");
        std::fs::create_dir(&sub).unwrap();

        let link = vault.path().join("link");
        std::os::unix::fs::symlink(&sub, &link).unwrap();

        let target = link.join("..").join("escape.md");
        let err = vault_write(
            vault.path().to_string_lossy().to_string(),
            target.to_string_lossy().to_string(),
            "x".to_string(),
        )
        .unwrap_err();
        assert!(err.contains("outside vault"), "got: {err}");
        assert!(!outside.path().join("escape.md").exists());
    }

    #[test]
    #[cfg(unix)]
    fn a_symlink_staying_inside_the_vault_is_allowed() {
        // Containment, not a blanket symlink ban: a link between two
        // places in the same vault is a legitimate way to organize
        // notes and keeps working.
        let vault = TempDir::new().unwrap();
        let real = vault.path().join("real.md");
        std::fs::write(&real, "hi").unwrap();
        let link = vault.path().join("alias.md");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let got = vault_read(
            vault.path().to_string_lossy().to_string(),
            link.to_string_lossy().to_string(),
        )
        .unwrap();
        assert_eq!(got, "hi");
    }

    #[test]
    fn rename_validates_both_endpoints() {
        let vault = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let src = vault.path().join("a.md");
        std::fs::write(&src, "x").unwrap();

        // Destination outside the vault must be refused even though
        // the source is fine.
        let err = vault_rename(
            vault.path().to_string_lossy().to_string(),
            src.to_string_lossy().to_string(),
            outside.path().join("b.md").to_string_lossy().to_string(),
        )
        .unwrap_err();
        assert!(err.contains("outside vault"), "got: {err}");
        assert!(src.exists(), "source must be untouched after a refusal");
    }

    #[test]
    fn list_tree_filters_and_sorts() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        touch(root, "alpha.md", "");
        touch(root, "Beta.md", "");
        touch(root, ".hidden.md", ""); // hidden — excluded
        touch(root, "notes.txt", ""); // non-markdown — excluded
        touch(root, "sub/zeta.md", "");
        touch(root, "sub/a-readme.markdown", "");
        let entries = vault_list_tree(root.to_string_lossy().to_string()).unwrap();
        // Top level: one dir (sub) before files; files sorted
        // case-insensitively.
        let names: Vec<&str> = entries
            .iter()
            .map(|e| match e {
                VaultEntry::Dir { name, .. } => name.as_str(),
                VaultEntry::File { name, .. } => name.as_str(),
            })
            .collect();
        assert_eq!(names, vec!["sub", "alpha.md", "Beta.md"]);
        // Hidden + non-markdown are absent.
        assert!(!names.iter().any(|n| n.starts_with('.')));
        assert!(!names.iter().any(|n| n.ends_with(".txt")));
    }

    #[test]
    fn resolve_wikilink_exact_then_stem() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_string_lossy().to_string();
        touch(dir.path(), "sub/Project Notes.md", "");

        // Exact relative path with extension.
        let r = vault_resolve_wikilink(root.clone(), "sub/Project Notes.md".into(), false).unwrap();
        assert!(r.path.ends_with("Project Notes.md"));
        assert!(!r.created);

        // Stem match, case-insensitive, anywhere in the tree.
        let r = vault_resolve_wikilink(root.clone(), "project notes".into(), false).unwrap();
        assert!(r.path.ends_with("Project Notes.md"));
        assert!(!r.created);

        // Missing target without create_if_missing -> error.
        let err = vault_resolve_wikilink(root.clone(), "missing".into(), false).unwrap_err();
        assert!(err.contains("not found"), "got: {err}");

        // Missing target with create_if_missing -> file is created at root.
        let r = vault_resolve_wikilink(root, "fresh".into(), true).unwrap();
        assert!(r.created);
        assert!(r.path.ends_with("fresh.md"));
        assert!(Path::new(&r.path).is_file());
    }

    #[test]
    fn delete_then_rename() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path().to_string_lossy().to_string();
        let a = touch(dir.path(), "a.md", "x").to_string_lossy().to_string();
        let b = dir.path().join("b.md").to_string_lossy().to_string();
        vault_rename(vault.clone(), a.clone(), b.clone()).unwrap();
        assert!(!Path::new(&a).exists());
        assert!(Path::new(&b).exists());
        vault_delete(vault, b.clone()).unwrap();
        assert!(!Path::new(&b).exists());
    }
}
