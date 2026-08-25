// Vault write tools — `write_note`, `append_note`, `edit_note`.
//
// All three live here because they share path normalization, the
// active-vault lookup, and the index-touch hygiene step. They're
// registered together by `register_write_note` (kept under the
// historical name even though it now installs three tools) so
// permissions and registration stay symmetrical with
// `register_search_notes`.
//
// Always-confirmed: any one of these can delete or rewrite the
// user's notes. Auto-approval is the wrong default even for power
// users. Each tool overrides `preview()` to render a textual diff
// the confirmation UI can show in place of raw JSON args.
//
// Path safety:
//   * `path` is treated as relative to the vault root. Leading `/`
//     stripped; `..` segments rejected (we never canonicalize before
//     calling `vault_write`, but `vault_write` itself runs the
//     `within` containment check).
//   * `.md` extension auto-appended when missing — the convention
//     across the vault tooling.
//
// Index hygiene:
//   * After a successful write, we touch the file in the FTS index
//     so `search_notes` finds it on the next call without waiting
//     for the file-watcher's debounce.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::tool::{Tool, ToolContext, ToolError};
use crate::journal;
use crate::search::{vault_index_touch, SearchState};
use crate::vault::{vault_read, vault_write};

/// Maximum lines included in a confirmation preview. Long bodies
/// are truncated with a trailing `... N more lines` marker so the
/// user can still skim what's about to land on disk without burying
/// the [y/N] prompt below the fold.
const PREVIEW_MAX_LINES: usize = 30;

pub struct WriteNote {
    search: Arc<SearchState>,
}

impl WriteNote {
    pub fn new(search: Arc<SearchState>) -> Self {
        Self { search }
    }
}

#[async_trait]
impl Tool for WriteNote {
    fn name(&self) -> &str {
        "write_note"
    }

    fn description(&self) -> &str {
        "Create or overwrite a markdown note in the user's open vault. \
         `path` is relative to the vault root; subdirectories are \
         created as needed. By default, fails if the file already \
         exists; pass `overwrite: true` to replace. Always prompts the \
         user for confirmation before writing."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative path under the vault root, e.g. 'Skills/Robotics Engineer' or 'Skills/Robotics Engineer.md'. `.md` is appended when missing."
                },
                "content": {
                    "type": "string",
                    "description": "Full markdown body to write."
                },
                "overwrite": {
                    "type": "boolean",
                    "description": "Replace an existing file. Default false (create-only).",
                    "default": false
                }
            },
            "required": ["path", "content"]
        })
    }

    fn requires_confirmation(&self) -> bool {
        true
    }

    fn preview(&self, args: &Value) -> Option<String> {
        let path = args.get("path")?.as_str()?;
        let content = args.get("content")?.as_str()?;
        let overwrite = args
            .get("overwrite")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let rel = normalize_rel(path).ok()?;
        let header = if overwrite {
            format!("write_note  {rel}  (overwrite)")
        } else {
            format!("write_note  {rel}  (create)")
        };
        Some(render_preview(&header, &add_lines(content)))
    }

    async fn dispatch(&self, args: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            content: String,
            #[serde(default)]
            overwrite: bool,
        }
        let args: Args = serde_json::from_value(args)
            .map_err(|e| ToolError::Argument(format!("invalid args: {e}")))?;

        let vault = self.search.active_vault().ok_or_else(|| {
            ToolError::Argument("no vault is open — ask the user to open a vault first".into())
        })?;

        let rel = normalize_rel(&args.path)?;
        let abs = Path::new(&vault).join(&rel);

        if abs.exists() && !args.overwrite {
            return Err(ToolError::Argument(format!(
                "already exists: {rel}  (pass overwrite=true to replace)"
            )));
        }
        let existed = abs.exists();

        // Capture pre-image for the journal before overwriting.
        // None when the file didn't exist; failing to read an
        // existing file aborts the write so we don't blow away
        // content the journal can't recover.
        let before: Option<Vec<u8>> = if existed {
            Some(
                std::fs::read(&abs)
                    .map_err(|e| ToolError::Runtime(anyhow::anyhow!("read pre-image: {e}")))?,
            )
        } else {
            None
        };

        // `vault_write` runs the path-containment check + mkdir +
        // write. We pass the absolute path because that's its
        // contract today; the `within` check inside it is what
        // protects against escapes.
        let after_bytes = args.content.clone().into_bytes();
        vault_write(
            vault.clone(),
            abs.to_string_lossy().into_owned(),
            args.content,
        )
        .map_err(|e| ToolError::Runtime(anyhow::anyhow!(e)))?;

        // Best-effort index update — failure here just means the
        // freshly-written note shows up a moment later via the
        // watcher rather than instantly.
        let _ = vault_index_touch(&self.search, &vault, &abs.to_string_lossy());

        // Journal + (optionally) git-commit. Failures here don't
        // unwind the write — the user's file is already on disk —
        // but they do surface in the result so a debugging UI can
        // show them.
        let journaled = journal::record_write(
            &vault,
            "write_note",
            &rel,
            before.as_deref(),
            Some(&after_bytes),
        );

        Ok(json!({
            "vault": vault,
            "path": rel,
            "absolute_path": abs.to_string_lossy(),
            "created": !existed,
            "journal": journal_report(journaled),
        }))
    }
}

