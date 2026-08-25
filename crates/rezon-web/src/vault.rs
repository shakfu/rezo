// Thin Tauri command wrappers over `rezon_core::vault`. The actual
// path-traversal-safe filesystem ops live in core; this module only
// adds the `#[tauri::command]` attribute and forwards arguments.
//
// `vault_write` additionally journals the change (pre-image +
// post-image snapshots under `<vault>/.rezon-history/`) and opts the
// vault into per-edit git versioning. The journal is best-effort:
// failures don't unwind the write but are surfaced via the returned
// optional warning so the UI can display them.

use std::path::Path;

use rezon_core::journal;
use rezon_core::vault;
use tauri::{AppHandle, Emitter};

pub use rezon_core::vault::{ResolvedLink, VaultEntry};

#[tauri::command]
pub fn vault_list_tree(vault: String) -> Result<Vec<VaultEntry>, String> {
    vault::vault_list_tree(vault)
}

#[tauri::command]
pub fn vault_read(vault: String, path: String) -> Result<String, String> {
    vault::vault_read(vault, path)
}

#[tauri::command]
pub fn vault_write(
    app: AppHandle,
    vault: String,
    path: String,
    content: String,
) -> Result<(), String> {
    // Capture the pre-image (if any) so the journal can offer
    // proper undo. Failure to read an existing file aborts the
    // write — better to error than to silently lose recoverable
    // state.
    let abs = Path::new(&path);
    let before: Option<Vec<u8>> = if abs.exists() {
        Some(std::fs::read(abs).map_err(|e| format!("read pre-image {path}: {e}"))?)
    } else {
        None
    };
    let after_bytes = content.clone().into_bytes();
    vault::vault_write(vault.clone(), path.clone(), content)?;
    // The frontend gets back the same `()` it always did, but
    // journal warnings (e.g. a pre-commit hook rejected our
    // auto-commit) now surface via a `vault-warning` Tauri event
    // so the UI can show a toast. The write itself already
    // succeeded — this is a notice, not an error.
    let rel = relativize(&vault, &path);
    match journal::record_write(
        &vault,
        "manual_edit",
        &rel,
        before.as_deref(),
        Some(&after_bytes),
    ) {
        Ok(out) => {
            if let Some(w) = out.git_warning {
                let _ = app.emit("vault-warning", format!("git: {w}"));
            }
        }
        Err(e) => {
            let _ = app.emit("vault-warning", format!("journal: {e}"));
        }
    }
    Ok(())
}

/// Undo the most-recent journaled change to the vault. Returns
/// `{ path, targetId }` describing what was reverted, or an error
/// when there's nothing to undo. Git/journal warnings (e.g. a
/// pre-commit hook rejected the auto-commit recording the undo)
/// emit on the `vault-warning` event so the UI can toast them
/// without changing this command's return shape.
#[tauri::command]
pub fn vault_undo(app: AppHandle, vault: String) -> Result<UndoReport, String> {
    let out = journal::undo_last_op(&vault)?.ok_or_else(|| "nothing to undo".to_string())?;
    if let Some(w) = out.journal.git_warning {
        let _ = app.emit("vault-warning", format!("git: {w}"));
    }
    Ok(UndoReport {
        path: out.path,
        target_id: out.target_id,
    })
}

