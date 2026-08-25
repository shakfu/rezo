// Rustyline-based input editor.
//
// Wraps rustyline's `Editor` with a `Helper` that provides:
//   * Tab completion for slash commands and (for path-taking
//     commands) filesystem paths.
//   * Highlighting that renders the user's input bold bright white.
//
// History is persisted to `<config_dir>/history.txt`. The editor is
// owned by the REPL and moved in/out of `tokio::task::spawn_blocking`
// for each read so the async runtime never blocks on stdin.

use std::borrow::Cow;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use rustyline::completion::{Completer, FilenameCompleter, Pair};
use rustyline::config::{CompletionType, Config, EditMode};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::FileHistory;
use rustyline::validate::Validator;
use rustyline::{Context as RlContext, Editor, Helper};

/// Slash-command verbs known to the REPL. Mirrors the dispatch table
/// in `repl::handle_command`. Keep alphabetised for predictable
/// completion ordering.
pub const COMMANDS: &[&str] = &[
    "agent",
    "c",
    "chat",
    "clear",
    "conv",
    "del",
    "delete",
    "embed",
    "exit",
    "export",
    "fork",
    "find",
    "h",
    "help",
    "history",
    "import",
    "load",
    "max-steps",
    "model",
    "models",
    "new",
    "next",
    "note",
    "prev",
    "provider",
    "q",
    "quit",
    "rename",
    "search",
    "redo",
    "setup",
    "undo",
    "system",
    "thinking",
    "tools",
    "vault",
];

/// Verbs whose first argument is a filesystem path. Tab after the
/// verb + space delegates to `FilenameCompleter`.
const PATH_COMMANDS: &[&str] = &["load", "embed", "vault", "note", "export", "import"];

pub struct ReplHelper {
    filename: FilenameCompleter,
}

impl ReplHelper {
    pub fn new() -> Self {
        Self {
            filename: FilenameCompleter::new(),
        }
    }
}

impl Completer for ReplHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        ctx: &RlContext<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let Some(after_slash) = line.strip_prefix('/') else {
            return Ok((pos, Vec::new()));
        };
        // `pos` is relative to `line`; shift into the post-slash space.
        let after_pos = pos.saturating_sub(1);
        let verb_end = after_slash
            .find(char::is_whitespace)
            .unwrap_or(after_slash.len());
        if after_pos <= verb_end {
            // Caret is within the verb — complete command names.
            let prefix = &after_slash[..after_pos];
            let matches: Vec<Pair> = COMMANDS
                .iter()
                .filter(|c| c.starts_with(prefix))
                .map(|c| Pair {
                    display: format!("/{c}"),
                    replacement: c.to_string(),
                })
                .collect();
            // Start replacement at byte 1 — the `/` stays put.
            return Ok((1, matches));
        }
        let verb = &after_slash[..verb_end];
        if PATH_COMMANDS.contains(&verb) {
            return self.filename.complete(line, pos, ctx);
        }
        Ok((pos, Vec::new()))
    }
}

// Prompt styling via `highlight_prompt`: rustyline measures width
// from the plain prompt we pass into `readline`, then renders the
// styled version returned here. We deliberately leave the SGR open
// (no trailing `\x1b[0m`) so the terminal stays in bold-white while
// rustyline draws the input buffer — typed characters inherit the
// style without a separate Highlighter::highlight impl (which the
// crate's layout calc doesn't fully discount for ANSI bytes,
// drifting the cursor). `read_line` emits the reset after `readline`
// returns so subsequent assistant output isn't tinted.
impl Highlighter for ReplHelper {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        Cow::Owned(format!("\x1b[1;97m{prompt}"))
    }
}

impl Hinter for ReplHelper {
    type Hint = String;
}

impl Validator for ReplHelper {}

impl Helper for ReplHelper {}

pub type ReplEditor = Editor<ReplHelper, FileHistory>;

pub fn history_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("com", "rezon", "rezon-tui")
        .context("could not resolve user config dir")?;
    Ok(dirs.config_dir().join("history.txt"))
}

pub fn build_editor(history: &PathBuf) -> Result<ReplEditor> {
    let cfg = Config::builder()
        .completion_type(CompletionType::List)
        .edit_mode(EditMode::Emacs)
        .auto_add_history(true)
        .build();
    let mut editor: ReplEditor = Editor::with_config(cfg).context("rustyline editor")?;
    editor.set_helper(Some(ReplHelper::new()));
    if let Some(parent) = history.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let _ = editor.load_history(history);
    Ok(editor)
}

