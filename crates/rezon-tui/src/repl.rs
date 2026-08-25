// Sequential REPL loop.
//
// The shape mirrors classic interactive llm clients (cyllama, ollama
// run): print a prompt, read a line of stdin, either treat it as a
// `/command` or send it as a chat turn. Tokens stream straight to
// stdout; the terminal handles scrollback. The previous turn stays on
// screen indefinitely (until the user scrolls past).
//
// Streaming concurrency: the chat / agent task spawns onto the tokio
// runtime and emits events through `sink::UiEvent` over an mpsc. The
// REPL's `wait_for_turn` drains the channel synchronously, printing
// tokens / tool pills / stats until it sees `Done` or `Error`.
// Ctrl-C cancels the in-flight run by flipping the LlmState /
// AgentRunHandle cancel flag; a second Ctrl-C exits.

use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::Arc;

use anstream::{eprintln, print, println};
use anstyle::{AnsiColor, Color, Reset, Style};
use anyhow::Result;
use rezon_core::llm::{self, ChatMsg, ChatOpts, ChatSink, LlmState};
use tokio::signal::ctrl_c;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::agent::{spawn_agent_run, AgentRunHandle};
use crate::conv_index::ConvIndex;
use crate::input::{self, ReadOutcome, ReplEditor};
use crate::sink::{StatsLite, TuiChatSink, UiEvent};
use crate::store::Store;
use crate::vault::VaultCtx;

// Style palette. Values are `anstyle::Style` / `anstyle::Reset`; both
// types implement `Display` so the existing `{x}...{reset}` format
// strings keep working. Output is routed through `anstream`'s macros,
// which strip SGR codes when stdout isn't a tty (e.g. piped) and
// translate them to Win32 console calls on legacy Windows consoles.
//
// Semantic palette (mirroring the cyllama reference look):
//   cyan    — app name, slash-command names highlighted inline
//   white   — default fg; user input shown bold + bright
//   magenta — per-turn token stats line, tool pills
//   green   — tool-call success marker
//   red     — error marker
//   bright black ("META") — secondary status lines
const fn fg(c: AnsiColor) -> Style {
    Style::new().fg_color(Some(Color::Ansi(c)))
}

const C_RESET: Reset = Reset;
const C_BOLD: Style = Style::new().bold();
const C_DIM: Style = Style::new().dimmed();
const C_USER: Style = fg(AnsiColor::BrightWhite).bold();
const C_APP: Style = fg(AnsiColor::Cyan);
const C_TOOL: Style = fg(AnsiColor::Magenta);
const C_OK: Style = fg(AnsiColor::Green);
const C_ERR: Style = fg(AnsiColor::Red);
const C_META: Style = fg(AnsiColor::BrightBlack);

pub struct Repl {
    state: Arc<LlmState>,
    /// Provider/model/api-key/base-url defaults from the CLI. Each
    /// conversation can override individual fields via
    /// `Conversation::settings`; effective values are composed via
    /// `effective_chat_opts()`.
    cli_chat_opts: ChatOpts,
    default_system: String,
    /// Agent mode default from `--agent` at launch. Conversations
    /// override via `Conversation::settings.agent_mode`.
    cli_agent_mode: bool,
    /// Reasoning-visibility default from `--show-thinking` at launch.
    /// Conversations override via `Conversation::settings.show_thinking`.
    cli_show_thinking: bool,
    max_steps: usize,
    store: Store,
    vault: VaultCtx,
    /// FTS5 index over conversation messages. `None` when the
    /// underlying SQLite file couldn't be opened; in that case
    /// `/search` reports "no matches" rather than crashing.
    conv_index: Option<ConvIndex>,
    ui_tx: UnboundedSender<UiEvent>,
    ui_rx: UnboundedReceiver<UiEvent>,
    /// Active agent handle while an agent run is in flight (used for
    /// Ctrl-C cancel). None for chat runs — chat uses LlmState::cancel.
    agent_handle: Option<AgentRunHandle>,
}

