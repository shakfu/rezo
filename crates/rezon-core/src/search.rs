// Full-text search index for vault notes.
//
// One SQLite database per vault, stored under the app's data dir
// keyed by a hash of the vault path. The schema is:
//
//   files(path PRIMARY KEY, mtime, size)   -- change detection
//   notes USING fts5(path UNINDEXED, content, tokenize='porter unicode61')
//
// The `files` table lets us decide whether a file's FTS row is stale
// without keeping content in two places: we re-read from disk and
// re-tokenize only when (mtime, size) differs from the recorded row.
//
// A `notify` watcher per vault runs on its own OS thread and reapplies
// changes incrementally. The state holds a Weak<> back into the
// VaultIndex so dropping the state cleanly stops the thread.

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, SystemTime};

/// Register sqlite-vec as a sqlite3 auto extension. Called exactly once at
/// app boot. After this every `Connection::open` (including those created
/// by `rusqlite::Connection::open`) has `vec_*` functions available and
/// can `CREATE VIRTUAL TABLE ... USING vec0(...)`.
pub fn register_sqlite_vec() {
    // We re-type sqlite-vec's init function (its public symbol is
    // exposed as an extern "C" raw fn) into the signature
    // sqlite3_auto_extension wants. Source and target are both
    // `unsafe extern "C" fn(...) -> ...` with the same calling
    // convention; the cast is sound, just opaque to the compiler.
    #[allow(clippy::missing_transmute_annotations)]
    unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    }
}

pub struct SearchState {
    /// Per-process root the per-vault SQLite databases live under.
    /// Resolved once by the shell (Tauri's `app_data_dir` for
    /// rezon-web; `directories::ProjectDirs` for rezon-tui) and
    /// passed in at construction.
    data_dir: PathBuf,
    inner: Mutex<HashMap<String, Arc<Mutex<VaultIndex>>>>,
    // The path of the vault the user most recently opened. Used by
    // the `search_notes` agent tool, which has no FE-supplied vault
    // argument and needs to resolve "the user's current vault" on its
    // own. Set whenever `vault_index_open` is called.
    active_vault: Mutex<Option<String>>,
}

impl SearchState {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            inner: Mutex::new(HashMap::new()),
            active_vault: Mutex::new(None),
        }
    }

    pub fn active_vault(&self) -> Option<String> {
        self.active_vault.lock().ok().and_then(|g| g.clone())
    }

    fn set_active_vault(&self, vault: &str) {
        if let Ok(mut g) = self.active_vault.lock() {
            *g = Some(vault.to_string());
        }
    }
}

impl SearchState {
    /// Accessor for the embedder's catch-up loop: returns a snapshot
    /// of the (vault path -> index) map so it can iterate without
    /// holding the outer mutex across the entire pass.
    pub fn inner_for_embed(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Mutex<VaultIndex>>>> {
        self.inner.lock().unwrap()
    }

    pub fn shutdown(&self) {
        let mut guard = self.inner.lock().unwrap();
        for (_, v) in guard.drain() {
            // Signal each watcher to exit and drop its watcher handle.
            if let Ok(mut idx) = v.lock() {
                idx.stop_watcher();
            }
        }
    }

    /// Drop a single vault's index handle: stops its file watcher,
    /// closes its SQLite connection, and clears `active_vault` if it
    /// pointed at the same path. Returns `true` if a handle was
    /// actually removed.
    pub fn close_vault(&self, vault: &str) -> bool {
        let removed = {
            let mut map = self.inner.lock().unwrap();
            map.remove(vault)
        };
        let Some(idx) = removed else {
            return false;
        };
        if let Ok(mut g) = idx.lock() {
            g.stop_watcher();
        }
        if let Ok(mut a) = self.active_vault.lock() {
            if a.as_deref() == Some(vault) {
                *a = None;
            }
        }
        true
    }
}

pub struct VaultIndex {
    vault: PathBuf,
    db: Connection,
    // Held to keep the watcher alive. Dropping it stops notifications.
    _watcher: Option<RecommendedWatcher>,
    // Signal the worker thread to exit when the state is dropped.
    stop: Option<Sender<()>>,
}

impl VaultIndex {
    fn stop_watcher(&mut self) {
        // Dropping the watcher first stops further notify events; then
        // we close the worker channel by dropping the sender.
        self._watcher.take();
        self.stop.take();
    }
}

#[derive(Serialize)]
pub struct SearchHit {
    pub path: String,
    pub snippet: String,
}

fn vault_db_path(data_dir: &Path, vault: &Path) -> Result<PathBuf, String> {
    let key = {
        let mut h = Sha256::new();
        h.update(vault.to_string_lossy().as_bytes());
        let digest = h.finalize();
        let hex: String = digest
            .iter()
            .take(8)
            .map(|b| format!("{:02x}", b))
            .collect();
        hex
    };
    fs::create_dir_all(data_dir.join("vaults")).map_err(|e| format!("create app data dir: {e}"))?;
    Ok(data_dir.join("vaults").join(format!("{key}.db")))
}

fn init_schema(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS files (
            path  TEXT PRIMARY KEY,
            mtime INTEGER NOT NULL,
            size  INTEGER NOT NULL
         );
         CREATE VIRTUAL TABLE IF NOT EXISTS notes USING fts5(
            path UNINDEXED,
            content,
            tokenize='porter unicode61'
         );
         CREATE TABLE IF NOT EXISTS chunks (
            id     INTEGER PRIMARY KEY,
            path   TEXT NOT NULL,
            ord    INTEGER NOT NULL,
            char_start INTEGER NOT NULL,
            char_end   INTEGER NOT NULL,
            text   TEXT NOT NULL,
            dirty  INTEGER NOT NULL DEFAULT 1
         );
         CREATE INDEX IF NOT EXISTS idx_chunks_path  ON chunks(path);
         CREATE INDEX IF NOT EXISTS idx_chunks_dirty ON chunks(dirty);
         CREATE TABLE IF NOT EXISTS meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
         );",
    )
}