/// Read a line via `editor.readline`. The editor is moved into a
/// blocking task and returned afterwards so the async runtime is
/// never stalled.
pub async fn read_line(slot: &mut Option<ReplEditor>, prompt: String) -> ReadOutcome {
    let editor = match slot.take() {
        Some(e) => e,
        None => return ReadOutcome::Eof,
    };
    let join = tokio::task::spawn_blocking(move || {
        let mut e = editor;
        let r = e.readline(&prompt);
        (e, r)
    })
    .await;
    let (editor, result) = match join {
        Ok(pair) => pair,
        Err(_) => return ReadOutcome::Eof,
    };
    *slot = Some(editor);
    // Restore default SGR so the assistant response isn't tinted by
    // the bold-white state the prompt left open. Written directly
    // via `anstream::stdout()` so it's stripped on non-tty.
    let mut out = anstream::stdout().lock();
    let _ = out.write_all(b"\x1b[0m");
    let _ = out.flush();
    match result {
        Ok(line) => ReadOutcome::Line(line),
        // Ctrl-C aborts the current line buffer; loop continues with a
        // fresh prompt (matches bash / python REPL convention).
        Err(ReadlineError::Interrupted) => ReadOutcome::Interrupted,
        // Ctrl-D on an empty line — exit.
        Err(ReadlineError::Eof) => ReadOutcome::Eof,
        Err(_) => ReadOutcome::Eof,
    }
}

pub enum ReadOutcome {
    Line(String),
    Interrupted,
    Eof,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustyline::history::DefaultHistory;
    use rustyline::{history::History, Context as RlContext};

    /// Spin up a rustyline `Context` over an empty history so the
    /// completer can be called in isolation. The history reference
    /// has to outlive the context, hence the inner closure pattern.
    fn with_ctx<F: FnOnce(&RlContext<'_>)>(f: F) {
        let history = DefaultHistory::new();
        let ctx = RlContext::new(&history);
        f(&ctx);
        // Touch the history so the unused-`History`-import warning
        // doesn't fire if/when rustyline ever stops requiring the
        // trait import.
        let _ = History::len(&history);
    }

    /// Convenience: run the helper's `complete` against a fixed
    /// line + caret position, returning the matched replacements
    /// only (rustyline emits `Pair { display, replacement }`).
    fn complete(line: &str, pos: usize) -> Vec<String> {
        let helper = ReplHelper::new();
        let history = DefaultHistory::new();
        let ctx = RlContext::new(&history);
        let (_, pairs) = helper.complete(line, pos, &ctx).expect("complete ok");
        pairs.into_iter().map(|p| p.replacement).collect()
    }

    #[test]
    fn empty_line_yields_no_completions() {
        let results = complete("", 0);
        assert!(results.is_empty(), "got: {results:?}");
    }

    #[test]
    fn slash_alone_yields_every_command() {
        let results = complete("/", 1);
        // The full command list is fairly long; spot-check a few
        // commands that should appear, and that the count matches
        // the const.
        for must_have in ["help", "exit", "conv", "model", "vault", "search"] {
            assert!(
                results.iter().any(|r| r == must_have),
                "expected `{must_have}` in {results:?}"
            );
        }
        assert_eq!(results.len(), COMMANDS.len(), "result count mismatch");
    }

    #[test]
    fn prefix_filters_to_matching_commands() {
        let results = complete("/h", 2);
        // `h`, `help`, `history` all start with "h".
        for expected in ["h", "help", "history"] {
            assert!(
                results.iter().any(|r| r == expected),
                "expected `{expected}` in {results:?}"
            );
        }
        // Nothing that doesn't start with "h" should be present.
        for forbidden in ["exit", "model"] {
            assert!(
                !results.iter().any(|r| r == forbidden),
                "did not expect `{forbidden}` in {results:?}"
            );
        }
    }

    #[test]
    fn caret_inside_verb_completes_only_the_verb() {
        // Caret at position 3 of "/he" — should match anything
        // starting with `he`.
        let results = complete("/he", 3);
        assert!(results.iter().any(|r| r == "help"));
        assert!(results.iter().all(|r| r.starts_with("he")));
    }

    #[test]
    fn caret_past_verb_for_non_path_command_yields_nothing() {
        // `/exit ` — caret after the space, but `exit` isn't a
        // path-taking verb, so nothing comes back.
        let results = complete("/exit foo", 9);
        assert!(results.is_empty(), "got: {results:?}");
    }

    #[test]
    fn path_command_delegates_to_filename_completer_after_verb() {
        // Path-taking verbs (load / embed / vault / note / export
        // / import) should produce *something* once we're past the
        // space — even with no real prefix, FilenameCompleter
        // enumerates the current directory entries.
        //
        // Asserting the exact list would be brittle (depends on
        // CWD), so we just confirm the completer returns Ok and
        // the start offset moves past the verb portion.
        let helper = ReplHelper::new();
        let history = DefaultHistory::new();
        let ctx = RlContext::new(&history);
        let line = "/load ";
        let (start, _pairs) = helper
            .complete(line, line.len(), &ctx)
            .expect("filename completer ok");
        // FilenameCompleter returns the byte offset where the
        // replacement should start — must be at or past the verb +
        // space (`/load ` is 6 bytes).
        assert!(start >= 6, "got start={start}");
    }

    #[test]
    fn non_slash_input_yields_nothing() {
        // Anything that doesn't start with `/` is just chat input;
        // we deliberately don't complete it (the model takes the
        // raw string).
        let results = complete("hello world", 5);
        assert!(results.is_empty(), "got: {results:?}");
    }

    #[test]
    fn unused_with_ctx_helper_still_works() {
        // Sanity check the `with_ctx` helper — used as a template
        // for future tests that want to share context plumbing.
        with_ctx(|_| {
            // ctx is alive here.
        });
    }
}