impl Repl {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        state: LlmState,
        chat_opts: ChatOpts,
        store: Store,
        default_system: String,
        agent_mode: bool,
        max_steps: usize,
        vault: VaultCtx,
        show_thinking: bool,
        conv_index: Option<ConvIndex>,
    ) -> Self {
        let (ui_tx, ui_rx) = unbounded_channel();
        Self {
            state: Arc::new(state),
            cli_chat_opts: chat_opts,
            default_system,
            cli_agent_mode: agent_mode,
            cli_show_thinking: show_thinking,
            max_steps,
            store,
            vault,
            conv_index,
            ui_tx,
            ui_rx,
            agent_handle: None,
        }
    }

    /// Re-index the currently active conversation in the FTS index.
    /// Called after any in-place mutation of the message vector.
    fn reindex_active(&self) {
        if let Some(idx) = self.conv_index.as_ref() {
            if let Err(e) = idx.replace_conv(self.store.active()) {
                eprintln!("conv index replace: {e}");
            }
        }
    }

    fn deindex_conv(&self, conv_id: &str) {
        if let Some(idx) = self.conv_index.as_ref() {
            if let Err(e) = idx.delete_conv(conv_id) {
                eprintln!("conv index delete: {e}");
            }
        }
    }

    /// Compose ChatOpts from the active conversation's overrides on
    /// top of CLI defaults.
    fn effective_chat_opts(&self) -> ChatOpts {
        let s = &self.store.active().settings;
        let cli = &self.cli_chat_opts;
        ChatOpts {
            provider: s.provider.clone().unwrap_or_else(|| cli.provider.clone()),
            model: s.model.clone().or_else(|| cli.model.clone()),
            base_url: s.base_url.clone().or_else(|| cli.base_url.clone()),
            api_key: s.api_key.clone().or_else(|| cli.api_key.clone()),
            ..Default::default()
        }
    }

    fn effective_agent_mode(&self) -> bool {
        self.store
            .active()
            .settings
            .agent_mode
            .unwrap_or(self.cli_agent_mode)
    }

    fn effective_show_thinking(&self) -> bool {
        self.store
            .active()
            .settings
            .show_thinking
            .unwrap_or(self.cli_show_thinking)
    }

    pub async fn run(&mut self) -> Result<()> {
        // Wipe whatever startup noise reached the terminal (model
        // load progress, any pre-init stderr that beat `void_logs`)
        // so the chat starts from a clean top-of-screen. ANSI: erase
        // screen + scrollback, home cursor. Gated on tty so piped
        // output isn't polluted.
        if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
            print!("\x1b[2J\x1b[3J\x1b[H");
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
        self.print_banner();
        // Auto-open the most recently used vault on launch.
        if let Some(path) = self.store.active_vault.clone() {
            if let Err(e) = self.vault.open(&path) {
                eprintln!(
                    "{meta}auto-open vault failed: {e}{reset}",
                    meta = C_META,
                    reset = C_RESET
                );
                self.store.active_vault = None;
            } else {
                println!("{meta}vault: {path}{reset}", meta = C_META, reset = C_RESET);
                println!();
            }
        }

        // Set up rustyline editor with persistent history.
        let history_path = input::history_path()?;
        let mut editor: Option<ReplEditor> = Some(input::build_editor(&history_path)?);
        // Pass a plain prompt; `ReplHelper::highlight_prompt` adds
        // the bold-white styling at render time without inflating
        // the width rustyline uses for cursor positioning.
        let prompt = String::from("> ");

        loop {
            let line = match input::read_line(&mut editor, prompt.clone()).await {
                ReadOutcome::Line(s) => s,
                ReadOutcome::Interrupted => continue, // Ctrl-C: new prompt
                ReadOutcome::Eof => break,            // Ctrl-D: exit
            };
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix('/') {
                // Special-case `/clear` here so we have access to the
                // local rustyline editor; calling its `clear_screen`
                // resets the layout state the editor uses to redraw
                // the next prompt cleanly.
                if rest.trim() == "clear" {
                    if let Some(e) = editor.as_mut() {
                        let _ = e.clear_screen();
                    }
                    if io::stdout().is_terminal() {
                        print!("\x1b[2J\x1b[H");
                        anstream::stdout().flush().ok();
                    }
                    continue;
                }
                match self.handle_command(rest).await {
                    CmdResult::Continue => {}
                    CmdResult::Exit => break,
                }
                continue;
            }
            self.run_turn(trimmed.to_string()).await?;
        }
        self.save_ignore_err();
        // Flush history. Errors are non-fatal: the user just loses
        // history for this session.
        if let Some(mut e) = editor {
            let _ = e.save_history(&history_path);
        }
        // Tear down worker threads (embed worker, search watchers,
        // local llama context) in the order documented by their
        // `shutdown` impls. Matters most for local llama: the metal
        // backend asserts that its KV-cache buffers are gone before
        // process-exit destructors run.
        self.vault.shutdown();
        self.state.shutdown();
        Ok(())
    }

    fn print_banner(&self) {
        // Clear the visible viewport but keep scrollback intact —
        // \x1b[2J wipes the screen, \x1b[H homes the cursor; we
        // deliberately don't use \x1b[3J (which would also clear
        // scrollback). Skip when stdout isn't a tty (piped runs).
        if io::stdout().is_terminal() {
            print!("\x1b[2J\x1b[H");
        }
        // App + version: app name in cyan, version in default fg.
        let app = "rezon";
        let version = format!(" v{} chat", env!("CARGO_PKG_VERSION"));
        let model = self.header_model();
        let left_visible_len = app.chars().count() + version.chars().count();
        let width = terminal_size::terminal_size()
            .map(|(terminal_size::Width(w), _)| w as usize)
            .unwrap_or(80);
        let pad = width.saturating_sub(left_visible_len + model.chars().count());
        // `rezon` is bold cyan in the banner; elsewhere `C_APP` stays
        // non-bold so inline command refs (/help, /exit, …) don't
        // shout.
        let banner_app = C_APP.bold();
        println!(
            "{banner_app}{app}{reset}{version}{pad}{model}",
            reset = C_RESET,
            pad = " ".repeat(pad.max(2)),
        );
        println!();
        // Help hint: highlight /help and /exit in cyan, the rest in
        // default fg. Mirrors cyllama's banner style.
        println!(
            "type {app}/help{reset} to list available commands, or {app}/exit{reset} to quit",
            app = C_APP,
            reset = C_RESET,
        );
        if self.effective_agent_mode() {
            println!(
                "agent mode is on (use {app}/chat{reset} to disable)",
                app = C_APP,
                reset = C_RESET
            );
        }
        println!();
    }

    fn header_model(&self) -> String {
        let opts = self.effective_chat_opts();
        if opts.provider == "local" {
            if let Some(path) = self.state.status().path {
                return basename(&path);
            }
            return "local".to_string();
        }
        match &opts.model {
            Some(m) => format!("{}/{}", opts.provider, m),
            None => opts.provider.clone(),
        }
    }

    // ---- Turn -----------------------------------------------------

    async fn run_turn(&mut self, user_input: String) -> Result<()> {
        let convo = self.store.active_mut();
        convo.messages.push(ChatMsg {
            role: "user".to_string(),
            content: user_input.clone(),
            ..ChatMsg::default()
        });
        convo.maybe_auto_title();

        if self.effective_agent_mode() {
            self.spawn_agent(user_input);
        } else {
            self.spawn_chat();
        }
        self.wait_for_turn().await;
        // Stamp the conversation as just-used so /conv ranks it
        // first. Re-index so /search picks up the new messages.
        self.store.active_mut().touch();
        self.reindex_active();
        self.save_ignore_err();
        Ok(())
    }

    fn spawn_chat(&self) {
        let state = self.state.clone();
        let opts = self.effective_chat_opts();
        let mut msgs = self.store.active().messages.clone();
        // Stored conversation keeps the raw `[[wikilink]]` markers
        // (for legibility in /history and to re-resolve on each
        // turn). Expansion happens here at the send boundary, and
        // only on the system + last user message so prompt caching
        // stays valid across turns.
        self.expand_send_msgs(&mut msgs);
        let sink: Arc<dyn ChatSink> = Arc::new(TuiChatSink::new(self.ui_tx.clone()));
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = llm::chat(&state, msgs, opts, sink).await {
                let _ = tx.send(UiEvent::Error(e));
            }
        });
    }

    fn spawn_agent(&mut self, user_input: String) {
        // Pull the just-pushed user message off so the agent path
        // can prepend it itself (build_agent_messages handles that).
        let mut history = self.store.active().messages.clone();
        if matches!(history.last(), Some(m) if m.role == "user") {
            history.pop();
        }
        // `search_notes` is only registered when a vault is open;
        // otherwise the tool wouldn't have anywhere to search.
        let vault_arg = self.vault.active_vault().is_some().then_some(&self.vault);
        // Expand wikilinks the same way the chat path does: the
        // history's system message and the current user input both
        // get a `<context>` block appended for resolved targets.
        // Past assistant/user turns pass through untouched.
        let user_input = self.expand_for_vault(&user_input);
        if let Some(sys) = history.first_mut() {
            if sys.role == "system" {
                sys.content = self.expand_for_vault(&sys.content);
            }
        }
        match spawn_agent_run(
            self.state.clone(),
            self.effective_chat_opts(),
            history,
            user_input,
            self.ui_tx.clone(),
            self.max_steps,
            vault_arg,
            &self.store.disabled_tools,
        ) {
            Ok(h) => self.agent_handle = Some(h),
            Err(e) => {
                let _ = self.ui_tx.send(UiEvent::Error(e.to_string()));
            }
        }
    }

    /// Apply wikilink expansion to a chat message vec destined for
    /// the LLM. Mutates the system message (if any, at index 0) and
    /// the most recent user message; everything else passes through
    /// so already-cached prefixes stay stable.
    fn expand_send_msgs(&self, msgs: &mut [llm::ChatMsg]) {
        // System slot — conventionally the first message when set.
        if let Some(first) = msgs.first_mut() {
            if first.role == "system" {
                first.content = self.expand_for_vault(&first.content);
            }
        }
        // Most recent user turn (walking from the end is cheaper
        // than scanning the whole vec, and the last user is
        // typically at or near the tail).
        for msg in msgs.iter_mut().rev() {
            if msg.role == "user" {
                msg.content = self.expand_for_vault(&msg.content);
                break;
            }
        }
    }

    fn expand_for_vault(&self, text: &str) -> String {
        let Some(vault) = self.vault.active_vault() else {
            return text.to_string();
        };
        let r = rezon_core::wikilink::expand(&vault, text);
        if !r.unresolved.is_empty() {
            // Non-fatal: report each unresolvable marker but still
            // send the message. The original `[[X]]` stays in place
            // so the user can see what didn't resolve.
            let list = r.unresolved.join(", ");
            let _ = self
                .ui_tx
                .send(UiEvent::Error(format!("wikilink unresolved: {list}")));
        }
        r.text
    }

    /// Drain events until Done / Error. Handles tool confirmation
    /// inline by pausing to read y/n from stdin.
    async fn wait_for_turn(&mut self) {
        let mut assistant = String::new();
        let mut stats: Option<StatsLite> = None;
        let mut interrupts = 0u8;
        let mut newline_pending = false;
        // Live-tps state: stream_start anchors the rolling rate,
        // gen_chars approximates token count (rate is computed
        // against `gen_chars / 4`, the same approximation chats use
        // when the provider doesn't return usage), last_tick
        // throttles title-bar writes to ~every 200ms.
        let mut stream_start: Option<std::time::Instant> = None;
        let mut last_tick: Option<std::time::Instant> = None;
        let mut gen_chars: usize = 0;
        // Capture the terminal width at the moment streaming begins
        // so the markdown re-render uses the same row-count math
        // that produced the visible output. If the user resizes the
        // window mid-stream, the already-drawn content keeps its
        // original wrap layout — re-reading the width at Done time
        // would over- or under-clear and leak stale rows.
        let mut stream_width: Option<u16> = None;
        loop {
            tokio::select! {
                ev = self.ui_rx.recv() => {
                    let Some(ev) = ev else { break; };
                    match ev {
                        UiEvent::Token(s) => {
                            // If a thinking block left dim+italic
                            // active, reset before the assistant
                            // content streams in.
                            print!("{C_RESET}{s}");
                            anstream::stdout().flush().ok();
                            assistant.push_str(&s);
                            gen_chars += s.chars().count();
                            if stream_start.is_none() {
                                stream_start = Some(std::time::Instant::now());
                                stream_width = terminal_size::terminal_size()
                                    .map(|(w, _)| w.0);
                            }
                            maybe_update_title(&stream_start, &mut last_tick, gen_chars);
                            newline_pending = !s.ends_with('\n');
                        }
                        UiEvent::Thinking(s) => {
                            if self.effective_show_thinking() {
                                // Dim + italic so reasoning is
                                // visually distinct from the
                                // final answer.
                                print!("{C_DIM}{}", s);
                                // We don't reset between deltas —
                                // each chunk inherits the style;
                                // we reset once a content Token
                                // arrives (a noop write of the
                                // reset escape).
                                anstream::stdout().flush().ok();
                                newline_pending = !s.ends_with('\n');
                            }
                            // Thinking tokens count toward the live
                            // rate too; the agent is doing real
                            // work and the user wants to see it.
                            gen_chars += s.chars().count();
                            if stream_start.is_none() {
                                stream_start = Some(std::time::Instant::now());
                            }
                            maybe_update_title(&stream_start, &mut last_tick, gen_chars);
                        }
                        UiEvent::Stats(s) => stats = Some(s),
                        UiEvent::Done => {
                            reset_title();
                            if newline_pending { println!(); }
                            // Chat mode: replace the raw streamed
                            // text with markdown-formatted output.
                            // Agent mode keeps the raw stream
                            // because tool pills are interleaved
                            // with assistant tokens — we'd clobber
                            // them.
                            if self.agent_handle.is_none() && !assistant.is_empty() {
                                rerender_markdown(&assistant, stream_width);
                            }
                            if let Some(s) = stats {
                                print_stats(&s);
                            }
                            println!();
                            break;
                        }
                        UiEvent::Error(e) => {
                            reset_title();
                            if newline_pending { println!(); newline_pending = false; }
                            eprintln!("{err}error:{reset} {e}", err = C_ERR, reset = C_RESET);
                            println!();
                            // Agent runs always emit `AgentHistory`
                            // + `Done` from the spawn block, even
                            // on error / cancel — keep draining so
                            // the snapshot lands. Chat runs have no
                            // such follow-up, so break immediately.
                            if self.agent_handle.is_none() {
                                break;
                            }
                        }
                        UiEvent::ToolStart { name } => {
                            if newline_pending { println!(); newline_pending = false; }
                            println!("{tool}→ {name}{reset}", tool = C_TOOL, reset = C_RESET);
                        }
                        UiEvent::ToolEnd { ok, summary } => {
                            let icon = if ok { format!("{ok_}✓{reset}", ok_ = C_OK, reset = C_RESET) }
                                       else { format!("{err}✗{reset}", err = C_ERR, reset = C_RESET) };
                            println!("  {icon} {dim}{summary}{reset}", dim = C_DIM, reset = C_RESET);
                        }
                        UiEvent::Confirm { name, arguments, preview, tx } => {
                            if newline_pending { println!(); newline_pending = false; }
                            let approved = prompt_yes_no(&name, &arguments, preview.as_deref()).await;
                            let _ = tx.send(approved);
                        }
                        UiEvent::AgentHistory(msgs) => {
                            // Replace the incrementally-built UI-pill
                            // representation with the agent loop's
                            // structured history (assistant turns with
                            // `tool_calls`, tool-role replies with
                            // `tool_call_id`). Next agent run will
                            // replay these so the model sees its own
                            // prior tool selections.
                            self.store.active_mut().messages = msgs;
                            // The `assistant` accumulator was used to
                            // mirror the streamed assistant text into
                            // a chat-style ChatMsg at end of turn;
                            // the snapshot replaces it, so wipe to
                            // prevent the duplicate push below.
                            assistant.clear();
                        }
                    }
                }
                _ = ctrl_c() => {
                    interrupts += 1;
                    if interrupts == 1 {
                        if let Some(h) = &self.agent_handle {
                            h.cancel();
                        } else {
                            self.state.cancel();
                        }
                        eprintln!("\n{meta}cancelling… (Ctrl-C again to force exit){reset}",
                                  meta = C_META, reset = C_RESET);
                    } else {
                        std::process::exit(130);
                    }
                }
            }
        }
        if !assistant.is_empty() {
            self.store.active_mut().messages.push(ChatMsg {
                role: "assistant".to_string(),
                content: assistant,
                ..ChatMsg::default()
            });
        }
        self.agent_handle = None;
    }

    // ---- Commands -------------------------------------------------

    async fn handle_command(&mut self, line: &str) -> CmdResult {
        let (verb, args) = match line.split_once(char::is_whitespace) {
            Some((v, r)) => (v, r.trim()),
            None => (line.trim(), ""),
        };
        match verb {
            "help" | "h" | "?" => self.cmd_help(),
            "exit" | "quit" | "q" => return CmdResult::Exit,
            "new" => self.cmd_new(),
            "conv" | "c" => self.cmd_conv(args),
            "next" => self.cmd_cycle(1),
            "prev" => self.cmd_cycle(-1),
            "rename" => self.cmd_rename(args),
            "delete" | "del" => self.cmd_delete(),
            "agent" => {
                self.store.active_mut().settings.agent_mode = Some(true);
                self.save_ignore_err();
                println!(
                    "{meta}agent mode on (for this conversation){reset}",
                    meta = C_META,
                    reset = C_RESET
                );
            }
            "chat" => {
                self.store.active_mut().settings.agent_mode = Some(false);
                self.save_ignore_err();
                println!(
                    "{meta}chat mode (for this conversation){reset}",
                    meta = C_META,
                    reset = C_RESET
                );
            }
            "thinking" => self.cmd_thinking(args),
            "model" => self.cmd_model(args).await,
            "provider" => self.cmd_provider(args),
            "max-steps" => self.cmd_max_steps(args),
            "system" => self.cmd_system(args).await,
            "load" => self.cmd_load(args).await,
            "history" => self.cmd_history(),
            // `/clear` is intercepted before handle_command runs so
            // it can poke the rustyline editor; this branch only
            // matches if something else accidentally routed here.
            "clear" => {}
            "vault" => self.cmd_vault(args),
            "note" => self.cmd_note(args),
            "find" => self.cmd_find(args),
            "embed" => self.cmd_embed(args).await,
            "search" => self.cmd_search(args),
            "tools" => self.cmd_tools(args),
            "export" => self.cmd_export(args),
            "import" => self.cmd_import(args),
            "fork" => self.cmd_fork(),
            "models" => self.cmd_models(args).await,
            "setup" => self.cmd_setup(),
            "undo" => self.cmd_undo(),
            "redo" => self.cmd_redo(),
            "" => {}
            other => {
                println!(
                    "{err}unknown command:{reset} /{other}",
                    err = C_ERR,
                    reset = C_RESET
                );
            }
        }
        CmdResult::Continue
    }

    fn cmd_help(&self) {
        // Each row is rendered via `help_row` so the verb (and any
        // inline command refs in the description) is highlighted in
        // cyan while the column alignment is preserved.
        println!("commands");
        help_row("/help", "this list");
        help_row("/exit", "quit");
        help_row("/new", "start a new conversation");
        help_row("/conv", "fuzzy picker over conversations");
        help_row("/conv list", "plain numbered listing");
        help_row("/conv <n>", "switch to conversation n (1-indexed)");
        help_row("/next /prev", "cycle conversations");
        help_row("/rename <title>", "rename current conversation");
        help_row("/delete", "delete current conversation");
        help_row("/agent /chat", "toggle agent loop (per conversation)");
        help_row(
            "/thinking on|off|toggle",
            "show / hide agent reasoning (per conversation)",
        );
        help_row(
            "/model [name]",
            "picker over current provider's models (no arg) or set by name",
        );
        help_row(
            "/provider [key]",
            "picker over providers (no arg) or set by key",
        );
        help_row(
            "/max-steps <n>",
            &format!("agent step cap (current: {})", self.max_steps),
        );
        help_row("/system [text]", "set system prompt (no arg shows current)");
        help_row("/load <gguf>", "load local model in-session");
        help_row("/history", "show current conversation history");
        help_row(
            "/search [query]",
            "fuzzy picker over all conversation messages",
        );
        help_row("/export <path>", "write the active conversation to JSON");
        help_row("/import <path>", "load a JSON conversation as a new entry");
        help_row("/fork", "duplicate the active conversation");
        help_row(
            "/models [provider] [--refresh]",
            "list models for current provider; --refresh re-fetches the provider's own list",
        );
        help_row("/setup", "re-run the first-launch configuration wizard");
        help_row(
            "/undo",
            "revert the most recent vault edit (journal-backed)",
        );
        help_row("/redo", "reapply the most recent vault undo");
        help_row("/tools", "list tools (✓ enabled · · disabled)");
        help_row(
            "/tools disable <name>",
            "drop a tool from the agent registry",
        );
        help_row(
            "/tools enable <name>",
            "re-enable a previously disabled tool",
        );
        help_row("/clear", "clear the screen");
        println!();
        println!("vault");
        help_row("/vault", "show open vault");
        help_row(
            "/vault <path>",
            "open a vault directory (auto-opens next launch)",
        );
        help_row("/vault close", "forget the saved vault path");
        help_row(
            "/note <path>",
            "read a note from the vault (relative or absolute)",
        );
        help_row(
            "/find <query>",
            "search notes (semantic if /embed loaded, FTS5 otherwise)",
        );
        help_row("/embed", "show embedding model status");
        help_row("/embed <gguf>", "load a local embedding model");
    }

    fn cmd_new(&mut self) {
        self.store.new_conversation(&self.default_system);
        self.store.active_mut().touch();
        self.save_ignore_err();
        // New conv has no messages yet, but ensure no stale rows
        // exist for the freshly minted id (defensive).
        self.reindex_active();
        println!(
            "{meta}new conversation{reset}",
            meta = C_META,
            reset = C_RESET
        );
    }

    fn cmd_conv(&mut self, args: &str) {
        if args == "list" {
            for (i, c) in self.store.conversations.iter().enumerate() {
                let marker = if i == self.store.active { "*" } else { " " };
                println!(
                    " {marker} {idx:>2}  {title}  {hint}",
                    idx = i + 1,
                    title = c.title,
                    hint = conv_hint(c),
                );
            }
            return;
        }
        if args.is_empty() {
            // Fuzzy picker over conversation titles with a
            // disambiguation suffix (msg count + relative time)
            // so duplicates can still be told apart.
            //
            // Items are sorted most-recently-used first; the picker
            // index maps back to the original conversation index
            // via the `ranks` array so a /conv pick still selects
            // the right one.
            let mut ranks: Vec<usize> = (0..self.store.conversations.len()).collect();
            ranks.sort_by_key(|&i| {
                std::cmp::Reverse(self.store.conversations[i].last_used.unwrap_or(0))
            });
            let items: Vec<String> = ranks
                .iter()
                .map(|&i| {
                    let c = &self.store.conversations[i];
                    let active = if i == self.store.active { "* " } else { "  " };
                    format!(
                        "{active}{title}  {hint}",
                        title = c.title,
                        hint = conv_hint(c)
                    )
                })
                .collect();
            if let Some(picked) = crate::picker::pick(items, "conv ", "") {
                self.store.select(ranks[picked]);
                self.save_ignore_err();
                println!(
                    "{meta}-> {title}{reset}",
                    meta = C_META,
                    reset = C_RESET,
                    title = self.store.active().title
                );
            }
            return;
        }
        match args.parse::<usize>() {
            Ok(n) if n >= 1 && n <= self.store.conversations.len() => {
                self.store.select(n - 1);
                self.save_ignore_err();
                println!(
                    "{meta}-> {title}{reset}",
                    meta = C_META,
                    reset = C_RESET,
                    title = self.store.active().title
                );
            }
            _ => println!(
                "{err}usage: /conv <1..{}> | /conv (list){reset}",
                self.store.conversations.len(),
                err = C_ERR,
                reset = C_RESET,
            ),
        }
    }

    fn cmd_cycle(&mut self, delta: i32) {
        let n = self.store.conversations.len() as i32;
        if n <= 1 {
            return;
        }
        let i = ((self.store.active as i32 + delta) % n + n) % n;
        self.store.select(i as usize);
        self.save_ignore_err();
        println!(
            "{meta}-> {title}{reset}",
            meta = C_META,
            reset = C_RESET,
            title = self.store.active().title
        );
    }

    fn cmd_rename(&mut self, args: &str) {
        if args.is_empty() {
            println!(
                "{err}usage: /rename <title>{reset}",
                err = C_ERR,
                reset = C_RESET
            );
            return;
        }
        self.store.rename_active(args.to_string());
        self.save_ignore_err();
    }

    fn cmd_delete(&mut self) {
        let sys = self.default_system.clone();
        // Capture the id BEFORE delete so we can purge its FTS rows.
        // `delete_active` either drops the active conv (slot shifts
        // up — old id is gone) or, when it's the last one,
        // replaces it in-place with a fresh blank (new id).
        let old_id = self.store.active().id.clone();
        self.store.delete_active(&sys);
        self.deindex_conv(&old_id);
        // If we replaced in-place, the new active conv has a fresh
        // id and no messages — reindex to make sure nothing stale
        // is associated with it. If we shifted, the new active was
        // already in the index.
        self.reindex_active();
        self.save_ignore_err();
        println!("{meta}deleted{reset}", meta = C_META, reset = C_RESET);
    }

    async fn cmd_model(&mut self, args: &str) {
        if args.is_empty() {
            // Picker over the current provider's models. For cloud
            // providers, the list comes from `recommended_models` in
            // `models.json`. For `local`, we scan the configured
            // `models_dir` for `*.gguf` files.
            let opts = self.effective_chat_opts();
            let provider_key = opts.provider.clone();
            let current = opts.model.clone();
            if provider_key == "local" {
                self.pick_local_model(current.as_deref()).await;
                return;
            }
            let Some(def) = rezon_core::llm::cloud_provider_def(&provider_key) else {
                println!(
                    "{err}unknown provider: {provider_key}{reset}",
                    err = C_ERR,
                    reset = C_RESET
                );
                return;
            };
            if def.recommended_models.is_empty() {
                println!(
                    "{meta}no recommended models for {provider_key} — use /model <name>{reset}",
                    meta = C_META,
                    reset = C_RESET
                );
                return;
            }
            let items: Vec<String> = def
                .recommended_models
                .iter()
                .map(|m| {
                    let active = current.as_deref() == Some(m.as_str());
                    let default = m == &def.default_model;
                    let marker = if active { "* " } else { "  " };
                    let suffix = if default { "  (default)" } else { "" };
                    format!("{marker}{m}{suffix}")
                })
                .collect();
            let prompt = format!("model [{}] ", def.label);
            if let Some(picked) = crate::picker::pick(items, &prompt, "") {
                let name = def.recommended_models[picked].clone();
                self.store.active_mut().settings.model = Some(name.clone());
                self.save_ignore_err();
                println!(
                    "{meta}model -> {name} (for this conversation){reset}",
                    meta = C_META,
                    reset = C_RESET
                );
            }
            return;
        }
        self.store.active_mut().settings.model = Some(args.to_string());
        self.save_ignore_err();
        println!(
            "{meta}model -> {args} (for this conversation){reset}",
            meta = C_META,
            reset = C_RESET
        );
    }

    /// Pick a local `*.gguf` file from the configured models dir.
    /// "Configured" = `$REZON_MODELS_DIR` if set, otherwise the
    /// `models_dir` field in the persisted store. Resolution and the
    /// directory scan happen here (not in main) so a `/setup` mid-
    /// session takes effect immediately.
    async fn pick_local_model(&mut self, current: Option<&str>) {
        let Some(dir) = effective_models_dir(&self.store) else {
            println!(
                "{err}no models dir configured — set $REZON_MODELS_DIR or run /setup{reset}",
                err = C_ERR,
                reset = C_RESET
            );
            return;
        };
        let path = std::path::Path::new(&dir);
        let entries = match scan_gguf(path) {
            Ok(v) => v,
            Err(e) => {
                println!(
                    "{err}scan {}: {e}{reset}",
                    path.display(),
                    err = C_ERR,
                    reset = C_RESET
                );
                return;
            }
        };
        if entries.is_empty() {
            println!(
                "{meta}no *.gguf files in {}{reset}",
                path.display(),
                meta = C_META,
                reset = C_RESET
            );
            return;
        }
        let items: Vec<String> = entries
            .iter()
            .map(|abs| {
                let display = std::path::Path::new(abs)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(abs.as_str())
                    .to_string();
                let active = current == Some(abs.as_str());
                let marker = if active { "* " } else { "  " };
                format!("{marker}{display}")
            })
            .collect();
        let prompt = format!("model [local · {}] ", path.display());
        let Some(picked) = crate::picker::pick(items, &prompt, "") else {
            return;
        };
        let picked_path = entries[picked].clone();
        self.store.active_mut().settings.model = Some(picked_path.clone());
        self.save_ignore_err();
        // Eagerly load — the user picked, so they expect it ready
        // for the next turn. If load fails, the setting still
        // sticks so a later `/load` retry uses the same path.
        let label = format!("loading {}", basename(&picked_path));
        match crate::spinner::with_spinner(label, self.state.load(picked_path.clone())).await {
            Ok(_) => {
                persist_last_model_path(&picked_path);
                println!(
                    "{meta}model -> {picked_path} (loaded){reset}",
                    meta = C_META,
                    reset = C_RESET
                );
            }
            Err(e) => println!(
                "{err}load {picked_path}: {e}{reset}",
                err = C_ERR,
                reset = C_RESET
            ),
        }
    }

    fn cmd_provider(&mut self, args: &str) {
        if args.is_empty() {
            // Picker over every known provider, plus `local`. Current
            // provider is marked `*`. `Other` (the user-configurable
            // OpenAI-compatible slot) needs base_url + api_key to be
            // useful, so we still expose it but the picker doesn't
            // try to do the setup — that path is `/provider other`
            // followed by env config.
            let current = self.effective_chat_opts().provider;
            let mut keys: Vec<String> = vec!["local".to_string()];
            let mut labels: Vec<String> = vec!["Local (loaded GGUF)".to_string()];
            for def in rezon_core::llm::cloud_providers_catalog() {
                keys.push(def.key.clone());
                labels.push(def.label.clone());
            }
            let items: Vec<String> = keys
                .iter()
                .zip(labels.iter())
                .map(|(k, l)| {
                    let active = k == &current;
                    let marker = if active { "* " } else { "  " };
                    format!(
                        "{marker}{k}  {meta}({l}){reset}",
                        meta = C_META,
                        reset = C_RESET
                    )
                })
                .collect();
            if let Some(picked) = crate::picker::pick(items, "provider ", "") {
                let key = keys[picked].clone();
                // Switching providers invalidates the per-conv model
                // (the old name almost certainly isn't valid for the
                // new provider). Clear it so the new provider's
                // default kicks in — the user can override with
                // /model afterwards.
                self.store.active_mut().settings.provider = Some(key.clone());
                self.store.active_mut().settings.model = None;
                self.save_ignore_err();
                println!(
                    "{meta}provider -> {key} (for this conversation){reset}",
                    meta = C_META,
                    reset = C_RESET
                );
            }
            return;
        }
        self.store.active_mut().settings.provider = Some(args.to_string());
        self.save_ignore_err();
        println!(
            "{meta}provider -> {args} (for this conversation){reset}",
            meta = C_META,
            reset = C_RESET
        );
    }

    fn cmd_thinking(&mut self, args: &str) {
        let new_val = match args.trim() {
            "on" | "show" => true,
            "off" | "hide" => false,
            "" | "toggle" => !self.effective_show_thinking(),
            other => {
                println!(
                    "{err}unknown: /thinking {other}  (use: on / off / toggle){reset}",
                    err = C_ERR,
                    reset = C_RESET
                );
                return;
            }
        };
        self.store.active_mut().settings.show_thinking = Some(new_val);
        self.save_ignore_err();
        println!(
            "{meta}thinking visibility: {} (for this conversation){reset}",
            if new_val { "on" } else { "off" },
            meta = C_META,
            reset = C_RESET
        );
    }

    fn cmd_max_steps(&mut self, args: &str) {
        match args.parse::<usize>() {
            Ok(n) if n > 0 => {
                self.max_steps = n;
                println!(
                    "{meta}max-steps -> {n}{reset}",
                    meta = C_META,
                    reset = C_RESET
                );
            }
            _ => println!(
                "{err}usage: /max-steps <positive integer>{reset}",
                err = C_ERR,
                reset = C_RESET
            ),
        }
    }

    async fn cmd_system(&mut self, args: &str) {
        if args.is_empty() {
            let current = self.store.active().system.clone();
            if current.trim().is_empty() {
                println!(
                    "{meta}no system prompt set for this conversation{reset}",
                    meta = C_META,
                    reset = C_RESET
                );
            } else {
                println!(
                    "{meta}current system prompt:{reset}",
                    meta = C_META,
                    reset = C_RESET
                );
                for line in current.lines() {
                    println!("  {line}");
                }
            }
            return;
        }
        self.set_active_system(args.to_string());
        self.save_ignore_err();
        println!(
            "{meta}system prompt updated{reset}",
            meta = C_META,
            reset = C_RESET
        );
    }

    async fn cmd_load(&self, args: &str) {
        if args.is_empty() {
            println!(
                "{err}usage: /load <gguf path>{reset}",
                err = C_ERR,
                reset = C_RESET
            );
            return;
        }
        let label = format!("loading {}", basename(args));
        let result = crate::spinner::with_spinner(label, self.state.load(args.to_string())).await;
        match result {
            Ok(_) => {
                persist_last_model_path(args);
                println!(
                    "{meta}local model loaded{reset}",
                    meta = C_META,
                    reset = C_RESET
                );
            }
            Err(e) => println!("{err}load: {e}{reset}", err = C_ERR, reset = C_RESET),
        }
    }

    fn cmd_tools(&mut self, args: &str) {
        // Mirror the registry assembly in `spawn_agent_run` so the
        // list reflects what the agent actually sees.
        use rezon_core::agent::{
            tool::ToolRegistry,
            tools::{register_core_tools, register_search_notes, register_write_note},
        };
        let mut reg = ToolRegistry::new();
        register_core_tools(&mut reg);
        if self.vault.active_vault().is_some() {
            register_search_notes(
                &mut reg,
                self.vault.search.clone(),
                self.vault.embed.clone(),
            );
            register_write_note(&mut reg, self.vault.search.clone());
        }
        let all: Vec<String> = reg.names().map(str::to_string).collect();

        let (verb, name) = match args.split_once(char::is_whitespace) {
            Some((v, n)) => (v.trim(), n.trim()),
            None => (args.trim(), ""),
        };
        match verb {
            "" | "list" => {
                for n in &all {
                    let disabled = self.store.disabled_tools.iter().any(|d| d == n);
                    let (marker, style) = if disabled {
                        ("·", C_DIM)
                    } else {
                        ("✓", C_OK)
                    };
                    let _ = style;
                    println!(
                        "  {style}{marker}{reset}  {n}",
                        marker = marker,
                        style = if disabled { C_DIM } else { C_OK },
                        reset = C_RESET,
                    );
                }
                if all.is_empty() {
                    println!(
                        "{meta}no tools registered{reset}",
                        meta = C_META,
                        reset = C_RESET
                    );
                }
            }
            "disable" => {
                let pick_name = if name.is_empty() {
                    let enabled: Vec<String> = all
                        .iter()
                        .filter(|n| !self.store.disabled_tools.iter().any(|d| d == *n))
                        .cloned()
                        .collect();
                    if enabled.is_empty() {
                        println!(
                            "{meta}all tools already disabled{reset}",
                            meta = C_META,
                            reset = C_RESET
                        );
                        return;
                    }
                    match crate::picker::pick(enabled.clone(), "disable ", "") {
                        Some(i) => enabled[i].clone(),
                        None => return,
                    }
                } else if all.iter().any(|n| n == name) {
                    name.to_string()
                } else {
                    println!(
                        "{err}unknown tool: {name}{reset}",
                        err = C_ERR,
                        reset = C_RESET
                    );
                    return;
                };
                if !self.store.disabled_tools.iter().any(|d| d == &pick_name) {
                    self.store.disabled_tools.push(pick_name.clone());
                    self.save_ignore_err();
                }
                println!(
                    "{meta}disabled: {pick_name}{reset}",
                    meta = C_META,
                    reset = C_RESET
                );
            }
            "enable" => {
                let pick_name = if name.is_empty() {
                    let disabled = self.store.disabled_tools.clone();
                    if disabled.is_empty() {
                        println!(
                            "{meta}no disabled tools{reset}",
                            meta = C_META,
                            reset = C_RESET
                        );
                        return;
                    }
                    match crate::picker::pick(disabled.clone(), "enable ", "") {
                        Some(i) => disabled[i].clone(),
                        None => return,
                    }
                } else {
                    name.to_string()
                };
                let before = self.store.disabled_tools.len();
                self.store.disabled_tools.retain(|d| d != &pick_name);
                if self.store.disabled_tools.len() != before {
                    self.save_ignore_err();
                }
                println!(
                    "{meta}enabled: {pick_name}{reset}",
                    meta = C_META,
                    reset = C_RESET
                );
            }
            other => {
                println!(
                    "{err}unknown subcommand: /tools {other}{reset}",
                    err = C_ERR,
                    reset = C_RESET
                );
            }
        }
    }

    fn cmd_search(&mut self, args: &str) {
        // Two paths converge into the same picker:
        //   * non-empty query + FTS index: query the index and map
        //     `Hit{conv_id, msg_idx}` back to `(conv_idx, msg_idx)`.
        //     FTS5's `snippet()` highlights the match window with
        //     `<<` / `>>` markers — shown verbatim in the display.
        //   * empty query or missing index: linear walk of every
        //     non-system / non-tool message, picker filters.
        struct Candidate {
            display: String,
            conv_idx: usize,
            msg_idx: usize,
        }
        let mut candidates: Vec<Candidate> = Vec::new();
        let query = args.trim();
        let use_fts = !query.is_empty() && self.conv_index.is_some();
        if use_fts {
            // Safety: `use_fts` implies index presence.
            let idx = self.conv_index.as_ref().unwrap();
            match idx.search(query, 200) {
                Ok(hits) => {
                    for h in hits {
                        let Some(ci) = self
                            .store
                            .conversations
                            .iter()
                            .position(|c| c.id == h.conv_id)
                        else {
                            continue;
                        };
                        let conv = &self.store.conversations[ci];
                        let role_label: String = match h.role.as_str() {
                            "user" => "you".into(),
                            "assistant" => "rzn".into(),
                            other => other.into(),
                        };
                        // The snippet is short by design; collapse
                        // any embedded newlines so the picker row
                        // stays on one terminal line.
                        let snippet: String =
                            h.snippet.replace('\n', " ").chars().take(120).collect();
                        candidates.push(Candidate {
                            display: format!("{role_label} · {snippet}  ({})", conv.title),
                            conv_idx: ci,
                            msg_idx: h.msg_idx,
                        });
                    }
                }
                Err(e) => {
                    eprintln!("conv index search: {e}");
                }
            }
        }
        if candidates.is_empty() {
            // Fall back to linear walk so the user can browse + pick
            // when no FTS hits (or no index). Pass `args` as the
            // picker's initial filter.
            for (ci, conv) in self.store.conversations.iter().enumerate() {
                for (mi, m) in conv.messages.iter().enumerate() {
                    if matches!(m.role.as_str(), "system" | "tool") {
                        continue;
                    }
                    let role_label = match m.role.as_str() {
                        "user" => "you",
                        "assistant" => "rzn",
                        other => other,
                    };
                    let first_line = m.content.lines().next().unwrap_or("");
                    let snippet: String = first_line.chars().take(80).collect();
                    candidates.push(Candidate {
                        display: format!("{role_label} · {snippet}  ({})", conv.title),
                        conv_idx: ci,
                        msg_idx: mi,
                    });
                }
            }
        }
        if candidates.is_empty() {
            println!(
                "{meta}no messages to search{reset}",
                meta = C_META,
                reset = C_RESET
            );
            return;
        }
        let items: Vec<String> = candidates.iter().map(|c| c.display.clone()).collect();
        // When FTS already filtered for us we pre-seed the picker
        // with an empty buffer; otherwise pass `args` through as a
        // free-text refinement.
        let initial = if use_fts && !candidates.is_empty() {
            ""
        } else {
            args
        };
        let Some(idx) = crate::picker::pick(items, "search ", initial) else {
            return;
        };
        let pick = &candidates[idx];
        // Switch to the picked conversation and print the message
        // body so the user sees the match in context.
        if self.store.active != pick.conv_idx {
            self.store.select(pick.conv_idx);
            self.save_ignore_err();
        }
        let conv = self.store.active();
        println!(
            "{meta}-> {title}{reset}",
            meta = C_META,
            reset = C_RESET,
            title = conv.title
        );
        println!();
        let msg = &conv.messages[pick.msg_idx];
        let role_label = match msg.role.as_str() {
            "user" => "you",
            "assistant" => "rzn",
            other => other,
        };
        println!("{meta}{role_label}:{reset}", meta = C_META, reset = C_RESET);
        for line in msg.content.lines() {
            println!("  {line}");
        }
        println!();
    }

    #[allow(dead_code)]
    fn cmd_search_substring(&self, args: &str) {
        let q_lc = args.to_lowercase();
        // Cap output so a popular term doesn't bury the terminal.
        const MAX_LINES_PER_CONV: usize = 6;
        const MAX_TOTAL_LINES: usize = 80;
        let mut total_lines = 0usize;
        let mut total_convs = 0usize;
        let mut total_messages = 0usize;

        // Iterate newest-first so recent matches surface at the top
        // of the output buffer.
        for (i, conv) in self.store.conversations.iter().enumerate().rev() {
            // Collect (msg_index, &ChatMsg) hits inside this conv.
            let hits: Vec<&ChatMsg> = conv
                .messages
                .iter()
                .filter(|m| m.content.to_lowercase().contains(&q_lc))
                .collect();
            if hits.is_empty() {
                continue;
            }
            total_messages += hits.len();
            let marker = if i == self.store.active { "*" } else { " " };
            println!(
                "{meta}{marker} conv {idx:>2} · {title}{reset}",
                marker = marker,
                idx = i + 1,
                title = conv.title,
                meta = C_META,
                reset = C_RESET,
            );
            let mut conv_lines = 0usize;
            'msg: for m in hits {
                let role_label = match m.role.as_str() {
                    "user" => "you",
                    "assistant" => "rzn",
                    "system" => "sys",
                    "tool" => "tool",
                    other => other,
                };
                for raw_line in m.content.lines() {
                    if !raw_line.to_lowercase().contains(&q_lc) {
                        continue;
                    }
                    let highlighted = highlight_match(raw_line, args);
                    println!(
                        "    {meta}{role:>4}{reset}  {highlighted}",
                        role = role_label,
                        meta = C_META,
                        reset = C_RESET,
                    );
                    conv_lines += 1;
                    total_lines += 1;
                    if conv_lines >= MAX_LINES_PER_CONV {
                        println!("    {meta}…{reset}", meta = C_META, reset = C_RESET);
                        break 'msg;
                    }
                    if total_lines >= MAX_TOTAL_LINES {
                        break 'msg;
                    }
                }
            }
            println!();
            total_convs += 1;
            if total_lines >= MAX_TOTAL_LINES {
                println!(
                    "{meta}(output capped at {} lines · refine the query for more){reset}",
                    MAX_TOTAL_LINES,
                    meta = C_META,
                    reset = C_RESET,
                );
                break;
            }
        }
        if total_convs == 0 {
            println!("{meta}no matches{reset}", meta = C_META, reset = C_RESET);
        } else {
            println!(
                "{meta}{m} match{mp} across {c} conversation{cp}{reset}",
                m = total_messages,
                mp = if total_messages == 1 { "" } else { "es" },
                c = total_convs,
                cp = if total_convs == 1 { "" } else { "s" },
                meta = C_META,
                reset = C_RESET,
            );
        }
    }

    fn cmd_vault(&mut self, args: &str) {
        let args = args.trim();
        if args.is_empty() {
            match self.vault.active_vault() {
                Some(p) => println!("{meta}vault: {p}{reset}", meta = C_META, reset = C_RESET),
                None => println!(
                    "{meta}no vault open · use {app}/vault <path>{reset}",
                    meta = C_META,
                    app = C_APP,
                    reset = C_RESET
                ),
            }
            return;
        }
        if args == "close" {
            let was_open = self.vault.close();
            self.store.active_vault = None;
            self.save_ignore_err();
            if was_open {
                println!("{meta}vault closed{reset}", meta = C_META, reset = C_RESET);
            } else {
                println!(
                    "{meta}no vault to close (forgot persisted path){reset}",
                    meta = C_META,
                    reset = C_RESET
                );
            }
            return;
        }
        match self.vault.open(args) {
            Ok(_) => {
                let active = self.vault.active_vault();
                self.store.active_vault = active.clone();
                self.save_ignore_err();
                println!(
                    "{meta}vault: {}{reset}",
                    active.as_deref().unwrap_or(args),
                    meta = C_META,
                    reset = C_RESET
                );
            }
            Err(e) => {
                println!("{err}vault: {e}{reset}", err = C_ERR, reset = C_RESET);
            }
        }
    }

    fn cmd_note(&self, args: &str) {
        if args.is_empty() {
            println!(
                "{err}usage: /note <path>{reset}",
                err = C_ERR,
                reset = C_RESET
            );
            return;
        }
        match self.vault.read_note(args) {
            Ok(text) => {
                println!("{}", text);
            }
            Err(e) => println!("{err}note: {e}{reset}", err = C_ERR, reset = C_RESET),
        }
    }

    fn cmd_find(&self, args: &str) {
        if args.is_empty() {
            println!(
                "{err}usage: /find <query>{reset}",
                err = C_ERR,
                reset = C_RESET
            );
            return;
        }
        match self.vault.find(args, 8) {
            Ok((hits, mode)) => {
                if hits.is_empty() {
                    println!(
                        "{meta}no matches ({mode}){reset}",
                        meta = C_META,
                        reset = C_RESET
                    );
                    return;
                }
                println!(
                    "{meta}{n} match{plural} ({mode}){reset}",
                    meta = C_META,
                    reset = C_RESET,
                    n = hits.len(),
                    plural = if hits.len() == 1 { "" } else { "es" },
                );
                for h in hits {
                    println!("{app}{}{reset}", h.path, app = C_APP, reset = C_RESET);
                    for line in h.snippet.lines() {
                        println!("  {dim}{line}{reset}", dim = C_DIM, reset = C_RESET);
                    }
                }
            }
            Err(e) => println!("{err}find: {e}{reset}", err = C_ERR, reset = C_RESET),
        }
    }

    async fn cmd_embed(&self, args: &str) {
        let args = args.trim();
        if args.is_empty() {
            let s = self.vault.embed_status();
            if s.loaded {
                println!(
                    "{meta}embed: {} (dim={}){reset}",
                    s.path.unwrap_or_default(),
                    s.dim.unwrap_or(0),
                    meta = C_META,
                    reset = C_RESET
                );
            } else {
                println!(
                    "{meta}no embed model loaded · use {app}/embed <gguf>{reset}",
                    meta = C_META,
                    app = C_APP,
                    reset = C_RESET
                );
            }
            return;
        }
        let label = format!("loading {}", basename(args));
        let result =
            crate::spinner::with_spinner(label, self.vault.load_embed(args.to_string())).await;
        match result {
            Ok(s) => println!(
                "{meta}embed: loaded (dim={}){reset}",
                s.dim.unwrap_or(0),
                meta = C_META,
                reset = C_RESET
            ),
            Err(e) => println!("{err}embed: {e}{reset}", err = C_ERR, reset = C_RESET),
        }
    }

    fn cmd_export(&self, args: &str) {
        let conv = self.store.active();
        // Resolve the destination path. Three cases:
        //   * absolute / contains `/` → use verbatim
        //   * filename only + output_dir set → write under output_dir
        //   * empty → auto-name from conv title under output_dir
        // No output_dir + empty arg is still an error.
        let target: std::path::PathBuf = if args.is_empty() {
            let Some(dir) = self.store.output_dir.as_deref() else {
                println!(
                    "{err}usage: /export <path>  (or set an output dir via /setup){reset}",
                    err = C_ERR,
                    reset = C_RESET
                );
                return;
            };
            let stem: String = conv
                .title
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '-'
                    }
                })
                .collect();
            let stem = if stem.trim_matches('-').is_empty() {
                "conversation".to_string()
            } else {
                stem
            };
            std::path::Path::new(dir).join(format!("{stem}.json"))
        } else {
            let p = std::path::Path::new(args);
            if p.is_absolute() || args.contains('/') {
                p.to_path_buf()
            } else if let Some(dir) = self.store.output_dir.as_deref() {
                std::path::Path::new(dir).join(args)
            } else {
                p.to_path_buf()
            }
        };
        if let Some(parent) = target.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                println!(
                    "{err}mkdir {}: {e}{reset}",
                    parent.display(),
                    err = C_ERR,
                    reset = C_RESET
                );
                return;
            }
        }
        let json = match serde_json::to_string_pretty(conv) {
            Ok(s) => s,
            Err(e) => {
                println!("{err}serialize: {e}{reset}", err = C_ERR, reset = C_RESET);
                return;
            }
        };
        match std::fs::write(&target, json) {
            Ok(_) => println!(
                "{meta}exported {} -> {}{reset}",
                conv.title,
                target.display(),
                meta = C_META,
                reset = C_RESET
            ),
            Err(e) => println!("{err}write: {e}{reset}", err = C_ERR, reset = C_RESET),
        }
    }

    fn cmd_import(&mut self, args: &str) {
        if args.is_empty() {
            println!(
                "{err}usage: /import <path>{reset}",
                err = C_ERR,
                reset = C_RESET
            );
            return;
        }
        let bytes = match std::fs::read(args) {
            Ok(b) => b,
            Err(e) => {
                println!("{err}read: {e}{reset}", err = C_ERR, reset = C_RESET);
                return;
            }
        };
        let mut conv: crate::store::Conversation = match serde_json::from_slice(&bytes) {
            Ok(c) => c,
            Err(e) => {
                println!("{err}parse: {e}{reset}", err = C_ERR, reset = C_RESET);
                return;
            }
        };
        // Re-key with a fresh id so an imported copy can coexist with
        // the original (or with a previous import of the same file).
        conv.id = crate::store::next_id();
        conv.touch();
        let title = conv.title.clone();
        self.store.conversations.push(conv);
        self.store.active = self.store.conversations.len() - 1;
        self.reindex_active();
        self.save_ignore_err();
        println!(
            "{meta}imported -> {title}{reset}",
            meta = C_META,
            reset = C_RESET
        );
    }

    fn cmd_fork(&mut self) {
        let mut clone = self.store.active().clone();
        clone.id = crate::store::next_id();
        clone.touch();
        // Mark the fork distinctly. If the title already ends with
        // `(fork)` we still append, so chained forks read as
        // `foo (fork) (fork)` — easier than counting.
        clone.title = format!("{} (fork)", clone.title);
        let new_title = clone.title.clone();
        self.store.conversations.push(clone);
        self.store.active = self.store.conversations.len() - 1;
        self.reindex_active();
        self.save_ignore_err();
        println!(
            "{meta}forked -> {new_title}{reset}",
            meta = C_META,
            reset = C_RESET
        );
    }

    async fn cmd_models(&self, args: &str) {
        // `/models` defaults to the *current* provider — it's a
        // listing command, not a switcher. Pass `/models <key>` (or
        // `/models local`) to look at a different provider's catalog
        // without changing the active one. `--refresh` forces a fetch
        // of the provider's own list past its cache TTL.
        let mut refresh = false;
        let mut rest: Vec<&str> = Vec::new();
        for tok in args.split_whitespace() {
            if tok == "--refresh" || tok == "-r" {
                refresh = true;
            } else {
                rest.push(tok);
            }
        }
        let provider_key = if rest.is_empty() {
            self.effective_chat_opts().provider
        } else {
            rest.join(" ")
        };
        if provider_key == "local" {
            let dir = effective_models_dir(&self.store);
            let header_dir = dir
                .as_deref()
                .unwrap_or("<unset — run /setup or set $REZON_MODELS_DIR>");
            println!(
                "{bold}Local{bold:#} {meta}({}){reset}",
                header_dir,
                bold = C_BOLD,
                meta = C_META,
                reset = C_RESET
            );
            let loaded_path = self.state.status().path;
            if let Some(dir_str) = dir {
                let dir_path = std::path::Path::new(&dir_str);
                match scan_gguf(dir_path) {
                    Ok(entries) if entries.is_empty() => println!(
                        "  {meta}(no *.gguf files){reset}",
                        meta = C_META,
                        reset = C_RESET
                    ),
                    Ok(entries) => {
                        for abs in &entries {
                            let active = loaded_path.as_deref() == Some(abs.as_str());
                            let marker = if active {
                                format!("{C_APP}*{C_APP:#}")
                            } else {
                                " ".to_string()
                            };
                            let display = std::path::Path::new(abs)
                                .file_name()
                                .and_then(|s| s.to_str())
                                .unwrap_or(abs.as_str());
                            println!("  {marker} {display}");
                        }
                    }
                    Err(e) => println!(
                        "{err}scan {}: {e}{reset}",
                        dir_path.display(),
                        err = C_ERR,
                        reset = C_RESET
                    ),
                }
            }
            if let Some(p) = loaded_path {
                println!(
                    "  {meta}(currently loaded: {p}){reset}",
                    meta = C_META,
                    reset = C_RESET
                );
            }
            return;
        }
        let Some(def) = rezon_core::llm::cloud_provider_def(&provider_key) else {
            println!(
                "{err}unknown provider: {provider_key}{reset}",
                err = C_ERR,
                reset = C_RESET
            );
            return;
        };
        println!(
            "{bold}{}{bold:#} {meta}({} models){reset}",
            def.label,
            def.recommended_models.len(),
            bold = C_BOLD,
            meta = C_META,
            reset = C_RESET
        );
        let current = self.effective_chat_opts().model;
        for m in &def.recommended_models {
            let active = current.as_deref() == Some(m.as_str());
            let default = m == &def.default_model;
            let marker = if active {
                format!("{C_APP}*{C_APP:#}")
            } else {
                " ".to_string()
            };
            let suffix = if default {
                format!(" {C_META}(default){C_META:#}")
            } else {
                String::new()
            };
            println!("  {marker} {m}{suffix}");
        }
        if def.recommended_models.is_empty() {
            println!(
                "  {meta}(no recommended list){reset}",
                meta = C_META,
                reset = C_RESET
            );
        }
        self.print_fetched_models(def, refresh).await;
    }

    /// Append the provider's own `/v1/models` list beneath the curated
    /// one. Quiet on failure: the recommended list above is already a
    /// working answer and the model field accepts free text, so an
    /// unreachable endpoint is a non-event.
    async fn print_fetched_models(&self, def: &rezon_core::llm::CloudProviderDef, refresh: bool) {
        use rezon_core::model_catalog as mc;

        if def.user_configurable || def.base_url.is_empty() {
            return;
        }
        let api_key = rezon_core::llm::lookup_api_key(
            &def.key,
            &def.env_var,
            None,
            &rezon_core::secrets::KeyringStore,
        )
        .map(|(k, _)| k);
        if api_key.is_none() {
            return;
        }
        let Some(path) = mc::cache_path() else {
            return;
        };
        let cat = mc::resolve_catalog(
            &def.key,
            &def.base_url,
            api_key.as_deref(),
            &def.recommended_models,
            &path,
            refresh,
            mc::DEFAULT_TTL_SECS,
        )
        .await;
        if cat.fetched.is_empty() {
            if let Some(e) = cat.error {
                println!(
                    "  {meta}(provider list unavailable: {e}){reset}",
                    meta = C_META,
                    reset = C_RESET
                );
            }
            return;
        }
        let note = match cat.source {
            mc::CatalogSource::StaleCache => " (cached; refresh failed)",
            mc::CatalogSource::Cache => " (cached)",
            _ => "",
        };
        println!(
            "  {meta}— {} more from provider{note}{reset}",
            cat.fetched.len(),
            meta = C_META,
            reset = C_RESET
        );
        let current = self.effective_chat_opts().model;
        for m in &cat.fetched {
            let marker = if current.as_deref() == Some(m.as_str()) {
                format!("{C_APP}*{C_APP:#}")
            } else {
                " ".to_string()
            };
            println!(
                "  {marker} {meta}{m}{reset}",
                meta = C_META,
                reset = C_RESET
            );
        }
    }

    fn cmd_undo(&mut self) {
        let Some(vault) = self.vault.active_vault() else {
            println!(
                "{err}/undo requires an open vault{reset}",
                err = C_ERR,
                reset = C_RESET,
            );
            return;
        };
        match rezon_core::journal::undo_last_op(&vault) {
            Ok(None) => println!(
                "{meta}nothing to undo{reset}",
                meta = C_META,
                reset = C_RESET,
            ),
            Ok(Some(out)) => {
                // Keep the search index aligned with the on-disk
                // state after the revert — journal doesn't depend
                // on search, so the touch is the shell's job.
                let abs = std::path::Path::new(&vault).join(&out.path);
                let _ = rezon_core::search::vault_index_touch(
                    &self.vault.search,
                    &vault,
                    &abs.to_string_lossy(),
                );
                let git_suffix = if out.journal.git_committed {
                    " (git committed)"
                } else {
                    ""
                };
                println!(
                    "{meta}undid {tool} on {path}{git}{reset}",
                    meta = C_META,
                    reset = C_RESET,
                    tool = out.tool,
                    path = out.path,
                    git = git_suffix,
                );
                if let Some(w) = out.journal.git_warning {
                    println!(
                        "{meta}  git warning: {w}{reset}",
                        meta = C_META,
                        reset = C_RESET
                    );
                }
            }
            Err(e) => println!("{err}/undo: {e}{reset}", err = C_ERR, reset = C_RESET),
        }
    }

    fn cmd_redo(&mut self) {
        let Some(vault) = self.vault.active_vault() else {
            println!(
                "{err}/redo requires an open vault{reset}",
                err = C_ERR,
                reset = C_RESET,
            );
            return;
        };
        match rezon_core::journal::redo_last_op(&vault) {
            Ok(None) => println!(
                "{meta}nothing to redo{reset}",
                meta = C_META,
                reset = C_RESET,
            ),
            Ok(Some(out)) => {
                let abs = std::path::Path::new(&vault).join(&out.path);
                let _ = rezon_core::search::vault_index_touch(
                    &self.vault.search,
                    &vault,
                    &abs.to_string_lossy(),
                );
                let git_suffix = if out.journal.git_committed {
                    " (git committed)"
                } else {
                    ""
                };
                println!(
                    "{meta}redid {path}{git}{reset}",
                    meta = C_META,
                    reset = C_RESET,
                    path = out.path,
                    git = git_suffix,
                );
                if let Some(w) = out.journal.git_warning {
                    println!(
                        "{meta}  git warning: {w}{reset}",
                        meta = C_META,
                        reset = C_RESET
                    );
                }
            }
            Err(e) => println!("{err}/redo: {e}{reset}", err = C_ERR, reset = C_RESET),
        }
    }

    fn cmd_setup(&mut self) {
        if let Err(e) = crate::setup::run_forced(&mut self.store) {
            println!("{err}setup: {e}{reset}", err = C_ERR, reset = C_RESET);
        }
    }

    fn cmd_history(&self) {
        for m in &self.store.active().messages {
            match m.role.as_str() {
                "user" => println!(
                    "{user}> {}{reset}",
                    m.content,
                    user = C_USER,
                    reset = C_RESET
                ),
                // Route assistant content through the same markdown
                // renderer used for live streaming, so /history is
                // formatted consistently with the in-flight display.
                "assistant" => {
                    let rendered = crate::markdown::render(&m.content);
                    // `render` already ends in a newline.
                    print!("{rendered}");
                }
                "system" => println!(
                    "{dim}[system] {}{reset}",
                    m.content,
                    dim = C_DIM,
                    reset = C_RESET
                ),
                "tool" => println!("{tool}{}{reset}", m.content, tool = C_TOOL, reset = C_RESET),
                _ => println!("[{role}] {}", m.content, role = m.role),
            }
            println!();
        }
    }

    fn set_active_system(&mut self, text: String) {
        let convo = self.store.active_mut();
        convo.system = text.clone();
        let has_system = matches!(convo.messages.first(), Some(m) if m.role == "system");
        if text.trim().is_empty() {
            if has_system {
                convo.messages.remove(0);
            }
        } else if has_system {
            convo.messages[0].content = text;
        } else {
            convo.messages.insert(
                0,
                ChatMsg {
                    role: "system".to_string(),
                    content: text,
                    ..ChatMsg::default()
                },
            );
        }
    }

    fn save_ignore_err(&self) {
        if let Err(e) = self.store.save() {
            eprintln!("save store: {e}");
        }
    }
}