/// Normalize a relative path argument:
///   * trim whitespace
///   * strip leading `/`
///   * reject `..` segments (no escape via traversal)
///   * append `.md` if no extension
fn normalize_rel(input: &str) -> Result<String, ToolError> {
    let mut s = input.trim().to_string();
    if s.is_empty() {
        return Err(ToolError::Argument("path is empty".into()));
    }
    while s.starts_with('/') {
        s.remove(0);
    }
    if s.split('/').any(|seg| seg == "..") {
        return Err(ToolError::Argument(format!(
            "path contains `..` segment: {input}"
        )));
    }
    let p = Path::new(&s);
    let needs_ext = p.extension().is_none()
        || !matches!(
            p.extension()
                .and_then(|e| e.to_str())
                .map(str::to_lowercase)
                .as_deref(),
            Some("md") | Some("markdown"),
        );
    if needs_ext {
        s.push_str(".md");
    }
    Ok(s)
}

/// Format a unified-diff-ish block: header line + the prefixed body
/// lines, truncated past `PREVIEW_MAX_LINES` with a note about how
/// many were elided.
fn render_preview(header: &str, body_lines: &[String]) -> String {
    let mut out =
        String::with_capacity(header.len() + body_lines.iter().map(|s| s.len() + 1).sum::<usize>());
    out.push_str(header);
    out.push('\n');
    let take = body_lines.len().min(PREVIEW_MAX_LINES);
    for line in &body_lines[..take] {
        out.push_str(line);
        out.push('\n');
    }
    if body_lines.len() > take {
        let elided = body_lines.len() - take;
        out.push_str(&format!(
            "  … {elided} more line{}",
            if elided == 1 { "" } else { "s" }
        ));
    }
    out
}

/// Roll a journal outcome into the JSON shape returned by the
/// mutating tools. Both the success and failure paths fold into the
/// same tag set so the model can read a stable shape:
///   `{ entry_id, git_committed, warning? }` on success
///   `{ error: "..." }` when the journal itself failed
fn journal_report(outcome: Result<journal::JournalOutcome, String>) -> Value {
    match outcome {
        Ok(j) => {
            let mut v = json!({
                "entry_id": j.entry.id,
                "git_committed": j.git_committed,
            });
            if let Some(w) = j.git_warning {
                v["warning"] = json!(w);
            }
            v
        }
        Err(e) => json!({ "error": e }),
    }
}

/// Prefix each line with `+ ` for the add side of a diff preview.
fn add_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return vec!["+ (empty)".to_string()];
    }
    text.lines().map(|l| format!("+ {l}")).collect()
}

/// Prefix each line with `- ` for the remove side of a diff preview.
fn del_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return vec!["- (empty)".to_string()];
    }
    text.lines().map(|l| format!("- {l}")).collect()
}

// =========================================================================
// append_note
// =========================================================================

pub struct AppendNote {
    search: Arc<SearchState>,
}

impl AppendNote {
    pub fn new(search: Arc<SearchState>) -> Self {
        Self { search }
    }
}