/// Drop and recreate `vec_chunks` with the supplied dimension, and mark
/// every chunk as dirty so the worker re-embeds them. Called when the
/// loaded embedding model's dim differs from the value previously
/// recorded in `meta`.
fn reset_vec_table(db: &Connection, dim: usize) -> rusqlite::Result<()> {
    db.execute("DROP TABLE IF EXISTS vec_chunks", [])?;
    db.execute(
        &format!("CREATE VIRTUAL TABLE vec_chunks USING vec0(embedding float[{dim}])"),
        [],
    )?;
    db.execute("UPDATE chunks SET dirty = 1", [])?;
    db.execute(
        "INSERT INTO meta(key, value) VALUES('embed_dim', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![dim.to_string()],
    )?;
    Ok(())
}

fn current_embed_dim(db: &Connection) -> rusqlite::Result<Option<usize>> {
    let v: Option<String> = db
        .query_row("SELECT value FROM meta WHERE key='embed_dim'", [], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(v.and_then(|s| s.parse().ok()))
}

fn is_markdown(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let lo = e.to_lowercase();
            lo == "md" || lo == "markdown"
        })
        .unwrap_or(false)
}

fn walk_markdown(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(read) = fs::read_dir(root) else {
        return;
    };
    for ent in read.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let path = ent.path();
        let Ok(ft) = ent.file_type() else { continue };
        if ft.is_dir() {
            walk_markdown(&path, out);
        } else if ft.is_file() && is_markdown(&path) {
            out.push(path);
        }
    }
}

fn file_meta(p: &Path) -> Option<(i64, i64)> {
    let md = fs::metadata(p).ok()?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let size = md.len() as i64;
    Some((mtime, size))
}