/// Reapply the most-recent journaled undo. Mirrors `vault_undo` —
/// errors when the redo stack is empty (no recent undo, or a
/// fresh write has invalidated it). Returns the path affected so
/// the UI can refresh open editors / the file tree.
#[tauri::command]
pub fn vault_redo(app: AppHandle, vault: String) -> Result<RedoReport, String> {
    let out = journal::redo_last_op(&vault)?.ok_or_else(|| "nothing to redo".to_string())?;
    if let Some(w) = out.journal.git_warning {
        let _ = app.emit("vault-warning", format!("git: {w}"));
    }
    Ok(RedoReport {
        path: out.path,
        target_undo_id: out.target_undo_id,
        was_creation: out.was_creation,
    })
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RedoReport {
    pub path: String,
    pub target_undo_id: String,
    pub was_creation: bool,
}

/// Recent journal entries, newest first. Caps at `limit` rows so a
/// long history doesn't ship megabytes to the frontend on every
/// open. `Op::Write` and `Op::Undo` are both surfaced — the
/// frontend filters / colors them for display.
#[tauri::command]
pub fn vault_journal_recent(
    vault: String,
    limit: Option<usize>,
) -> Result<Vec<JournalEntryDto>, String> {
    let limit = limit.unwrap_or(100).clamp(1, 1000);
    let entries = journal::recent_entries(&vault, limit)?;
    Ok(entries.into_iter().map(JournalEntryDto::from).collect())
}

/// Frontend-friendly shape of `journal::JournalEntry`. The core type
/// uses `#[serde(tag = "op")]` (snake_case) which serialises as
/// `{ id, ts, tool, path, op: "write", before_sha, after_sha }`
/// merged via `#[serde(flatten)]`. We flatten to a stable
/// camelCase shape and split the op kind into a discriminator
/// string so the frontend renders without pattern-matching.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalEntryDto {
    pub id: String,
    pub ts: u64,
    pub tool: String,
    pub path: String,
    /// One of `"write"` or `"undo"`.
    pub kind: String,
    pub before_sha: Option<String>,
    pub after_sha: Option<String>,
    /// Present only when `kind == "undo"`.
    pub target_id: Option<String>,
}