#[async_trait]
impl Tool for AppendNote {
    fn name(&self) -> &str {
        "append_note"
    }

    fn description(&self) -> &str {
        "Append markdown to the end of an existing note in the user's \
         vault. Cheaper than `write_note` for incremental updates \
         because the model doesn't have to resend the existing body. \
         By default fails if the note doesn't exist; pass \
         `create_if_missing: true` to create it. A newline is inserted \
         between the existing body and the appended content when the \
         existing body doesn't already end with one. Always confirmed."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative path under the vault root. `.md` is appended when missing."
                },
                "content": {
                    "type": "string",
                    "description": "Markdown to append. A leading separator newline is inserted automatically."
                },
                "create_if_missing": {
                    "type": "boolean",
                    "description": "Create the note if it doesn't exist. Default false.",
                    "default": false
                }
            },
            "required": ["path", "content"]
        })
    }

    fn requires_confirmation(&self) -> bool {
        true
    }

    fn preview(&self, args: &Value) -> Option<String> {
        let path = args.get("path")?.as_str()?;
        let content = args.get("content")?.as_str()?;
        let rel = normalize_rel(path).ok()?;
        let header = format!("append_note  {rel}");
        Some(render_preview(&header, &add_lines(content)))
    }

    async fn dispatch(&self, args: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            content: String,
            #[serde(default)]
            create_if_missing: bool,
        }
        let args: Args = serde_json::from_value(args)
            .map_err(|e| ToolError::Argument(format!("invalid args: {e}")))?;

        let vault = self.search.active_vault().ok_or_else(|| {
            ToolError::Argument("no vault is open — ask the user to open a vault first".into())
        })?;

        let rel = normalize_rel(&args.path)?;
        let abs = Path::new(&vault).join(&rel);

        let (existing, existed) = if abs.exists() {
            let body = vault_read(vault.clone(), abs.to_string_lossy().into_owned())
                .map_err(|e| ToolError::Runtime(anyhow::anyhow!(e)))?;
            (body, true)
        } else if args.create_if_missing {
            (String::new(), false)
        } else {
            return Err(ToolError::Argument(format!(
                "not found: {rel}  (pass create_if_missing=true to create)"
            )));
        };

        let before_bytes: Option<Vec<u8>> = if existed {
            Some(existing.clone().into_bytes())
        } else {
            None
        };

        let mut next = existing;
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str(&args.content);
        let after_bytes = next.clone().into_bytes();

        vault_write(vault.clone(), abs.to_string_lossy().into_owned(), next)
            .map_err(|e| ToolError::Runtime(anyhow::anyhow!(e)))?;
        let _ = vault_index_touch(&self.search, &vault, &abs.to_string_lossy());

        let journaled = journal::record_write(
            &vault,
            "append_note",
            &rel,
            before_bytes.as_deref(),
            Some(&after_bytes),
        );

        Ok(json!({
            "vault": vault,
            "path": rel,
            "absolute_path": abs.to_string_lossy(),
            "created": !existed,
            "journal": journal_report(journaled),
        }))
    }
}

// =========================================================================
// edit_note
// =========================================================================

pub struct EditNote {
    search: Arc<SearchState>,
}

impl EditNote {
    pub fn new(search: Arc<SearchState>) -> Self {
        Self { search }
    }
}

#[async_trait]
impl Tool for EditNote {
    fn name(&self) -> &str {
        "edit_note"
    }

