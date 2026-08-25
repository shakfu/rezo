// First-launch setup wizard. Prompts for paths and a default
// provider, persisting them into the store so subsequent launches
// boot straight to the prompt. Re-runnable via `/setup`.
//
// Each prompt opens a rustyline session pre-filled with the default
// (env-var / config-dir / current store value), so:
//   * <enter> accepts the visible value as-is
//   * editing in place tweaks the path with tab-completion
//   * backspace-to-empty + <enter> clears the field
//   * ctrl-c / ctrl-d aborts the wizard (any prior answers stay in
//     memory but are not persisted — `setup_complete` stays false).
//
// Path prompts wire in a `FilenameCompleter`; the provider prompt
// completes from a fixed word list.

use std::borrow::Cow;
use std::path::PathBuf;

use anyhow::Result;
use rustyline::completion::{Completer, FilenameCompleter, Pair};
use rustyline::config::{CompletionType, Config, EditMode};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{Context as RlContext, Editor, Helper};

use crate::store::{config_dir, Store};

const C_APP: &str = "\x1b[36m"; // cyan
const C_META: &str = "\x1b[90m"; // dim grey
const C_OK: &str = "\x1b[32m"; // green
const C_RESET: &str = "\x1b[0m";

const PROVIDER_CHOICES: &[&str] = &["local", "openai", "anthropic", "openrouter", "other"];

/// Run the wizard on first launch only (`setup_complete == false`).
pub fn maybe_run(store: &mut Store) -> Result<()> {
    if store.setup_complete {
        return Ok(());
    }
    run(store, /*forced=*/ false)
}

/// Force-run the wizard from `/setup`. Pre-fills each prompt with
/// the *current* store value rather than the env-var / config-dir
/// defaults, so the user can tweak one field without retyping the
/// rest.
pub fn run_forced(store: &mut Store) -> Result<()> {
    run(store, /*forced=*/ true)
}

fn run(store: &mut Store, forced: bool) -> Result<()> {
    let cfg = config_dir().ok();
    // All path defaults live under one familiar tree. `cfg` is still
    // resolved (used by tests and as a last-resort fallback if HOME
    // is unset) but the visible defaults route through `~/Documents/
    // Rezon/...` so the user can find every artefact in one place.
    let rezon_root =
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Documents").join("Rezon"));
    let under_root = |sub: &str| -> Option<String> {
        rezon_root
            .as_ref()
            .map(|p| p.join(sub).to_string_lossy().into_owned())
            .or_else(|| {
                cfg.as_ref()
                    .map(|d| d.join(sub).to_string_lossy().into_owned())
            })
    };

    println!();
    println!("{C_APP}rezon setup{C_RESET}");
    println!(
        "{C_META}<enter> accepts shown value · edit then <enter> to change · empty + <enter> clears · ctrl-c aborts{C_RESET}",
    );
    println!();

    let mut path_editor = build_editor(WizardHelper::path())?;
    let mut word_editor = build_editor(WizardHelper::words(PROVIDER_CHOICES))?;

    // --- models dir ---
    let models_default = if forced {
        store.models_dir.clone()
    } else {
        std::env::var("REZON_MODELS_DIR")
            .ok()
            .or_else(|| under_root("models"))
    };
    match prompt(
        &mut path_editor,
        "models dir (local *.gguf)",
        models_default.as_deref(),
    )? {
        PromptOutcome::Value(v) => store.models_dir = v,
        PromptOutcome::Abort => return Ok(()),
    }

    // --- vault dir ---
    let vault_default = if forced {
        store.active_vault.clone()
    } else {
        std::env::var("REZON_VAULT_DIR")
            .ok()
            .or_else(|| store.active_vault.clone())
            .or_else(|| under_root("vault"))
    };
    match prompt(
        &mut path_editor,
        "vault dir (notes)",
        vault_default.as_deref(),
    )? {
        PromptOutcome::Value(v) => store.active_vault = v,
        PromptOutcome::Abort => return Ok(()),
    }

    // --- output dir ---
    let output_default = if forced {
        store.output_dir.clone()
    } else {
        std::env::var("REZON_OUTPUT_DIR")
            .ok()
            .or_else(|| under_root("exports"))
    };
    match prompt(
        &mut path_editor,
        "output dir (/export default)",
        output_default.as_deref(),
    )? {
        PromptOutcome::Value(v) => store.output_dir = v,
        PromptOutcome::Abort => return Ok(()),
    }

    // --- default provider ---
    let provider_default = if forced {
        store.default_provider.clone()
    } else {
        std::env::var("REZON_PROVIDER")
            .ok()
            .or_else(|| store.default_provider.clone())
            .or_else(|| Some("openrouter".to_string()))
    };
    match prompt(
        &mut word_editor,
        "default provider (local / openai / anthropic / openrouter / other)",
        provider_default.as_deref(),
    )? {
        PromptOutcome::Value(v) => store.default_provider = v,
        PromptOutcome::Abort => return Ok(()),
    }

    store.setup_complete = true;
    store.save()?;

    println!();
    println!(
        "{C_OK}setup complete{C_RESET} {C_META}(stored in {}){C_RESET}",
        store.path.display(),
    );
    println!();
    Ok(())
}