/// Split markdown text into overlapping chunks suitable for embedding.
/// Strategy: split on blank-line boundaries to get paragraphs; greedily
/// pack paragraphs into windows up to MAX_CHARS; carry the last
/// paragraph of each window into the next for overlap. Markdown
/// headings are treated as their own paragraph so window splits never
/// land mid-heading.
///
/// Returns (char_start, char_end, text) tuples. char positions are
/// byte offsets into the original string; the FE uses them to scroll
/// to a chunk's location in the editor later.
pub fn chunk_markdown(text: &str) -> Vec<(usize, usize, String)> {
    const MAX_CHARS: usize = 1800;

    // Split into paragraphs by blank lines, recording the byte range of
    // each paragraph in the original string. Trailing whitespace on
    // each paragraph is preserved so concatenation reproduces the
    // original locally.
    let mut paragraphs: Vec<(usize, usize, &str)> = Vec::new();
    let mut start = 0usize;
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // Find the next blank-line boundary: \n\n or end of input.
        let rest = &text[i..];
        if let Some(rel) = rest.find("\n\n") {
            let end = i + rel;
            if end > start {
                paragraphs.push((start, end, &text[start..end]));
            }
            i = end + 2;
            start = i;
        } else {
            paragraphs.push((start, text.len(), &text[start..]));
            break;
        }
    }
    if start < text.len() && paragraphs.last().map(|p| p.1) != Some(text.len()) {
        paragraphs.push((start, text.len(), &text[start..]));
    }

    let mut out: Vec<(usize, usize, String)> = Vec::new();
    let mut cur_start: Option<usize> = None;
    let mut cur_end: usize = 0;
    let mut cur_text = String::new();
    let mut prev_para: Option<(usize, usize, &str)> = None;

    let flush = |out: &mut Vec<(usize, usize, String)>,
                 cur_start: &mut Option<usize>,
                 cur_end: &mut usize,
                 cur_text: &mut String| {
        if let Some(s) = cur_start.take() {
            if !cur_text.trim().is_empty() {
                out.push((s, *cur_end, std::mem::take(cur_text)));
            } else {
                cur_text.clear();
            }
            *cur_end = 0;
        }
    };

    for &(s, e, p) in &paragraphs {
        let p_len = e - s;
        // If a single paragraph itself exceeds MAX_CHARS, emit it on
        // its own (no further splitting — embedding models truncate
        // gracefully; we'd rather over-embed than fragment a code
        // block or table mid-row).
        if cur_text.len() + p_len > MAX_CHARS && !cur_text.is_empty() {
            flush(&mut out, &mut cur_start, &mut cur_end, &mut cur_text);
            // Start the next window with the previous paragraph as
            // overlap, if it exists and isn't itself this paragraph.
            if let Some((ps, pe, pt)) = prev_para {
                if pe <= s {
                    cur_start = Some(ps);
                    // `cur_end` is intentionally not set here — the
                    // very next statement after this block (`cur_end
                    // = e`) overwrites it with the end of the
                    // current paragraph, which is what `flush` needs.
                    cur_text.push_str(pt);
                    cur_text.push_str("\n\n");
                }
            }
        }
        if cur_start.is_none() {
            cur_start = Some(s);
        }
        cur_end = e;
        cur_text.push_str(p);
        cur_text.push_str("\n\n");
        prev_para = Some((s, e, p));
    }
    flush(&mut out, &mut cur_start, &mut cur_end, &mut cur_text);
    out
}

fn rewrite_chunks(db: &Connection, path: &Path, content: &str) -> rusqlite::Result<()> {
    let path_str = path.to_string_lossy().to_string();
    db.execute("DELETE FROM chunks WHERE path = ?1", params![path_str])?;
    // vec_chunks rows are linked by chunks.id; chunks are about to
    // disappear so the orphaned vec rows must go too. The table may
    // not exist yet (no embedding model loaded) — ignore that case.
    let _ = db.execute(
        "DELETE FROM vec_chunks WHERE rowid IN (SELECT id FROM chunks WHERE path = ?1)",
        params![path_str],
    );
    let mut stmt = db.prepare(
        "INSERT INTO chunks (path, ord, char_start, char_end, text, dirty)
         VALUES (?1, ?2, ?3, ?4, ?5, 1)",
    )?;
    for (ord, (start, end, text)) in chunk_markdown(content).into_iter().enumerate() {
        stmt.execute(params![
            path_str,
            ord as i64,
            start as i64,
            end as i64,
            text
        ])?;
    }
    Ok(())
}

fn upsert_file(db: &Connection, path: &Path) -> rusqlite::Result<()> {
    let path_str = path.to_string_lossy().to_string();
    let (mtime, size) = match file_meta(path) {
        Some(v) => v,
        None => return Ok(()),
    };

    // Skip if (mtime, size) match the recorded row.
    let row: Option<(i64, i64)> = db
        .query_row(
            "SELECT mtime, size FROM files WHERE path = ?1",
            params![path_str],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((m, s)) = row {
        if m == mtime && s == size {
            return Ok(());
        }
    }

    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };

    db.execute("DELETE FROM notes WHERE path = ?1", params![path_str])?;
    db.execute(
        "INSERT INTO notes (path, content) VALUES (?1, ?2)",
        params![path_str, content],
    )?;
    rewrite_chunks(db, path, &content)?;
    db.execute(
        "INSERT INTO files (path, mtime, size) VALUES (?1, ?2, ?3)
         ON CONFLICT(path) DO UPDATE SET mtime = excluded.mtime, size = excluded.size",
        params![path_str, mtime, size],
    )?;
    Ok(())
}