    fn description(&self) -> &str {
        "Replace a passage inside an existing vault note. `find` must \
         match the existing body *exactly once*; if it matches zero or \
         multiple times the tool errors out and the model should pass \
         a longer / more specific `find`. Whitespace and newlines are \
         significant. Always confirmed."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative path under the vault root. `.md` is appended when missing."
                },
                "find": {
                    "type": "string",
                    "description": "Verbatim substring to locate. Must occur exactly once."
                },
                "replace": {
                    "type": "string",
                    "description": "Replacement text. May be empty to delete the matched span."
                }
            },
            "required": ["path", "find", "replace"]
        })
    }

    fn requires_confirmation(&self) -> bool {
        true
    }

    fn preview(&self, args: &Value) -> Option<String> {
        let path = args.get("path")?.as_str()?;
        let find = args.get("find")?.as_str()?;
        let replace = args.get("replace")?.as_str()?;
        let rel = normalize_rel(path).ok()?;
        let header = format!("edit_note  {rel}");
        let mut lines = del_lines(find);
        lines.extend(add_lines(replace));
        Some(render_preview(&header, &lines))
    }

    async fn dispatch(&self, args: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            find: String,
            replace: String,
        }
        let args: Args = serde_json::from_value(args)
            .map_err(|e| ToolError::Argument(format!("invalid args: {e}")))?;

        if args.find.is_empty() {
            return Err(ToolError::Argument("`find` is empty".into()));
        }

        let vault = self.search.active_vault().ok_or_else(|| {
            ToolError::Argument("no vault is open — ask the user to open a vault first".into())
        })?;

        let rel = normalize_rel(&args.path)?;
        let abs = Path::new(&vault).join(&rel);

        if !abs.exists() {
            return Err(ToolError::Argument(format!("not found: {rel}")));
        }

        let body = vault_read(vault.clone(), abs.to_string_lossy().into_owned())
            .map_err(|e| ToolError::Runtime(anyhow::anyhow!(e)))?;

        let matches = body.matches(&args.find).count();
        if matches == 0 {
            return Err(ToolError::Argument(format!("`find` not present in {rel}")));
        }
        if matches > 1 {
            return Err(ToolError::Argument(format!(
                "`find` matches {matches} times in {rel} — pass a longer / more specific snippet"
            )));
        }

        let next = body.replacen(&args.find, &args.replace, 1);
        let before_bytes = body.clone().into_bytes();
        let after_bytes = next.clone().into_bytes();

        vault_write(vault.clone(), abs.to_string_lossy().into_owned(), next)
            .map_err(|e| ToolError::Runtime(anyhow::anyhow!(e)))?;
        let _ = vault_index_touch(&self.search, &vault, &abs.to_string_lossy());

        let journaled = journal::record_write(
            &vault,
            "edit_note",
            &rel,
            Some(&before_bytes),
            Some(&after_bytes),
        );

        Ok(json!({
            "vault": vault,
            "path": rel,
            "absolute_path": abs.to_string_lossy(),
            "journal": journal_report(journaled),
        }))
    }
}

// =========================================================================
// undo_note
// =========================================================================

pub struct UndoNote {
    search: Arc<SearchState>,
}

impl UndoNote {
    pub fn new(search: Arc<SearchState>) -> Self {
        Self { search }
    }
}

#[async_trait]
impl Tool for UndoNote {
    fn name(&self) -> &str {
        "undo_note"
    }

    fn description(&self) -> &str {
        "Revert the most recent journaled change to a vault note. \
         Restores the file from the rezon edit journal. Always \
         confirmed."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }

    fn requires_confirmation(&self) -> bool {
        true
    }

    fn preview(&self, _args: &Value) -> Option<String> {
        let vault = self.search.active_vault()?;
        let target = journal::last_undoable(&vault).ok().flatten()?;
        let header = format!("undo_note  {}", target.path);
        let body = match &target.op {
            journal::Op::Write {
                before_sha,
                after_sha,
            } => {
                let mut lines: Vec<String> = Vec::new();
                if let Some(s) = after_sha {
                    if let Ok(bytes) = journal::read_blob(&vault, s) {
                        let text = String::from_utf8_lossy(&bytes);
                        lines.extend(del_lines(&text));
                    }
                }
                match before_sha {
                    Some(s) => {
                        if let Ok(bytes) = journal::read_blob(&vault, s) {
                            let text = String::from_utf8_lossy(&bytes);
                            lines.extend(add_lines(&text));
                        }
                    }
                    None => lines.push("+ (file will be deleted)".into()),
                }
                lines
            }
            _ => return None,
        };
        Some(render_preview(&header, &body))
    }