/// Record `path` as the most-recently-loaded local model. Best-
/// effort: filesystem failures are swallowed (a stale or absent
/// `last_model.txt` only costs the user one extra `/model` pick on
/// the next launch).
fn persist_last_model_path(path: &str) {
    if let Ok(d) = crate::store::config_dir() {
        rezon_core::llm::persist_last_model(&d, path);
    }
}

/// Resolve the active models dir. `$REZON_MODELS_DIR` wins so the
/// user can override per-shell; otherwise we use the value the setup
/// wizard persisted into the store. `None` means neither is set —
/// callers should send the user to `/setup`.
fn effective_models_dir(store: &crate::store::Store) -> Option<String> {
    std::env::var("REZON_MODELS_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| store.models_dir.clone())
}

/// Non-recursive scan of `dir` for `*.gguf` files. Returns absolute
/// paths sorted alphabetically. A missing directory yields `Ok(vec![])`
/// rather than an error so callers can show the empty-list message
/// instead of a stack trace.
fn scan_gguf(dir: &std::path::Path) -> std::io::Result<Vec<String>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("gguf") {
            out.push(path.to_string_lossy().into_owned());
        }
    }
    out.sort();
    Ok(out)
}

enum CmdResult {
    Continue,
    Exit,
}

/// Render a single `/help` row: the verb segment in cyan, the
/// description in default fg. Splits on the first whitespace so a
/// trailing `<arg>` or `[arg]` syntax stays in cyan along with the
/// slash command, but multi-word usages like `/next /prev` still
/// render both verbs in cyan.
fn help_row(usage: &str, description: &str) {
    // We word-split and colour each `/`-prefixed token cyan, others
    // (placeholders like `<n>`, `[text]`, …) in default fg. Width
    // is computed on the *plain* text so the SGR codes don't shift
    // the description column.
    const COLUMN: usize = 20;
    let plain_width = usage.chars().count();
    let pad = COLUMN.saturating_sub(plain_width);
    let mut rendered = String::with_capacity(usage.len() + 16);
    for (i, token) in usage.split(' ').enumerate() {
        if i > 0 {
            rendered.push(' ');
        }
        if token.starts_with('/') {
            rendered.push_str(&format!("{C_APP}{token}{C_APP:#}"));
        } else {
            rendered.push_str(token);
        }
    }
    println!("  {rendered}{pad}{description}", pad = " ".repeat(pad));
}