fn remove_file(db: &Connection, path: &Path) -> rusqlite::Result<()> {
    let path_str = path.to_string_lossy().to_string();
    db.execute("DELETE FROM notes WHERE path = ?1", params![path_str])?;
    let _ = db.execute(
        "DELETE FROM vec_chunks WHERE rowid IN (SELECT id FROM chunks WHERE path = ?1)",
        params![path_str],
    );
    db.execute("DELETE FROM chunks WHERE path = ?1", params![path_str])?;
    db.execute("DELETE FROM files WHERE path = ?1", params![path_str])?;
    Ok(())
}

fn full_reindex(db: &Connection, vault: &Path) -> rusqlite::Result<()> {
    let mut found: Vec<PathBuf> = Vec::new();
    walk_markdown(vault, &mut found);

    // Upsert every present file.
    for p in &found {
        upsert_file(db, p)?;
    }

    // Delete rows for files that no longer exist.
    let present: std::collections::HashSet<String> = found
        .into_iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();

    let mut to_remove: Vec<String> = Vec::new();
    {
        let mut stmt = db.prepare("SELECT path FROM files")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows.flatten() {
            if !present.contains(&row) {
                to_remove.push(row);
            }
        }
    }
    for path_str in to_remove {
        db.execute("DELETE FROM notes WHERE path = ?1", params![path_str])?;
        let _ = db.execute(
            "DELETE FROM vec_chunks WHERE rowid IN (SELECT id FROM chunks WHERE path = ?1)",
            params![path_str],
        );
        db.execute("DELETE FROM chunks WHERE path = ?1", params![path_str])?;
        db.execute("DELETE FROM files WHERE path = ?1", params![path_str])?;
    }
    Ok(())
}

fn open_or_create(data_dir: &Path, vault: &Path) -> Result<Arc<Mutex<VaultIndex>>, String> {
    let db_path = vault_db_path(data_dir, vault)?;
    let db = Connection::open(&db_path).map_err(|e| format!("open db: {e}"))?;
    db.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| format!("WAL: {e}"))?;
    init_schema(&db).map_err(|e| format!("init schema: {e}"))?;
    full_reindex(&db, vault).map_err(|e| format!("reindex: {e}"))?;

    let idx = Arc::new(Mutex::new(VaultIndex {
        vault: vault.to_path_buf(),
        db,
        _watcher: None,
        stop: None,
    }));

    start_watcher(Arc::downgrade(&idx))?;
    Ok(idx)
}

fn start_watcher(weak: Weak<Mutex<VaultIndex>>) -> Result<(), String> {
    // Resolve the vault path once for the watcher; we need it to scope
    // notify and to compute relative event paths.
    let strong = match weak.upgrade() {
        Some(s) => s,
        None => return Ok(()),
    };
    let vault = strong.lock().unwrap().vault.clone();

    // notify uses sync channels; the worker thread drains them and
    // calls back into the index. A separate `stop` channel lets us
    // shut down deterministically when state is torn down.
    let (notify_tx, notify_rx) = channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = notify_tx.send(res);
    })
    .map_err(|e| format!("watcher: {e}"))?;
    watcher
        .watch(&vault, RecursiveMode::Recursive)
        .map_err(|e| format!("watch: {e}"))?;

    let (stop_tx, stop_rx) = channel::<()>();
    let weak_for_thread = weak.clone();
    thread::spawn(move || {
        loop {
            // Cheap shutdown check.
            if stop_rx.try_recv().is_ok() {
                break;
            }
            let evt = match notify_rx.recv_timeout(Duration::from_millis(300)) {
                Ok(e) => e,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            };
            let Ok(event) = evt else { continue };
            let Some(strong) = weak_for_thread.upgrade() else {
                break;
            };
            let Ok(idx) = strong.lock() else { continue };
            apply_event(&idx, &event);
        }
    });

    // Store watcher + stop sender so dropping the index stops the thread.
    if let Some(strong) = weak.upgrade() {
        let mut g = strong.lock().unwrap();
        g._watcher = Some(watcher);
        g.stop = Some(stop_tx);
    }
    Ok(())
}

