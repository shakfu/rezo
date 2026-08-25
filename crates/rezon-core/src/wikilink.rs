// Wikilink expansion. Read-only Phase 1: scans text for `[[target]]`
// tokens, resolves each against the active vault, and returns the
// original text with a `<context>` block appended that carries the
// resolved note bodies.
//
// Storage stays raw — the marker `[[Researcher]]` in a conversation
// is what gets persisted, displayed, and re-resolved each turn — so
// note edits propagate to subsequent turns and users see what they
// typed. Expansion runs at the boundary between the persisted
// conversation and the LLM request: see `expand_for_send` for the
// "system + last user message only" policy that preserves prompt
// caching on prior turns.
//
// Token grammar:
//   `[[Target]]`           — bare title (matches by stem in any subdir)
//   `[[Folder/Target]]`    — explicit relative path under vault root
//   `[[Target|Alias]]`     — Obsidian alias form; we resolve `Target`
//                            and ignore the display alias.
// Targets containing `]]` or newlines are not supported and are left
// as plain text. Duplicate references in the same string resolve once
// and are deduplicated in the context block.

use std::collections::BTreeMap;

use crate::vault::{vault_read, vault_resolve_wikilink};

#[derive(Debug, Clone)]
pub struct ExpandResult {
    /// Input text with a context block appended (or unchanged when
    /// the input has no resolvable wikilinks).
    pub text: String,
    /// Resolved paths in the order first encountered. Useful for UI
    /// breadcrumbs ("included: Skills/Researcher.md").
    pub resolved: Vec<String>,
    /// Targets that didn't resolve to a file. Caller may surface
    /// these as warnings. The originating marker is left untouched in
    /// `text` so the user can fix the typo.
    pub unresolved: Vec<String>,
}