/// Overwrite the raw streamed assistant text with a markdown-
/// formatted version. Counts the rows the raw text occupied, scrolls
/// the cursor back to the top of that block, clears to end-of-screen,
/// and writes the rendered output in place. Silently no-ops when
/// stdout isn't a tty (piped output keeps the raw markdown — still
/// human-readable, and avoids spewing cursor-control escapes into
/// pipes that may not strip them).
fn rerender_markdown(raw: &str, stream_width: Option<u16>) {
    use std::io::Write;
    if !std::io::stdout().is_terminal() {
        return;
    }
    // Prefer the width captured when streaming started — that's the
    // width the visible rows were laid out against. Fall back to the
    // current terminal width when we never recorded one (defensive).
    let width = stream_width.unwrap_or_else(|| {
        terminal_size::terminal_size()
            .map(|(terminal_size::Width(w), _)| w)
            .unwrap_or(80)
    });
    let rows = crate::markdown::count_rows(raw, width);
    if rows == 0 {
        return;
    }
    let formatted = crate::markdown::render(raw);
    let mut out = anstream::stdout().lock();
    // \x1b[<n>A moves the cursor up n rows. \r returns to col 0.
    // \x1b[J clears from the cursor to the end of the screen.
    let _ = write!(out, "\x1b[{rows}A\r\x1b[J");
    let _ = out.write_all(formatted.as_bytes());
    let _ = out.flush();
}