    async fn dispatch(&self, _args: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        let vault = self
            .search
            .active_vault()
            .ok_or_else(|| ToolError::Argument("no vault is open".into()))?;
        let out = journal::undo_last_op(&vault)
            .map_err(|e| ToolError::Runtime(anyhow::anyhow!(e)))?
            .ok_or_else(|| ToolError::Argument("nothing to undo".into()))?;
        // Keep the FTS index in sync — journal::undo_last_op
        // deliberately doesn't depend on search, so this is the
        // tool's job. Best-effort: a stale entry just means the
        // next `search_notes` call still hits the in-memory hit.
        let abs = Path::new(&vault).join(&out.path);
        let _ = vault_index_touch(&self.search, &vault, &abs.to_string_lossy());

        Ok(json!({
            "vault": vault,
            "path": out.path,
            "target_id": out.target_id,
            "reverted_tool": out.tool,
            "was_deletion": out.was_deletion,
            "journal": {
                "entry_id": out.journal.entry.id,
                "git_committed": out.journal.git_committed,
                "warning": out.journal.git_warning,
            },
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_rel_appends_md_extension() {
        assert_eq!(
            normalize_rel("Skills/Researcher").unwrap(),
            "Skills/Researcher.md"
        );
    }

    #[test]
    fn normalize_rel_keeps_existing_md_extension() {
        assert_eq!(normalize_rel("notes/x.md").unwrap(), "notes/x.md");
    }

    #[test]
    fn normalize_rel_appends_md_to_non_markdown_extension() {
        // A `.txt` should still get `.md` appended — the tool is for
        // markdown notes specifically.
        assert_eq!(normalize_rel("foo.txt").unwrap(), "foo.txt.md");
    }

    #[test]
    fn normalize_rel_strips_leading_slash() {
        assert_eq!(normalize_rel("/abs/looking").unwrap(), "abs/looking.md");
    }

    #[test]
    fn normalize_rel_rejects_dotdot() {
        assert!(normalize_rel("../escape").is_err());
        assert!(normalize_rel("ok/../escape").is_err());
    }

    #[test]
    fn normalize_rel_rejects_empty() {
        assert!(normalize_rel("   ").is_err());
    }

    // ---- preview rendering ------------------------------------------

    #[test]
    fn render_preview_includes_header_and_lines() {
        let p = render_preview("write_note  X.md", &add_lines("hi\nthere"));
        assert!(p.starts_with("write_note  X.md\n"));
        assert!(p.contains("+ hi"));
        assert!(p.contains("+ there"));
    }

    #[test]
    fn render_preview_truncates_past_max_lines() {
        let many: Vec<String> = (0..PREVIEW_MAX_LINES + 5)
            .map(|i| format!("+ line {i}"))
            .collect();
        let p = render_preview("hdr", &many);
        assert!(p.contains("+ line 0"));
        assert!(p.contains(&format!("+ line {}", PREVIEW_MAX_LINES - 1)));
        assert!(!p.contains(&format!("+ line {}", PREVIEW_MAX_LINES)));
        assert!(p.contains("5 more lines"));
    }

    #[test]
    fn add_lines_handles_empty_body() {
        assert_eq!(add_lines(""), vec!["+ (empty)"]);
    }

    #[test]
    fn del_lines_handles_empty_body() {
        assert_eq!(del_lines(""), vec!["- (empty)"]);
    }

    #[test]
    fn write_note_preview_renders_with_overwrite_flag() {
        use crate::search::SearchState;
        use serde_json::json;
        use std::path::PathBuf;
        let tool = WriteNote::new(Arc::new(SearchState::new(PathBuf::from("/tmp"))));
        let p = tool
            .preview(&json!({
                "path": "Skills/X",
                "content": "hello\nworld",
                "overwrite": true,
            }))
            .unwrap();
        assert!(p.contains("Skills/X.md"));
        assert!(p.contains("(overwrite)"));
        assert!(p.contains("+ hello"));
        assert!(p.contains("+ world"));
    }

    #[test]
    fn edit_note_preview_shows_diff_pair() {
        use crate::search::SearchState;
        use serde_json::json;
        use std::path::PathBuf;
        let tool = EditNote::new(Arc::new(SearchState::new(PathBuf::from("/tmp"))));
        let p = tool
            .preview(&json!({
                "path": "n",
                "find": "old",
                "replace": "new",
            }))
            .unwrap();
        assert!(p.contains("- old"));
        assert!(p.contains("+ new"));
    }
}