fn apply_event(idx: &VaultIndex, event: &Event) {
    let touched: Vec<&PathBuf> = event
        .paths
        .iter()
        .filter(|p| is_markdown(p) || matches!(event.kind, EventKind::Remove(_)))
        .collect();
    for p in touched {
        match event.kind {
            EventKind::Remove(_) => {
                let _ = remove_file(&idx.db, p);
            }
            _ => {
                if p.is_file() {
                    let _ = upsert_file(&idx.db, p);
                } else if !p.exists() {
                    let _ = remove_file(&idx.db, p);
                }
            }
        }
    }
}

fn get_or_open(state: &SearchState, vault: &str) -> Result<Arc<Mutex<VaultIndex>>, String> {
    let mut map = state.inner.lock().unwrap();
    if let Some(v) = map.get(vault) {
        return Ok(v.clone());
    }
    let vp = PathBuf::from(vault);
    let idx = open_or_create(&state.data_dir, &vp)?;
    map.insert(vault.to_string(), idx.clone());
    Ok(idx)
}

/// FTS5 search implementation. Shells wrap this in their own
/// command/handler surface.
pub fn vault_search_impl(
    state: &SearchState,
    vault: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchHit>, String> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(Vec::new());
    }
    let idx = get_or_open(state, vault)?;
    let guard = idx.lock().map_err(|_| "search lock".to_string())?;
    // FTS5 MATCH wants its own query syntax. Quote each whitespace
    // token so user input like `foo bar` becomes `"foo" "bar"`, which
    // is implicit AND across phrases without interpreting special
    // characters. Empty tokens are skipped.
    let fts_query: String = q
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .map(|t| {
            let escaped = t.replace('"', "\"\"");
            format!("\"{}\"", escaped)
        })
        .collect::<Vec<_>>()
        .join(" ");
    if fts_query.is_empty() {
        return Ok(Vec::new());
    }

    let lim = limit.clamp(1, 500) as i64;
    let mut stmt = guard
        .db
        .prepare(
            "SELECT path, snippet(notes, 1, '<<', '>>', '...', 12)
             FROM notes
             WHERE notes MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )
        .map_err(|e| format!("prepare: {e}"))?;
    let rows = stmt
        .query_map(params![fts_query, lim], |r| {
            Ok(SearchHit {
                path: r.get(0)?,
                snippet: r.get(1)?,
            })
        })
        .map_err(|e| format!("query: {e}"))?;
    let mut out = Vec::new();
    for hit in rows.flatten() {
        out.push(hit);
    }
    Ok(out)
}

// ---- Vector helpers (used by the embedder worker) -------------------

#[derive(Debug, Clone)]
pub struct DirtyChunk {
    pub id: i64,
    pub text: String,
}

/// Internal API for the embedder. Returns the (already-open) vault
/// index for a path, opening it if needed.
pub fn open_vault(state: &SearchState, vault: &str) -> Result<Arc<Mutex<VaultIndex>>, String> {
    get_or_open(state, vault)
}

impl VaultIndex {
    pub fn ensure_vec_table(&self, dim: usize) -> Result<(), String> {
        match current_embed_dim(&self.db).map_err(|e| e.to_string())? {
            Some(d) if d == dim => Ok(()),
            _ => reset_vec_table(&self.db, dim).map_err(|e| e.to_string()),
        }
    }