/// Expand `[[wikilink]]` markers found in `text`.
///
/// Behavior:
///   * No vault open → returns the text unchanged with empty vectors.
///   * No markers found → returns the text unchanged.
///   * Markers found and resolved → appends a `<context>` block. The
///     original markers stay in-place inside `text`, so the user's
///     sentence reads naturally and the model sees both the
///     reference and the full content.
pub fn expand(vault: &str, text: &str) -> ExpandResult {
    let mut resolved: Vec<String> = Vec::new();
    let mut unresolved: Vec<String> = Vec::new();
    // BTreeMap so the appended context block is stable (alphabetical
    // by resolved path) regardless of which order the user typed the
    // markers in.
    let mut bodies: BTreeMap<String, String> = BTreeMap::new();

    for raw in scan(text) {
        // Pipe alias: `[[Target|Display]]` — keep Target only.
        let target = raw.split('|').next().unwrap_or(&raw).trim();
        if target.is_empty() {
            continue;
        }
        match vault_resolve_wikilink(vault.to_string(), target.to_string(), false) {
            Ok(link) => {
                if bodies.contains_key(&link.path) {
                    continue;
                }
                match vault_read(vault.to_string(), link.path.clone()) {
                    Ok(body) => {
                        resolved.push(link.path.clone());
                        bodies.insert(link.path, body);
                    }
                    Err(_) => unresolved.push(raw),
                }
            }
            Err(_) => unresolved.push(raw),
        }
    }

    if bodies.is_empty() {
        return ExpandResult {
            text: text.to_string(),
            resolved,
            unresolved,
        };
    }

    // Trim the vault prefix off resolved paths for the displayed
    // breadcrumb so the model sees "Skills/Researcher.md" rather
    // than the absolute path (which is noisy and leaks the user's
    // home directory).
    let mut out =
        String::with_capacity(text.len() + bodies.values().map(|s| s.len()).sum::<usize>() + 256);
    out.push_str(text);
    out.push_str("\n\n<context>\n");
    for (path, body) in &bodies {
        let rel = path
            .strip_prefix(vault)
            .unwrap_or(path)
            .trim_start_matches('/');
        out.push_str(&format!("## {rel}\n\n"));
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    out.push_str("</context>\n");

    ExpandResult {
        text: out,
        resolved,
        unresolved,
    }
}

/// Scan `text` for `[[...]]` markers and return the raw inner content
/// of each match (e.g. `Folder/Note|Alias`). Skips matches containing
/// a newline or an unbalanced bracket. Order preserved.
fn scan(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            // Find the matching `]]` on the same line.
            if let Some(end_rel) = find_close(&bytes[i + 2..]) {
                let inner_start = i + 2;
                let inner_end = inner_start + end_rel;
                let inner = &text[inner_start..inner_end];
                if !inner.is_empty() {
                    out.push(inner.to_string());
                }
                i = inner_end + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn find_close(after_open: &[u8]) -> Option<usize> {
    let mut j = 0usize;
    while j + 1 < after_open.len() {
        let b = after_open[j];
        if b == b'\n' {
            return None;
        }
        if b == b']' && after_open[j + 1] == b']' {
            return Some(j);
        }
        j += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn touch(dir: &std::path::Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, body).unwrap();
    }

    #[test]
    fn scan_picks_up_basic_markers() {
        let got = scan("hello [[Foo]] world [[Bar/Baz]] end");
        assert_eq!(got, vec!["Foo", "Bar/Baz"]);
    }

    #[test]
    fn scan_ignores_unclosed_and_newline_spanning() {
        assert!(scan("[[unclosed").is_empty());
        assert!(scan("[[has\nnewline]]").is_empty());
    }

    #[test]
    fn scan_empty_brackets_skipped() {
        assert!(scan("[[]] noise").is_empty());
    }

    #[test]
    fn expand_no_markers_passthrough() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path().to_string_lossy().to_string();
        let r = expand(&vault, "just a normal sentence.");
        assert_eq!(r.text, "just a normal sentence.");
        assert!(r.resolved.is_empty());
        assert!(r.unresolved.is_empty());
    }

    #[test]
    fn expand_resolves_and_appends_context_block() {
        let dir = TempDir::new().unwrap();
        touch(
            dir.path(),
            "Skills/Researcher.md",
            "A researcher investigates.",
        );
        let vault = dir.path().to_string_lossy().to_string();
        let r = expand(&vault, "tell me about a [[Researcher]].");
        assert!(r.text.contains("tell me about a [[Researcher]]."));
        assert!(r.text.contains("<context>"));
        assert!(r.text.contains("## Skills/Researcher.md"));
        assert!(r.text.contains("A researcher investigates."));
        assert_eq!(r.resolved.len(), 1);
        assert!(r.unresolved.is_empty());
    }

    #[test]
    fn expand_pipe_alias_uses_left_side_only() {
        let dir = TempDir::new().unwrap();
        touch(dir.path(), "Researcher.md", "body");
        let vault = dir.path().to_string_lossy().to_string();
        let r = expand(&vault, "[[Researcher|the researcher]]");
        assert_eq!(r.resolved.len(), 1);
        assert!(r.text.contains("body"));
    }

    #[test]
    fn expand_unresolved_marker_left_alone_and_reported() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path().to_string_lossy().to_string();
        let r = expand(&vault, "see [[NopeNotHere]] today");
        assert_eq!(r.text, "see [[NopeNotHere]] today");
        assert_eq!(r.unresolved, vec!["NopeNotHere"]);
    }

    #[test]
    fn expand_dedupes_repeated_marker() {
        let dir = TempDir::new().unwrap();
        touch(dir.path(), "X.md", "the X");
        let vault = dir.path().to_string_lossy().to_string();
        let r = expand(&vault, "[[X]] and again [[X]]");
        assert_eq!(r.resolved.len(), 1);
        // Body should appear once, not twice.
        assert_eq!(r.text.matches("the X").count(), 1);
    }
}
