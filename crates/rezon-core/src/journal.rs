// Per-vault edit journal with optional git versioning.
//
// Every mutation that flows through `vault_write_journaled` /
// `vault_delete_journaled` / `vault_rename_journaled` lands two
// places:
//
//   1. `<vault>/.rezon-history/log.jsonl` — append-only ledger of
//      entries: { id, ts, tool, op, path, before_sha, after_sha }.
//      Content blobs are deduped under
//      `<vault>/.rezon-history/blobs/<sha>` so undo/redo can restore
//      arbitrary prior states.
//   2. The vault's git repo (auto-`git init`'d if absent), one
//      commit per mutation, committing the changed file alone. The
//      `.rezon-history/` directory is excluded via `.gitignore` so
//      the journal doesn't pollute the git log — git is the
//      versioning surface, the journal is the rezon-internal
//      audit/undo log.
//
// Failures in either path are *non-fatal* for the underlying write
// — they're surfaced via the returned `JournalOutcome` so callers
// can warn. Rationale: a misconfigured git, a read-only `.git/objects`,
// or a `.rezon-history/` permission glitch shouldn't be why the user
// can't save their notes.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const HISTORY_DIR: &str = ".rezon-history";
const LOG_FILE: &str = "log.jsonl";
const BLOBS_DIR: &str = "blobs";
const GITIGNORE: &str = ".gitignore";
const GITIGNORE_RULE: &str = ".rezon-history/";
/// Marker file. When present at the vault root, journal recording
/// proceeds but git auto-init / auto-commit is skipped — useful for
/// vaults nested inside an outer repo where rezon shouldn't be
/// creating a second one.
const SKIP_GIT_SENTINEL: &str = ".rezon-skip-git";
/// Maximum journal entries kept in `log.jsonl`. Older entries are
/// truncated FIFO at the end of each `record_write`, and any blobs
/// they were the only reference to get pruned. Picked to cover a
/// few sessions of heavy editing without growing unbounded; tune via
/// `set_gc_policy` if you need a different ceiling.
const DEFAULT_MAX_ENTRIES: usize = 500;

/// Logical operation kind. `Write` covers all create/overwrite/
/// append/edit cases (the journal doesn't distinguish — before/after
/// snapshots fully describe the change). `Undo` references the
/// `target_id` of the entry it reverses, so the set of "undone" ids
/// is recoverable by scanning the log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Write {
        /// Sha256 of the file's content prior to the write, or `None`
        /// when the path didn't exist.
        before_sha: Option<String>,
        /// Sha256 of the file's content after the write, or `None`
        /// when the operation removed the file.
        after_sha: Option<String>,
    },
    Undo {
        /// The id of the entry being reverted.
        target_id: String,
        before_sha: Option<String>,
        after_sha: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub id: String,
    /// Milliseconds since unix epoch.
    pub ts: u64,
    /// Tool / source that triggered the change, e.g.
    /// `"write_note"`, `"append_note"`, `"edit_note"`,
    /// `"manual_edit"`, `"undo"`. Free-form; the journal doesn't
    /// validate.
    pub tool: String,
    /// Vault-relative path of the affected file.
    pub path: String,
    #[serde(flatten)]
    pub op: Op,
}

/// Result of a successful journal write. `git_committed` is false
/// when the vault has no git repo, or when git failed (with a
/// reason in `git_warning`). The caller can surface the warning
/// without blocking the underlying file write.
#[derive(Debug, Clone)]
pub struct JournalOutcome {
    pub entry: JournalEntry,
    pub git_committed: bool,
    pub git_warning: Option<String>,
}