    pub fn take_dirty_chunks(&self, limit: usize) -> Result<Vec<DirtyChunk>, String> {
        let mut stmt = self
            .db
            .prepare("SELECT id, text FROM chunks WHERE dirty = 1 ORDER BY id LIMIT ?1")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![limit as i64], |r| {
                Ok(DirtyChunk {
                    id: r.get(0)?,
                    text: r.get(1)?,
                })
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| e.to_string())?);
        }
        Ok(out)
    }

    /// Return the embedding dimension recorded in `meta`, if any. The
    /// embedder uses this to skip work when no model has been loaded.
    pub fn embed_dim(&self) -> Option<usize> {
        current_embed_dim(&self.db).ok().flatten()
    }

    /// Run a KNN search against `vec_chunks` for an externally-supplied
    /// query embedding (must already be L2-normalized to match
    /// what we stored). Groups by file, returning at most one row per
    /// path with the closest chunk's snippet. Used by both the
    /// semantic-search FE command and the `search_notes` agent tool.
    pub fn knn_search(&self, query: &[f32], limit: usize) -> Result<Vec<SearchHit>, String> {
        let dim = match self.embed_dim() {
            Some(d) => d,
            None => return Ok(Vec::new()),
        };
        if query.len() != dim {
            return Err(format!(
                "query dim {} does not match index dim {}",
                query.len(),
                dim
            ));
        }
        let mut blob: Vec<u8> = Vec::with_capacity(dim * 4);
        for f in query {
            blob.extend_from_slice(&f.to_le_bytes());
        }
        let k = (limit * 5).max(20) as i64;
        let mut stmt = self
            .db
            .prepare(
                "SELECT c.path, c.text, v.distance FROM vec_chunks v
                 JOIN chunks c ON c.id = v.rowid
                 WHERE v.embedding MATCH ?1 AND k = ?2
                 ORDER BY v.distance",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![blob, k], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, f64>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut best: std::collections::HashMap<String, (f64, String)> =
            std::collections::HashMap::new();
        for row in rows {
            let (p, text, dist) = row.map_err(|e| e.to_string())?;
            let entry = best.entry(p).or_insert((f64::INFINITY, String::new()));
            if dist < entry.0 {
                entry.0 = dist;
                let snippet = if text.chars().count() > 200 {
                    let mut s: String = text.chars().take(200).collect();
                    s.push('…');
                    s
                } else {
                    text
                };
                entry.1 = snippet;
            }
        }
        let mut ranked: Vec<(f64, String, String)> = best
            .into_iter()
            .map(|(path, (dist, snippet))| (dist, path, snippet))
            .collect();
        ranked.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(limit);
        Ok(ranked
            .into_iter()
            .map(|(_d, path, snippet)| SearchHit { path, snippet })
            .collect())
    }

    pub fn write_embeddings(&self, rows: &[(i64, Vec<f32>)]) -> Result<(), String> {
        let tx = self.db.unchecked_transaction().map_err(|e| e.to_string())?;
        for (id, emb) in rows {
            // vec0 stores fixed-length float arrays; serialize as the
            // raw little-endian byte buffer that sqlite-vec expects.
            let mut buf: Vec<u8> = Vec::with_capacity(emb.len() * 4);
            for f in emb {
                buf.extend_from_slice(&f.to_le_bytes());
            }
            tx.execute("DELETE FROM vec_chunks WHERE rowid = ?1", params![id])
                .map_err(|e| e.to_string())?;
            tx.execute(
                "INSERT INTO vec_chunks(rowid, embedding) VALUES (?1, ?2)",
                params![id, buf],
            )
            .map_err(|e| e.to_string())?;
            tx.execute("UPDATE chunks SET dirty = 0 WHERE id = ?1", params![id])
                .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())
    }
}

/// Force the index to be initialized for `vault`. Shells call this
/// when the user picks a vault so the watcher is running before the
/// first search.
pub fn vault_index_open(state: &SearchState, vault: &str) -> Result<(), String> {
    let _ = get_or_open(state, vault)?;
    state.set_active_vault(vault);
    Ok(())
}

#[derive(Serialize)]
pub struct RelatedHit {
    pub path: String,
    pub score: f32,
    pub snippet: String,
}