enum PromptOutcome {
    /// Inner `Option<String>` is the stored value (None == cleared).
    Value(Option<String>),
    /// Ctrl-C / Ctrl-D — exit wizard without persisting.
    Abort,
}

fn prompt(
    editor: &mut Editor<WizardHelper, DefaultHistory>,
    label: &str,
    default: Option<&str>,
) -> Result<PromptOutcome> {
    let prompt_str = format!("  {C_APP}{label}{C_RESET}{C_META}>{C_RESET} ");
    let initial = default.unwrap_or("");
    let line = match editor.readline_with_initial(&prompt_str, (initial, "")) {
        Ok(s) => s,
        Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
            return Ok(PromptOutcome::Abort);
        }
        Err(e) => return Err(e.into()),
    };
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(PromptOutcome::Value(None));
    }
    Ok(PromptOutcome::Value(Some(expand_tilde(trimmed))))
}

fn build_editor(helper: WizardHelper) -> Result<Editor<WizardHelper, DefaultHistory>> {
    let cfg = Config::builder()
        .completion_type(CompletionType::List)
        .edit_mode(EditMode::Emacs)
        .auto_add_history(false)
        .build();
    let mut editor: Editor<WizardHelper, DefaultHistory> = Editor::with_config(cfg)?;
    editor.set_helper(Some(helper));
    Ok(editor)
}

fn expand_tilde(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    s.to_string()
}

/// rustyline helper that does one of two things depending on its
/// mode: filename completion (for path prompts) or static-word
/// completion (for the provider prompt). Kept as a single enum
/// variant carrier so we don't need two Helper impls.
struct WizardHelper {
    mode: Mode,
}

enum Mode {
    Path(FilenameCompleter),
    Words(&'static [&'static str]),
}

impl WizardHelper {
    fn path() -> Self {
        Self {
            mode: Mode::Path(FilenameCompleter::new()),
        }
    }
    fn words(list: &'static [&'static str]) -> Self {
        Self {
            mode: Mode::Words(list),
        }
    }
}

impl Completer for WizardHelper {
    type Candidate = Pair;
    fn complete(
        &self,
        line: &str,
        pos: usize,
        ctx: &RlContext<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        match &self.mode {
            Mode::Path(fc) => fc.complete(line, pos, ctx),
            Mode::Words(list) => {
                let prefix = &line[..pos];
                let matches: Vec<Pair> = list
                    .iter()
                    .filter(|c| c.starts_with(prefix))
                    .map(|c| Pair {
                        display: (*c).to_string(),
                        replacement: (*c).to_string(),
                    })
                    .collect();
                Ok((0, matches))
            }
        }
    }
}

impl Highlighter for WizardHelper {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        Cow::Borrowed(prompt)
    }
}

impl Hinter for WizardHelper {
    type Hint = String;
}

impl Validator for WizardHelper {}
impl Helper for WizardHelper {}