/// Record a write to the journal. `before` is the pre-image (None if
/// the file didn't exist or wasn't read), `after` is the post-image
/// (None when the op deleted the file). Both are content snapshots,
/// not paths.
pub fn record_write(
    vault: &str,
    tool: &str,
    rel_path: &str,
    before: Option<&[u8]>,
    after: Option<&[u8]>,
) -> Result<JournalOutcome, String> {
    let vault_root = PathBuf::from(vault);
    ensure_history_dirs(&vault_root)?;
    ensure_gitignore(&vault_root)?;

    let before_sha = match before {
        Some(bytes) => Some(write_blob(&vault_root, bytes)?),
        None => None,
    };
    let after_sha = match after {
        Some(bytes) => Some(write_blob(&vault_root, bytes)?),
        None => None,
    };

    let entry = JournalEntry {
        id: next_id(),
        ts: now_ms(),
        tool: tool.to_string(),
        path: rel_path.to_string(),
        op: Op::Write {
            before_sha,
            after_sha,
        },
    };
    append_entry(&vault_root, &entry)?;
    // FIFO cap: keep the journal bounded so heavy editing sessions
    // don't fill the disk. Failure is non-fatal — a bloated journal
    // is a worse outcome than a missed eviction.
    let _ = gc(&vault_root, DEFAULT_MAX_ENTRIES);

    let (git_committed, git_warning) = git_commit(&vault_root, rel_path, &entry.tool);
    Ok(JournalOutcome {
        entry,
        git_committed,
        git_warning,
    })
}

/// Record an undo. `target_id` should be the entry whose effect was
/// reversed. `before`/`after` describe the on-disk state changed by
/// the undo itself (so a redo could reverse this entry).
pub fn record_undo(
    vault: &str,
    rel_path: &str,
    target_id: &str,
    before: Option<&[u8]>,
    after: Option<&[u8]>,
) -> Result<JournalOutcome, String> {
    let vault_root = PathBuf::from(vault);
    ensure_history_dirs(&vault_root)?;
    ensure_gitignore(&vault_root)?;

    let before_sha = match before {
        Some(bytes) => Some(write_blob(&vault_root, bytes)?),
        None => None,
    };
    let after_sha = match after {
        Some(bytes) => Some(write_blob(&vault_root, bytes)?),
        None => None,
    };
    let entry = JournalEntry {
        id: next_id(),
        ts: now_ms(),
        tool: "undo".to_string(),
        path: rel_path.to_string(),
        op: Op::Undo {
            target_id: target_id.to_string(),
            before_sha,
            after_sha,
        },
    };
    append_entry(&vault_root, &entry)?;
    let _ = gc(&vault_root, DEFAULT_MAX_ENTRIES);
    let (git_committed, git_warning) = git_commit(&vault_root, rel_path, "undo");
    Ok(JournalOutcome {
        entry,
        git_committed,
        git_warning,
    })
}

/// Returns the most recent reversible journal entry — the last
/// `Op::Write` that hasn't been undone by a subsequent `Op::Undo`.
/// `None` when there's nothing to undo.
pub fn last_undoable(vault: &str) -> Result<Option<JournalEntry>, String> {
    let entries = read_log(&PathBuf::from(vault))?;
    let mut undone: std::collections::HashSet<String> = Default::default();
    for e in &entries {
        if let Op::Undo { target_id, .. } = &e.op {
            undone.insert(target_id.clone());
        }
    }
    Ok(entries
        .into_iter()
        .rev()
        .find(|e| matches!(e.op, Op::Write { .. }) && !undone.contains(&e.id)))
}

/// Read the most-recent `limit` entries from the journal, newest
/// first. Used by UIs that show edit history. Cheaper than reading
/// the entire log in callers — capped by `limit` and returns owned
/// entries (no streaming hot-loop needed).
pub fn recent_entries(vault: &str, limit: usize) -> Result<Vec<JournalEntry>, String> {
    let mut entries = read_log(&PathBuf::from(vault))?;
    if entries.len() > limit {
        entries.drain(0..entries.len() - limit);
    }
    entries.reverse();
    Ok(entries)
}

/// Restore a blob to disk. Used by undo to reinstate a `before`
/// snapshot. Returns the bytes read for the caller's bookkeeping.
pub fn read_blob(vault: &str, sha: &str) -> Result<Vec<u8>, String> {
    let p = PathBuf::from(vault)
        .join(HISTORY_DIR)
        .join(BLOBS_DIR)
        .join(sha);
    fs::read(&p).map_err(|e| format!("read blob {sha}: {e}"))
}

/// Returns the entry that would be reapplied by `redo_last_op`, or
/// `None` when the redo stack is empty. Convention follows
/// text-editor undo/redo: any `Op::Write` after the most recent
/// `Op::Undo` invalidates the redo stack (a fresh edit forks the
/// timeline). Tail walk stops at the first `Op::Write`.
pub fn last_redoable(vault: &str) -> Result<Option<JournalEntry>, String> {
    let entries = read_log(&PathBuf::from(vault))?;
    // The last entry decides everything: if it's an Undo, redo
    // reapplies it; if it's a Write (including a `tool:"redo"`
    // synthesised one), the redo stack is invalidated; empty log →
    // nothing redoable.
    Ok(entries.into_iter().next_back().and_then(|e| match e.op {
        Op::Undo { .. } => Some(e),
        Op::Write { .. } => None,
    }))
}