/// Update the terminal-window title with a rolling tok/s estimate,
/// throttled to once every ~200ms. The rate is approximated from
/// emitted character count divided by 4 (matches the heuristic the
/// chat path uses when the provider omits usage). No-op when stdout
/// isn't a tty so piped runs don't pollute the receiving stream
/// with OSC escapes.
fn maybe_update_title(
    stream_start: &Option<std::time::Instant>,
    last_tick: &mut Option<std::time::Instant>,
    gen_chars: usize,
) {
    use std::io::Write;
    let Some(start) = stream_start else { return };
    if !std::io::stdout().is_terminal() {
        return;
    }
    let now = std::time::Instant::now();
    if let Some(prev) = last_tick {
        if now.duration_since(*prev) < std::time::Duration::from_millis(200) {
            return;
        }
    }
    *last_tick = Some(now);
    let secs = now.duration_since(*start).as_secs_f64().max(0.001);
    let tps = (gen_chars as f64 / 4.0) / secs;
    // OSC 0: sets both window title and icon name. `\x07` (BEL) is
    // the legacy terminator; widely supported.
    let mut out = anstream::stdout().lock();
    let _ = write!(out, "\x1b]0;rezon · ~{tps:.1} tok/s\x07");
    let _ = out.flush();
}

/// Restore the terminal-window title to a neutral value once the
/// stream ends (or errors / is cancelled). Paired with
/// `maybe_update_title`.
fn reset_title() {
    use std::io::Write;
    if !std::io::stdout().is_terminal() {
        return;
    }
    let mut out = anstream::stdout().lock();
    let _ = write!(out, "\x1b]0;rezon\x07");
    let _ = out.flush();
}

