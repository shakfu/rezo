// `read_note` — read-only counterpart to `write_note`. Returns the
// full body of a vault note given its relative path. Pairs with
// `search_notes` (which returns snippets, not full content) to close
// the read loop: search finds candidates, read fetches the body.
//
// Why a separate tool rather than reusing user-message wikilink
// expansion: that expansion fires once per turn against the user's
// most-recent message, so it can't be invoked programmatically by
// the agent during a multi-step plan. `read_note` gives the model
// direct, on-demand access without forcing the user to retype.
//
// Read-only and bounded to the active vault, so no confirmation gate
// (mirrors `search_notes`). Path containment is enforced by
// `vault_read` itself via the `within` check.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::tool::{Tool, ToolContext, ToolError};
use crate::search::SearchState;
use crate::vault::vault_read;

pub struct ReadNote {
    search: Arc<SearchState>,
}

impl ReadNote {
    pub fn new(search: Arc<SearchState>) -> Self {
        Self { search }
    }
}

#[async_trait]
impl Tool for ReadNote {
    fn name(&self) -> &str {
        "read_note"
    }

    fn description(&self) -> &str {
        "Read the full markdown body of a note in the user's open \
         vault. `path` is relative to the vault root; `.md` is \
         appended when missing. Pairs with `search_notes` for fetching \
         the full body of a search hit. Read-only."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative path under the vault root, e.g. 'Skills/Researcher' or 'Skills/Researcher.md'."
                }
            },
            "required": ["path"]
        })
    }

    fn requires_confirmation(&self) -> bool {
        false
    }

    async fn dispatch(&self, args: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
        }
        let args: Args = serde_json::from_value(args)
            .map_err(|e| ToolError::Argument(format!("invalid args: {e}")))?;

        let vault = self.search.active_vault().ok_or_else(|| {
            ToolError::Argument("no vault is open — ask the user to open a vault first".into())
        })?;

        let rel = normalize_rel(&args.path)?;
        let abs = Path::new(&vault).join(&rel);
        let content = vault_read(vault.clone(), abs.to_string_lossy().into_owned())
            .map_err(|e| ToolError::Runtime(anyhow::anyhow!(e)))?;

        Ok(json!({
            "vault": vault,
            "path": rel,
            "content": content,
        }))
    }
}

/// Same normalization as `write_note::normalize_rel` but local here
/// to keep the cross-module surface minimal: `..` rejected, leading
/// `/` stripped, `.md` auto-appended when missing.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn make_tool(vault: &str) -> ReadNote {
        let state = SearchState::new(PathBuf::from("/tmp"));
        // SearchState::active_vault is read off an internal set;
        // bypass the index machinery by calling the test-only setter
        // path indirectly via `vault_index_open`. For the unit tests
        // here we don't need the FTS index, just a known active
        // vault, which we install by opening it.
        crate::search::vault_index_open(&state, vault).expect("open vault index");
        ReadNote::new(Arc::new(state))
    }

    #[tokio::test]
    async fn read_note_returns_body_for_existing_file() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path().to_string_lossy().to_string();
        fs::create_dir_all(dir.path().join("Skills")).unwrap();
        fs::write(dir.path().join("Skills/Researcher.md"), "A note body.").unwrap();
        let tool = make_tool(&vault);
        let ctx = ToolContext {
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let out = tool
            .dispatch(json!({ "path": "Skills/Researcher" }), &ctx)
            .await
            .unwrap();
        assert_eq!(out["content"], "A note body.");
        assert_eq!(out["path"], "Skills/Researcher.md");
    }

    #[tokio::test]
    async fn read_note_errors_when_missing() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path().to_string_lossy().to_string();
        let tool = make_tool(&vault);
        let ctx = ToolContext {
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let err = tool
            .dispatch(json!({ "path": "Nope" }), &ctx)
            .await
            .unwrap_err();
        // Surface the path so the model can adjust on the next turn.
        let msg = format!("{err}");
        assert!(msg.contains("Nope") || msg.contains("read"));
    }
}