/// Summary of a successful `redo_last_op` call. Distinct from
/// `UndoOutcome` because the semantics differ — redo reapplies an
/// undo's pre-undo state and surfaces the originating undo's id for
/// callers that want to display "redid <tool>".
#[derive(Debug, Clone)]
pub struct RedoOutcome {
    /// Id of the `Op::Undo` entry that was reversed.
    pub target_undo_id: String,
    /// Vault-relative path of the affected file.
    pub path: String,
    /// True when the redo *created* the file (the undo had deleted
    /// it). Callers that own a file tree should refresh on `true`.
    pub was_creation: bool,
    /// `JournalOutcome` for the recorded fresh `Op::Write` entry
    /// (tool: "redo"). Carries `git_committed` and `git_warning`.
    pub journal: JournalOutcome,
}

/// Reapply the most-recent `Op::Undo`. Restores its `before_sha`
/// content (i.e. the disk state immediately before that undo ran)
/// and records a fresh `Op::Write` tagged `tool: "redo"` so the
/// chain stays linear: a subsequent undo reverses this redo, and a
/// fresh write after the redo invalidates further redos (matching
/// editor convention — there is no multi-step redo stack across
/// intervening manual edits).
///
/// Returns `Ok(None)` when the redo stack is empty (no recent undo
/// to reapply).
pub fn redo_last_op(vault: &str) -> Result<Option<RedoOutcome>, String> {
    let undo = match last_redoable(vault)? {
        Some(u) => u,
        None => return Ok(None),
    };
    let (target_undo_id, target_path, restore_sha) = match undo.op {
        Op::Undo { before_sha, .. } => (undo.id, undo.path, before_sha),
        // `last_redoable` filters to Op::Undo only; defense-in-depth.
        _ => return Err("internal: last_redoable returned a non-Undo entry".into()),
    };

    let abs = PathBuf::from(vault).join(&target_path);
    let current = fs::read(&abs).ok();

    let was_creation = current.is_none();
    match &restore_sha {
        Some(sha) => {
            let bytes = read_blob(vault, sha)?;
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
            }
            fs::write(&abs, &bytes).map_err(|e| format!("restore {}: {e}", abs.display()))?;
        }
        None => {
            // Undoing reached a deletion — redoing it removes the
            // file again. Missing is fine.
            if abs.exists() {
                fs::remove_file(&abs).map_err(|e| format!("delete {}: {e}", abs.display()))?;
            }
        }
    }

    let new_after = fs::read(&abs).ok();
    let journal = record_write(
        vault,
        "redo",
        &target_path,
        current.as_deref(),
        new_after.as_deref(),
    )?;

    Ok(Some(RedoOutcome {
        target_undo_id,
        path: target_path,
        was_creation,
        journal,
    }))
}

/// Summary of a successful `undo_last_op` call.
#[derive(Debug, Clone)]
pub struct UndoOutcome {
    /// Id of the entry that was reverted.
    pub target_id: String,
    /// Tool name from the reverted entry (e.g. `"write_note"`,
    /// `"manual_edit"`, `"vault_delete"`). Useful for UI strings
    /// like "undid X on Y".
    pub tool: String,
    /// Vault-relative path of the affected file.
    pub path: String,
    /// True when the undo deleted the file (the target was a
    /// creation, so the restored state is "absent"). Callers that
    /// own a file tree / open-tabs list should refresh.
    pub was_deletion: bool,
    /// `JournalOutcome` for the recorded undo entry — carries
    /// `git_committed` and any `git_warning`. Read these to surface
    /// hook failures or git-skip cases to the user.
    pub journal: JournalOutcome,
}