/// Inline-highlight every case-insensitive occurrence of `query` in
/// `line` with bold bright yellow. The result is a plain String with
/// embedded SGR escapes; anstream strips them when stdout isn't a
/// tty.
fn highlight_match(line: &str, query: &str) -> String {
    let line_lc = line.to_lowercase();
    let query_lc = query.to_lowercase();
    let q_len = query.len();
    let mut out = String::with_capacity(line.len() + 16);
    let mut cursor = 0;
    while cursor < line.len() {
        match line_lc[cursor..].find(&query_lc) {
            Some(rel) => {
                let start = cursor + rel;
                let end = start + q_len;
                out.push_str(&line[cursor..start]);
                // Hand-rolled escape so this fn doesn't need to take
                // the anstyle Style consts; rustfmt + anstream
                // friendly.
                out.push_str("\x1b[1;93m");
                out.push_str(&line[start..end]);
                out.push_str("\x1b[0m");
                cursor = end;
            }
            None => {
                out.push_str(&line[cursor..]);
                break;
            }
        }
    }
    out
}

/// Disambiguation suffix shown next to a conversation title in
/// `/conv` / `/conv list`. Format: `(N msgs · 2h ago)`. When the
/// conversation has never been touched (legacy stores) the
/// timestamp piece is omitted.
fn conv_hint(c: &crate::store::Conversation) -> String {
    let count = c.user_turn_count();
    let plural = if count == 1 { "" } else { "s" };
    let age = match c.last_used {
        Some(ms) if ms > 0 => format!(" · {}", relative_time(ms)),
        _ => String::new(),
    };
    format!(
        "{C_META}({count} msg{plural}{age}){C_META:#}",
        plural = plural,
    )
}