impl From<journal::JournalEntry> for JournalEntryDto {
    fn from(e: journal::JournalEntry) -> Self {
        match e.op {
            journal::Op::Write {
                before_sha,
                after_sha,
            } => Self {
                id: e.id,
                ts: e.ts,
                tool: e.tool,
                path: e.path,
                kind: "write".into(),
                before_sha,
                after_sha,
                target_id: None,
            },
            journal::Op::Undo {
                target_id,
                before_sha,
                after_sha,
            } => Self {
                id: e.id,
                ts: e.ts,
                tool: e.tool,
                path: e.path,
                kind: "undo".into(),
                before_sha,
                after_sha,
                target_id: Some(target_id),
            },
        }
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoReport {
    pub path: String,
    pub target_id: String,
}

#[tauri::command]
pub fn vault_create(app: AppHandle, vault: String, path: String) -> Result<(), String> {
    vault::vault_create(vault.clone(), path.clone())?;
    let rel = relativize(&vault, &path);
    // `vault_create` writes an empty file; record the creation so
    // `/undo` can delete it. before=None signals the file didn't
    // exist; after=Some(b"") preserves blob-deduped empty content.
    match journal::record_write(&vault, "vault_create", &rel, None, Some(b"")) {
        Ok(out) => {
            if let Some(w) = out.git_warning {
                let _ = app.emit("vault-warning", format!("git: {w}"));
            }
        }
        Err(e) => {
            let _ = app.emit("vault-warning", format!("journal: {e}"));
        }
    }
    Ok(())
}

#[tauri::command]
pub fn vault_mkdir(vault: String, path: String) -> Result<(), String> {
    // Directory ops are not journaled — the journal is file-oriented
    // (blobs keyed by content sha) and an empty directory has no
    // content. The directory will surface in the journal as soon as
    // a file is created inside it.
    vault::vault_mkdir(vault, path)
}

#[tauri::command]
pub fn vault_delete(app: AppHandle, vault: String, path: String) -> Result<(), String> {
    let abs = Path::new(&path);
    let was_dir = abs.is_dir();
    // For files, snapshot the pre-image so undo can restore. For
    // directories, walk and snapshot every contained file before the
    // recursive delete fires. Both paths land as `Op::Write` entries
    // with `after=None`, so `/undo` walks them back one at a time
    // (deepest-last so re-creating directories happens before files
    // land inside them).
    let snapshots: Vec<(String, Vec<u8>)> = if was_dir {
        snapshot_dir(&vault, abs)?
    } else if abs.exists() {
        let bytes = std::fs::read(abs).map_err(|e| format!("read pre-image {path}: {e}"))?;
        vec![(relativize(&vault, &path), bytes)]
    } else {
        return Err(format!("not found: {path}"));
    };
    vault::vault_delete(vault.clone(), path.clone())?;
    for (rel, before) in snapshots {
        match journal::record_write(&vault, "vault_delete", &rel, Some(&before), None) {
            Ok(out) => {
                if let Some(w) = out.git_warning {
                    let _ = app.emit("vault-warning", format!("git: {w}"));
                }
            }
            Err(e) => {
                let _ = app.emit("vault-warning", format!("journal: {e}"));
            }
        }
    }
    Ok(())
}

#[tauri::command]
pub fn vault_rename(app: AppHandle, vault: String, from: String, to: String) -> Result<(), String> {
    let from_abs = Path::new(&from);
    if !from_abs.exists() {
        return Err(format!("not found: {from}"));
    }
    // Capture pre-image of the source so the rename surfaces as
    // delete-from + create-at in the journal. Two `/undo`s reverse
    // the rename (first re-deletes the new file, second restores
    // the old). Directories: skip journaling (see note in
    // vault_mkdir); the move still goes through.
    let bytes_opt: Option<Vec<u8>> = if from_abs.is_file() {
        Some(std::fs::read(from_abs).map_err(|e| format!("read pre-image {from}: {e}"))?)
    } else {
        None
    };
    vault::vault_rename(vault.clone(), from.clone(), to.clone())?;
    if let Some(bytes) = bytes_opt {
        let rel_from = relativize(&vault, &from);
        let rel_to = relativize(&vault, &to);
        // Order: delete-from first, then create-at-to. `last_undoable`
        // walks back from the tail, so the first undo reverses the
        // "create at to" half (i.e. deletes the new file), and the
        // second undo restores the source.
        let _ = journal::record_write(&vault, "vault_rename", &rel_from, Some(&bytes), None);
        match journal::record_write(&vault, "vault_rename", &rel_to, None, Some(&bytes)) {
            Ok(out) => {
                if let Some(w) = out.git_warning {
                    let _ = app.emit("vault-warning", format!("git: {w}"));
                }
            }
            Err(e) => {
                let _ = app.emit("vault-warning", format!("journal: {e}"));
            }
        }
    }
    Ok(())
}

/// Vault-relative path: strip the vault root and any leading `/`.
/// Falls back to the absolute path when the strip fails (e.g. the
/// caller passed a path outside the vault — which the core layer's
/// `within` check should already reject before we get here, but
/// belt-and-suspenders).
fn relativize(vault: &str, abs: &str) -> String {
    abs.strip_prefix(vault)
        .map(|s| s.trim_start_matches('/').to_string())
        .unwrap_or_else(|| abs.to_string())
}

/// Walk `dir` recursively and return `(relative_path, content_bytes)`
/// for every regular file found. Used by `vault_delete` to snapshot
/// a directory tree before `remove_dir_all` wipes it. Hidden files
/// (`.gitignore`, `.rezon-history/...`) are included so a directory
/// undo restores the exact prior state.
fn snapshot_dir(vault: &str, dir: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut out = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let reader = std::fs::read_dir(&d).map_err(|e| format!("read_dir {:?}: {e}", d))?;
        for entry in reader.flatten() {
            let p = entry.path();
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() {
                let bytes = std::fs::read(&p).map_err(|e| format!("read {:?}: {e}", p))?;
                out.push((relativize(vault, &p.to_string_lossy()), bytes));
            }
        }
    }
    Ok(out)
}

#[tauri::command]
pub fn vault_resolve_wikilink(
    vault: String,
    target: String,
    create_if_missing: bool,
) -> Result<ResolvedLink, String> {
    vault::vault_resolve_wikilink(vault, target, create_if_missing)
}