/// Revert the most-recent journaled mutation. Restores the file from
/// the `before_sha` blob (or deletes it when the target was a
/// creation), then records a new `Op::Undo` entry so subsequent
/// undos walk the chain instead of re-undoing the same entry. The
/// caller is responsible for any index hygiene (`vault_index_touch`)
/// since `journal` doesn't depend on `search`.
///
/// Returns `Ok(None)` when there is nothing to undo.
pub fn undo_last_op(vault: &str) -> Result<Option<UndoOutcome>, String> {
    let target = match last_undoable(vault)? {
        Some(t) => t,
        None => return Ok(None),
    };
    let (target_id, tool, target_path, before_sha) = match target.op {
        Op::Write { before_sha, .. } => (target.id, target.tool, target.path, before_sha),
        // `last_undoable` filters Undo entries, so this branch is
        // unreachable in practice; surface the invariant clearly
        // rather than panic.
        _ => {
            return Err("internal: last_undoable returned a non-Write entry".into());
        }
    };

    let abs = PathBuf::from(vault).join(&target_path);
    // Snapshot the current on-disk content for the undo entry's
    // `before` field. Missing is fine (the file may already be
    // gone if a manual edit deleted it out-of-band).
    let current = fs::read(&abs).ok();

    let was_deletion = before_sha.is_none();
    match &before_sha {
        Some(sha) => {
            let bytes = read_blob(vault, sha)?;
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
            }
            fs::write(&abs, &bytes).map_err(|e| format!("restore {}: {e}", abs.display()))?;
        }
        None => {
            // Target was a creation; undo deletes the file. Missing
            // is fine — someone may have already removed it.
            if abs.exists() {
                fs::remove_file(&abs).map_err(|e| format!("delete {}: {e}", abs.display()))?;
            }
        }
    }

    // Re-read after the restore so the undo entry's `after_sha`
    // matches the on-disk state byte-for-byte. Deduplicates with
    // existing blobs.
    let new_after = fs::read(&abs).ok();

    let journal = record_undo(
        vault,
        &target_path,
        &target_id,
        current.as_deref(),
        new_after.as_deref(),
    )?;

    Ok(Some(UndoOutcome {
        target_id,
        tool,
        path: target_path,
        was_deletion,
        journal,
    }))
}

// ---- internals ---------------------------------------------------------

fn ensure_history_dirs(vault_root: &Path) -> Result<(), String> {
    let blobs = vault_root.join(HISTORY_DIR).join(BLOBS_DIR);
    fs::create_dir_all(&blobs).map_err(|e| format!("mkdir {}: {e}", blobs.display()))
}

fn ensure_gitignore(vault_root: &Path) -> Result<(), String> {
    let p = vault_root.join(GITIGNORE);
    let current = fs::read_to_string(&p).unwrap_or_default();
    if current.lines().any(|l| l.trim() == GITIGNORE_RULE) {
        return Ok(());
    }
    let mut next = current;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(GITIGNORE_RULE);
    next.push('\n');
    fs::write(&p, next).map_err(|e| format!("write .gitignore: {e}"))
}

fn write_blob(vault_root: &Path, bytes: &[u8]) -> Result<String, String> {
    let sha = sha256_hex(bytes);
    let p = vault_root.join(HISTORY_DIR).join(BLOBS_DIR).join(&sha);
    if p.exists() {
        return Ok(sha);
    }
    fs::write(&p, bytes).map_err(|e| format!("write blob: {e}"))?;
    Ok(sha)
}

fn append_entry(vault_root: &Path, entry: &JournalEntry) -> Result<(), String> {
    let p = vault_root.join(HISTORY_DIR).join(LOG_FILE);
    let line = serde_json::to_string(entry).map_err(|e| format!("serialize entry: {e}"))?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
        .map_err(|e| format!("open {}: {e}", p.display()))?;
    writeln!(f, "{line}").map_err(|e| format!("write log: {e}"))
}

fn read_log(vault_root: &Path) -> Result<Vec<JournalEntry>, String> {
    let p = vault_root.join(HISTORY_DIR).join(LOG_FILE);
    let raw = match fs::read_to_string(&p) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("read log: {e}")),
    };
    let mut out = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let e: JournalEntry =
            serde_json::from_str(line).map_err(|e| format!("parse log line {}: {e}", i + 1))?;
        out.push(e);
    }
    Ok(out)
}