fn relative_time(ts_ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if ts_ms == 0 || now <= ts_ms {
        return "now".into();
    }
    let diff_s = (now - ts_ms) / 1000;
    if diff_s < 60 {
        format!("{diff_s}s ago")
    } else if diff_s < 3600 {
        format!("{}m ago", diff_s / 60)
    } else if diff_s < 86_400 {
        format!("{}h ago", diff_s / 3600)
    } else {
        format!("{}d ago", diff_s / 86_400)
    }
}

fn basename(p: &str) -> String {
    std::path::Path::new(p)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(p)
        .to_string()
}

fn print_stats(s: &StatsLite) {
    let prompt = s
        .prompt_tokens
        .map(|n| n.to_string())
        .unwrap_or_else(|| "?".into());
    let secs = (s.duration_ms as f64) / 1000.0;
    let tps = if secs > 0.0 {
        (s.gen_tokens as f64) / secs
    } else {
        0.0
    };
    println!(
        "{stats}[ Prompt: {prompt} tok | Generation: {gen} tok @ {tps:.1} t/s ]{reset}",
        stats = C_TOOL,
        reset = C_RESET,
        gen = s.gen_tokens,
    );
}

async fn prompt_yes_no(name: &str, arguments: &str, preview: Option<&str>) -> bool {
    // When the tool provided a rendered preview, show it instead of
    // the raw arguments JSON: lines prefixed `+ ` go green, `- ` go
    // red, everything else stays default-fg. The y/N prompt sits
    // immediately under the preview so the user's eye lands on it
    // without scanning past the body.
    let body = match preview {
        Some(p) => colorize_diff(p),
        None => format!(
            "{dim}with args{reset} {arguments}",
            dim = C_DIM,
            reset = C_RESET
        ),
    };
    let prompt = format!(
        "{tool}approve tool{reset} {bold}{name}{reset}\n{body}\n{user}[y/N] > ",
        tool = C_TOOL,
        reset = C_RESET,
        bold = C_BOLD,
        user = C_USER,
    );
    let resp = tokio::task::spawn_blocking(move || {
        print!("{prompt}");
        anstream::stdout().flush().ok();
        let mut buf = String::new();
        let _ = io::stdin().lock().read_line(&mut buf);
        print!("{C_RESET}");
        anstream::stdout().flush().ok();
        buf
    })
    .await
    .unwrap_or_default();
    matches!(resp.trim().to_lowercase().as_str(), "y" | "yes")
}

/// Apply ANSI colour to a preview returned by `Tool::preview`. `+ `
/// lines become green, `- ` lines red, everything else stays in
/// default fg. Strictly line-oriented; doesn't try to render
/// intra-line diffs.
fn colorize_diff(preview: &str) -> String {
    let mut out = String::with_capacity(preview.len() + 64);
    for (i, line) in preview.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if let Some(rest) = line.strip_prefix("+ ") {
            out.push_str(&format!("{C_OK}+ {rest}{C_RESET}"));
        } else if let Some(rest) = line.strip_prefix("- ") {
            out.push_str(&format!("{C_ERR}- {rest}{C_RESET}"));
        } else {
            out.push_str(line);
        }
    }
    out
}