/// "Related notes" for the file at `path`: averages its non-dirty
/// chunk embeddings into a single query vector, runs a KNN against
/// `vec_chunks`, groups by file, and returns the best chunk-snippet
/// per file. Returns an empty list when the model isn't loaded or the
/// file has no embedded chunks yet.
pub fn vault_related(
    state: &SearchState,
    vault: &str,
    path: &str,
    limit: u32,
) -> Result<Vec<RelatedHit>, String> {
    let lim = limit.clamp(1, 50) as i64;
    let idx = get_or_open(state, vault)?;
    let guard = idx.lock().map_err(|_| "related lock".to_string())?;
    let db = &guard.db;

    let dim = match current_embed_dim(db).map_err(|e| e.to_string())? {
        Some(d) => d,
        None => return Ok(Vec::new()),
    };

    // Compute mean embedding for the source file. We pull the raw
    // BLOBs (little-endian f32) for the file's chunks via a join.
    let mut stmt = db
        .prepare(
            "SELECT v.embedding FROM vec_chunks v
             JOIN chunks c ON c.id = v.rowid
             WHERE c.path = ?1 AND c.dirty = 0",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![path], |r| r.get::<_, Vec<u8>>(0))
        .map_err(|e| e.to_string())?;

    let mut sum: Vec<f32> = vec![0.0; dim];
    let mut count = 0usize;
    for r in rows {
        let buf = r.map_err(|e| e.to_string())?;
        if buf.len() != dim * 4 {
            continue;
        }
        for (i, chunk) in buf.as_chunks::<4>().0.iter().enumerate() {
            sum[i] += f32::from_le_bytes(*chunk);
        }
        count += 1;
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    for v in &mut sum {
        *v /= count as f32;
    }
    // L2-normalize so cosine distance aligns with vec0's default L2.
    let norm: f32 = sum.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in &mut sum {
            *v /= norm;
        }
    }
    let mut query_blob: Vec<u8> = Vec::with_capacity(dim * 4);
    for f in &sum {
        query_blob.extend_from_slice(&f.to_le_bytes());
    }

    // Ask for more rows than `lim` so we have headroom to drop hits
    // that point back at the source file.
    let knn_k = (lim as usize * 5).max(20) as i64;
    let mut stmt = db
        .prepare(
            "SELECT c.path, c.text, v.distance FROM vec_chunks v
             JOIN chunks c ON c.id = v.rowid
             WHERE v.embedding MATCH ?1 AND k = ?2
             ORDER BY v.distance",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![query_blob, knn_k], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, f64>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?;

    let mut best: std::collections::HashMap<String, (f64, String)> =
        std::collections::HashMap::new();
    for r in rows {
        let (p, text, dist) = r.map_err(|e| e.to_string())?;
        if p == path {
            continue;
        }
        let entry = best.entry(p).or_insert((f64::INFINITY, String::new()));
        if dist < entry.0 {
            entry.0 = dist;
            // Trim snippet to ~160 chars for the UI.
            let snippet = if text.chars().count() > 160 {
                let mut s: String = text.chars().take(160).collect();
                s.push('…');
                s
            } else {
                text
            };
            entry.1 = snippet;
        }
    }
    let mut hits: Vec<RelatedHit> = best
        .into_iter()
        .map(|(path, (dist, snippet))| RelatedHit {
            path,
            // Convert L2 distance to a 0..1 similarity-ish score.
            // sqlite-vec returns squared L2 for the default metric.
            score: (1.0 / (1.0 + dist as f32)).clamp(0.0, 1.0),
            snippet,
        })
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(lim as usize);
    Ok(hits)
}

/// Touch (re-upsert) the FTS+chunks rows for a single file. Shells
/// call this after writing a file from within the app so the user
/// sees fresh search results without waiting for the notify event
/// (which on macOS can lag). The shell is also responsible for waking
/// any embedder catch-up loop it owns.
pub fn vault_index_touch(state: &SearchState, vault: &str, path: &str) -> Result<(), String> {
    let idx = get_or_open(state, vault)?;
    let guard = idx.lock().map_err(|_| "touch lock".to_string())?;
    let _ = upsert_file(&guard.db, Path::new(path));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_markdown_short_input_is_single_chunk() {
        let chunks = chunk_markdown("hello world\n\nshort note");
        assert_eq!(chunks.len(), 1);
        let (start, end, _) = &chunks[0];
        assert_eq!(*start, 0);
        assert_eq!(*end, "hello world\n\nshort note".len());
    }

    #[test]
    fn chunk_markdown_splits_on_blank_line_when_window_overflows() {
        // Two ~1500-char paragraphs separated by blank line should
        // emit at least two chunks once the cumulative window
        // exceeds MAX_CHARS (1800).
        let para_a = "A".repeat(1500);
        let para_b = "B".repeat(1500);
        let input = format!("{para_a}\n\n{para_b}");
        let chunks = chunk_markdown(&input);
        assert!(
            chunks.len() >= 2,
            "expected splitting at the paragraph boundary, got {} chunks",
            chunks.len()
        );
        // First chunk should start at 0; subsequent chunks should
        // begin at or after the boundary of the previous one.
        assert_eq!(chunks[0].0, 0);
        for window in chunks.windows(2) {
            assert!(window[1].0 >= window[0].0, "char_start must be monotonic");
        }
    }

    #[test]
    fn chunk_markdown_char_offsets_cover_input() {
        let input = "paragraph one\n\nparagraph two\n\nparagraph three";
        let chunks = chunk_markdown(input);
        // The first chunk's start is 0 and the last chunk's end is
        // the full input length (chunker may include the final
        // paragraph in a window that spans to text.len()).
        assert_eq!(chunks.first().unwrap().0, 0);
        assert_eq!(chunks.last().unwrap().1, input.len());
    }
}