/// Garbage-collect the journal: keep the most recent `max_entries`
/// in `log.jsonl` and delete any blob no longer referenced.
///
/// Cheap when nothing to evict: the log is read once, and if its
/// length is within budget we return without touching the filesystem.
/// Eviction rewrites `log.jsonl` atomically (`log.jsonl.tmp` then
/// rename) so a crash mid-eviction can't truncate history.
///
/// Returns the count of entries evicted (callers may ignore).
pub fn gc(vault_root: &Path, max_entries: usize) -> Result<usize, String> {
    let entries = read_log(vault_root)?;
    if entries.len() <= max_entries {
        return Ok(0);
    }
    let drop_count = entries.len() - max_entries;
    let kept: Vec<JournalEntry> = entries.into_iter().skip(drop_count).collect();

    // Atomic rewrite: write to a sibling tmp, fsync, rename over the
    // original. Failures fall through to the original log (which is
    // still consistent — we never deleted it).
    let log = vault_root.join(HISTORY_DIR).join(LOG_FILE);
    let tmp = vault_root.join(HISTORY_DIR).join(format!("{LOG_FILE}.tmp"));
    {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("create tmp log: {e}"))?;
        for e in &kept {
            let line = serde_json::to_string(e).map_err(|e| format!("serialize: {e}"))?;
            writeln!(f, "{line}").map_err(|e| format!("write tmp log: {e}"))?;
        }
        f.sync_all().map_err(|e| format!("fsync tmp log: {e}"))?;
    }
    fs::rename(&tmp, &log).map_err(|e| format!("rename tmp log: {e}"))?;

    // Prune blobs that no surviving entry references. Iteration over
    // `blobs/` is O(n) per GC, but GC only fires once we cross the
    // entry cap so the cost is amortized.
    let mut alive: std::collections::HashSet<String> = Default::default();
    for e in &kept {
        match &e.op {
            Op::Write {
                before_sha,
                after_sha,
            } => {
                if let Some(s) = before_sha {
                    alive.insert(s.clone());
                }
                if let Some(s) = after_sha {
                    alive.insert(s.clone());
                }
            }
            Op::Undo {
                before_sha,
                after_sha,
                ..
            } => {
                if let Some(s) = before_sha {
                    alive.insert(s.clone());
                }
                if let Some(s) = after_sha {
                    alive.insert(s.clone());
                }
            }
        }
    }
    let blobs_dir = vault_root.join(HISTORY_DIR).join(BLOBS_DIR);
    if let Ok(reader) = fs::read_dir(&blobs_dir) {
        for entry in reader.flatten() {
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            if !alive.contains(name_str) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    Ok(drop_count)
}

/// Try to commit the path to the vault's git repo. Auto-init's the
/// repo on first use. Returns (committed, optional_warning).
///
/// Failure modes (any one returns committed=false with a warning):
///   * git binary not on PATH
///   * `git init` fails
///   * `git add` fails (e.g. file deleted between write and commit)
///   * `git commit` fails (no changes — file is byte-identical to
///     the indexed version — or a hook rejected it)
///
/// The "no changes" case is normal (consecutive identical writes)
/// and rolls up to a `committed=false, warning=None` outcome via the
/// short-circuit before the commit attempt.
fn git_commit(vault_root: &Path, rel_path: &str, tool: &str) -> (bool, Option<String>) {
    if vault_root.join(SKIP_GIT_SENTINEL).exists() {
        // Sentinel present — caller has opted this vault out (e.g.
        // because it's a subdirectory of a larger repo and rezon
        // shouldn't create a nested one). Silent skip; no warning,
        // since the user has explicitly chosen this path.
        return (false, None);
    }
    if which_git().is_none() {
        return (false, Some("git not on PATH".into()));
    }
    if !vault_root.join(".git").exists() {
        if let Err(e) = run_git(vault_root, &["init", "-q"]) {
            return (false, Some(format!("git init: {e}")));
        }
    }
    // `git add` is permissive — `--` ends option parsing so paths
    // starting with `-` don't get mistaken for flags.
    if let Err(e) = run_git(vault_root, &["add", "--", rel_path, GITIGNORE]) {
        return (false, Some(format!("git add: {e}")));
    }
    let msg = format!("rezon: {tool} {rel_path}");
    // `--allow-empty=false` is git's default; if our add produced no
    // staged change (file unchanged or already committed), the
    // commit step prints "nothing to commit" and exits non-zero. We
    // treat that as a benign no-op.
    match run_git(vault_root, &["commit", "-q", "-m", &msg]) {
        Ok(_) => (true, None),
        Err(e) if e.contains("nothing to commit") || e.contains("nothing added") => (false, None),
        Err(e) => (false, Some(format!("git commit: {e}"))),
    }
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return Err(if stderr.is_empty() { stdout } else { stderr });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn which_git() -> Option<()> {
    Command::new("git")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| ())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn next_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("j-{}-{c}", now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn read_relative(vault: &Path, rel: &str) -> Vec<u8> {
        fs::read(vault.join(rel)).unwrap()
    }

    #[test]
    fn record_write_creates_history_dir_and_blobs() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path().to_string_lossy().to_string();
        // Pretend the caller already wrote "X.md".
        fs::write(dir.path().join("X.md"), b"hello").unwrap();
        let out = record_write(&vault, "write_note", "X.md", None, Some(b"hello")).unwrap();
        assert!(dir.path().join(".rezon-history/log.jsonl").exists());
        match &out.entry.op {
            Op::Write {
                before_sha,
                after_sha,
            } => {
                assert!(before_sha.is_none());
                let sha = after_sha.as_ref().unwrap();
                let blob = read_relative(dir.path(), &format!(".rezon-history/blobs/{sha}"));
                assert_eq!(blob, b"hello");
            }
            _ => panic!("expected Write"),
        }
    }

    #[test]
    fn ensure_gitignore_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path().to_string_lossy().to_string();
        record_write(&vault, "t", "a.md", None, Some(b"a")).unwrap();
        record_write(&vault, "t", "b.md", None, Some(b"b")).unwrap();
        let body = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        // Rule appears exactly once even after two writes.
        assert_eq!(body.matches(GITIGNORE_RULE).count(), 1);
    }

    #[test]
    fn blob_dedup_on_identical_content() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path().to_string_lossy().to_string();
        record_write(&vault, "t", "a.md", None, Some(b"same")).unwrap();
        record_write(&vault, "t", "b.md", None, Some(b"same")).unwrap();
        // One blob file (sha-named, deduped) plus the blobs dir
        // itself.
        let blobs_dir = dir.path().join(".rezon-history/blobs");
        let count = fs::read_dir(&blobs_dir).unwrap().count();
        assert_eq!(count, 1);
    }

    #[test]
    fn last_undoable_picks_most_recent_unreverted_write() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path().to_string_lossy().to_string();
        let a = record_write(&vault, "t", "a.md", None, Some(b"a1")).unwrap();
        let _b = record_write(&vault, "t", "a.md", Some(b"a1"), Some(b"a2")).unwrap();
        // Mark the second one as undone.
        record_undo(&vault, "a.md", &_b.entry.id, Some(b"a2"), Some(b"a1")).unwrap();
        let picked = last_undoable(&vault).unwrap().unwrap();
        assert_eq!(picked.id, a.entry.id);
    }

    #[test]
    fn last_undoable_returns_none_when_nothing_done() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path().to_string_lossy().to_string();
        assert!(last_undoable(&vault).unwrap().is_none());
    }

    #[test]
    fn gc_truncates_log_and_prunes_orphan_blobs() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path().to_string_lossy().to_string();
        for i in 0..6u32 {
            record_write(
                &vault,
                "t",
                &format!("f{i}.md"),
                None,
                Some(format!("v{i}").as_bytes()),
            )
            .unwrap();
        }
        // Sanity: 6 entries written.
        assert_eq!(read_log(dir.path()).unwrap().len(), 6);
        let dropped = gc(dir.path(), 2).unwrap();
        assert_eq!(dropped, 4);
        let kept = read_log(dir.path()).unwrap();
        assert_eq!(kept.len(), 2);
        // Only the surviving entries' blobs remain.
        let blobs: std::collections::HashSet<String> =
            fs::read_dir(dir.path().join(".rezon-history/blobs"))
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
        assert_eq!(blobs.len(), 2);
    }

    #[test]
    fn gc_noop_when_under_cap() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path().to_string_lossy().to_string();
        record_write(&vault, "t", "f.md", None, Some(b"x")).unwrap();
        let dropped = gc(dir.path(), 10).unwrap();
        assert_eq!(dropped, 0);
        assert_eq!(read_log(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn undo_last_op_restores_prior_content() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path().to_string_lossy().to_string();
        // Two writes: undo_last_op should revert the second one
        // and leave the first content on disk.
        fs::write(dir.path().join("X.md"), b"v1").unwrap();
        record_write(&vault, "t", "X.md", None, Some(b"v1")).unwrap();
        fs::write(dir.path().join("X.md"), b"v2").unwrap();
        record_write(&vault, "t", "X.md", Some(b"v1"), Some(b"v2")).unwrap();

        let out = undo_last_op(&vault).unwrap().expect("had an undoable");
        assert_eq!(out.path, "X.md");
        assert_eq!(out.tool, "t");
        assert!(!out.was_deletion);
        assert_eq!(fs::read(dir.path().join("X.md")).unwrap(), b"v1");
        // The undo entry was recorded — so a second undo finds the
        // *first* write (the chain walks back, skipping undone
        // entries).
        let second = undo_last_op(&vault).unwrap().expect("had a first write");
        assert!(second.was_deletion);
        assert!(!dir.path().join("X.md").exists());
    }

    #[test]
    fn redo_cycle_round_trips_undo_then_redo() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path().to_string_lossy().to_string();
        fs::write(dir.path().join("X.md"), b"v1").unwrap();
        record_write(&vault, "t", "X.md", None, Some(b"v1")).unwrap();
        fs::write(dir.path().join("X.md"), b"v2").unwrap();
        record_write(&vault, "t", "X.md", Some(b"v1"), Some(b"v2")).unwrap();

        // Undo brings us back to v1, redo should bring us forward to v2.
        undo_last_op(&vault).unwrap().unwrap();
        assert_eq!(fs::read(dir.path().join("X.md")).unwrap(), b"v1");
        let out = redo_last_op(&vault).unwrap().expect("had a redoable");
        assert_eq!(fs::read(dir.path().join("X.md")).unwrap(), b"v2");
        assert!(!out.was_creation);
    }

    #[test]
    fn redo_invalidated_by_intervening_write() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path().to_string_lossy().to_string();
        fs::write(dir.path().join("X.md"), b"v1").unwrap();
        record_write(&vault, "t", "X.md", None, Some(b"v1")).unwrap();
        fs::write(dir.path().join("X.md"), b"v2").unwrap();
        record_write(&vault, "t", "X.md", Some(b"v1"), Some(b"v2")).unwrap();
        undo_last_op(&vault).unwrap().unwrap();
        // A fresh write after the undo: redo stack must clear.
        fs::write(dir.path().join("X.md"), b"v3").unwrap();
        record_write(&vault, "t", "X.md", Some(b"v1"), Some(b"v3")).unwrap();
        assert!(last_redoable(&vault).unwrap().is_none());
        assert!(redo_last_op(&vault).unwrap().is_none());
    }

    #[test]
    fn redo_recreates_file_after_deletion_undo() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path().to_string_lossy().to_string();
        // Create a file, then journal-delete it, then undo (which
        // restores the file), then redo (which should re-delete it).
        fs::write(dir.path().join("X.md"), b"v1").unwrap();
        record_write(&vault, "t", "X.md", None, Some(b"v1")).unwrap();
        let before = fs::read(dir.path().join("X.md")).unwrap();
        fs::remove_file(dir.path().join("X.md")).unwrap();
        record_write(&vault, "t", "X.md", Some(&before), None).unwrap();
        // Undo: file should come back.
        undo_last_op(&vault).unwrap().unwrap();
        assert!(dir.path().join("X.md").exists());
        // Redo: file should be gone again.
        let out = redo_last_op(&vault).unwrap().expect("had a redoable");
        assert!(!dir.path().join("X.md").exists());
        // Redo's `was_creation` is true when current state was
        // *no file*. Here the file existed before the redo (the
        // undo restored it), so was_creation = false.
        assert!(!out.was_creation);
    }

    #[test]
    fn undo_last_op_returns_none_when_nothing_to_undo() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path().to_string_lossy().to_string();
        assert!(undo_last_op(&vault).unwrap().is_none());
    }

    #[test]
    fn skip_git_sentinel_suppresses_commit() {
        let dir = TempDir::new().unwrap();
        // Sentinel present before the first write — no warning, no
        // commit attempted.
        fs::write(dir.path().join(SKIP_GIT_SENTINEL), "").unwrap();
        let vault = dir.path().to_string_lossy().to_string();
        let out = record_write(&vault, "t", "a.md", None, Some(b"hi")).unwrap();
        assert!(!out.git_committed);
        assert!(out.git_warning.is_none());
        // .git was never created.
        assert!(!dir.path().join(".git").exists());
    }
}
