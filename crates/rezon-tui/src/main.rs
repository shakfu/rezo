// rezon-tui: terminal chat shell over `rezon-core`.
//
// Sequential REPL — no ratatui, no alternate screen, no modes.
// Output streams to stdout as it arrives; terminal scrollback keeps
// the conversation history. Slash commands handle navigation and
// configuration; `/help` lists them.

mod agent;
mod conv_index;
mod input;
mod markdown;
mod picker;
mod repl;
mod setup;
mod sink;
mod spinner;
mod store;
mod vault;

use anyhow::Result;
use clap::Parser;
use rezon_core::llm::{ChatOpts, LlmState};

use crate::repl::Repl;

#[derive(Debug, Parser)]
#[command(
    name = "rezon-tui",
    about = "Sequential chat REPL over rezon-core (local llama.cpp or OpenAI-compatible cloud providers)"
)]
struct Cli {
    /// Provider key: "local", "openai", "anthropic", "openrouter", or "other".
    #[arg(long, default_value = "openrouter")]
    provider: String,

    /// Model identifier. For cloud providers this is the model slug
    /// (e.g. "anthropic/claude-sonnet-4"); for `--provider local` it's
    /// the path to a GGUF file.
    #[arg(long)]
    model: Option<String>,

    /// Path to a GGUF file. Alias for `--model` when
    /// `--provider local` is in use; ignored otherwise.
    #[arg(long)]
    gguf: Option<String>,

    /// Base URL for `--provider other` (e.g. http://localhost:11434/v1
    /// for Ollama).
    #[arg(long)]
    base_url: Option<String>,

    /// Override the provider's API-key env var.
    #[arg(long)]
    api_key: Option<String>,

    /// System prompt prepended to new conversations.
    #[arg(long)]
    system: Option<String>,

    /// Run the multi-step agent loop (tool use). Toggle in-session
    /// with `/agent` and `/chat`.
    #[arg(long)]
    agent: bool,

    /// Hard cap on agent loop iterations. Override in-session with
    /// `/max-steps <n>`.
    #[arg(long, default_value_t = 8)]
    max_steps: usize,

    /// Show agent reasoning ("thinking") blocks as they stream.
    /// Toggle per-conversation with `/thinking on|off|toggle`.
    #[arg(long)]
    show_thinking: bool,
}

fn main() -> Result<()> {
    // Before `Cli::parse`, so `--help` still works if a `.env` is
    // malformed, and before anything resolves a key.
    //
    // Report what was loaded. `dotenvy` walks *up* from the current
    // directory, so a `.env` in a parent — commonly `~/.env` — supplies
    // keys to a run started anywhere beneath it. That is useful and
    // also easy to forget, and "why does it have a key?" is a
    // genuinely confusing question to debug in silence.
    let loaded = rezon_core::dotenv::load();
    for p in &loaded.paths {
        eprintln!("loaded env from {}", p.display());
    }
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    // sqlite-vec needs to be registered before any rusqlite
    // Connection::open call; do it once at process start.
    rezon_core::search::register_sqlite_vec();

    let state = LlmState::default();

    // Load + wizard *before* the local-load check so the wizard can
    // pick the provider for fresh installs (otherwise we'd error on
    // `--provider local` without `--gguf` before the user even sees
    // the wizard).
    let default_system = cli.system.clone().unwrap_or_default();
    let (mut store, _existed) = store::Store::load_or_new(&default_system)?;
    if let Err(e) = setup::maybe_run(&mut store) {
        eprintln!("setup wizard: {e}");
    }

    // Every launch starts in a fresh conversation. Prior conversations
    // stay in the store (and are reachable via `/conv`) but are not
    // pulled into the context window — automatic history reload was
    // surprising and could blow past `n_batch` on the first turn. We
    // skip the create when the existing active conv is already empty
    // so the user doesn't accumulate one blank conv per launch.
    let active_has_user_turns = store.active().messages.iter().any(|m| m.role == "user");
    if active_has_user_turns {
        store.new_conversation(&default_system);
        // Persist so a crash before the first turn still leaves the
        // store pointing at the fresh conv (avoids re-resuming the
        // previous one on next launch).
        if let Err(e) = store.save() {
            eprintln!("save store: {e}");
        }
    }

    // CLI `--provider` wins over the stored default *only* if the
    // user actually passed it. clap doesn't distinguish "user typed
    // the default value" from "clap filled in the default", so we
    // approximate: when the value equals the CLI default and the
    // store has its own preference, the store wins. Anyone passing
    // a non-default `--provider` overrides for the session.
    const CLI_PROVIDER_DEFAULT: &str = "openrouter";
    let provider = if cli.provider == CLI_PROVIDER_DEFAULT {
        store
            .default_provider
            .clone()
            .unwrap_or_else(|| cli.provider.clone())
    } else {
        cli.provider.clone()
    };

    // Resolve a local GGUF to auto-load. Priority:
    //   1. `--gguf <path>` (explicit CLI flag)
    //   2. `--model <path>` when it points at a .gguf file
    //   3. `<config_dir>/last_model.txt`, written every time we
    //      successfully load a local model — survives across launches
    //      so resuming "the model I was just using" is one-shot.
    // No source → skip auto-load and let the user pick via `/model`.
    let last_model_path = store::config_dir()
        .ok()
        .and_then(|d| rezon_core::llm::read_last_model(&d));
    let local_path: Option<String> = if provider == "local" {
        cli.gguf
            .clone()
            .or_else(|| cli.model.clone().filter(|p| looks_like_gguf(p)))
            .or_else(|| last_model_path.clone().filter(|p| looks_like_gguf(p)))
    } else {
        None
    };
    if let Some(path) = local_path.as_ref() {
        let label = format!(
            "loading {}",
            std::path::Path::new(path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(path)
        );
        match spinner::with_spinner(label, state.load(path.clone())).await {
            Ok(_) => {
                if let Ok(d) = store::config_dir() {
                    rezon_core::llm::persist_last_model(&d, path);
                }
            }
            Err(e) => {
                // Don't abort — the REPL is still useful (user can
                // /load or switch provider). Surface the error and
                // continue.
                eprintln!("load model: {e}");
            }
        }
    } else if provider == "local" {
        eprintln!("note: provider is local but no model loaded — use /model to pick one");
    }

    let chat_opts = ChatOpts {
        provider,
        model: cli.model.clone(),
        base_url: cli.base_url.clone(),
        api_key: cli.api_key.clone(),
        ..Default::default()
    };

    // Open + rebuild the FTS index from the loaded store. Failure is
    // non-fatal: `/search` will degrade to "no matches" but the rest
    // of the REPL keeps working.
    let conv_index = match store.path.parent().map(|p| p.join("conversations.db")) {
        Some(path) => match conv_index::ConvIndex::open(&path) {
            Ok(idx) => {
                if let Err(e) = idx.rebuild_from(&store) {
                    eprintln!("conv index rebuild: {e}");
                }
                Some(idx)
            }
            Err(e) => {
                eprintln!("conv index open: {e}");
                None
            }
        },
        None => None,
    };

    let vault = vault::VaultCtx::new()?;
    let mut repl = Repl::new(
        state,
        chat_opts,
        store,
        default_system,
        cli.agent,
        cli.max_steps,
        vault,
        cli.show_thinking,
        conv_index,
    );
    repl.run().await
}

fn looks_like_gguf(p: &str) -> bool {
    std::path::Path::new(p)
        .extension()
        .and_then(|s| s.to_str())
        .map(|e| e.eq_ignore_ascii_case("gguf"))
        .unwrap_or(false)
}
