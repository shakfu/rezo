# rezon

LLM client with interchangeable providers. Ships two shells over one
shared Rust backend:

- **`rezon`** — Tauri 2 + React 19 desktop app (`make dev` / `make
  build`).
- **`rezon-tui`** — sequential REPL chat for the terminal
  (`make run-tui`).

Providers:

- **Local**: [`llama-cpp-2`](https://crates.io/crates/llama-cpp-2)
  with Metal acceleration, loading any `.gguf` model from disk.
- **Cloud (OpenAI / Anthropic / OpenRouter)**:
  [`async-openai`](https://github.com/64bit/async-openai) client
  pointed at each provider's OpenAI-compatible endpoint.
- **Other**: same `async-openai` client with model + base URL + API
  key supplied at runtime. Targets any OpenAI-compatible server —
  Ollama, LM Studio, `llama.cpp` `server`, self-hosted gateways, etc.

### Cloud providers

| key          | env var              | base URL                         |
| ------------ | -------------------- | -------------------------------- |
| `openai`     | `OPENAI_API_KEY`     | `https://api.openai.com/v1`      |
| `anthropic`  | `ANTHROPIC_API_KEY`  | `https://api.anthropic.com/v1`   |
| `openrouter` | `OPENROUTER_API_KEY` | `https://openrouter.ai/api/v1`   |
| `other`      | (entered at runtime) | (entered at runtime)             |

Recommended-models lists for the named providers live in
`crates/rezon-core/models.json` (embedded into the binary via
`include_str!`).

## Layout

```
src/                          React + Vite frontend (consumed by rezon-web)
crates/
  rezon-core/                 Provider-agnostic backend (no Tauri deps)
    src/
      llm.rs                  Chat: local llama.cpp + cloud (async-openai).
                              `ChatSink` trait abstracts the event surface.
      embed.rs                Embedding model worker + catch-up loop.
      search.rs               FTS5 + sqlite-vec per-vault index, file watcher.
      vault.rs                Vault-scoped filesystem ops; containment
                              checked on resolved paths.
      journal.rs              Append-only vault write log; undo / redo.
      wikilink.rs             `[[target]]` parsing + expansion.
      agent/                  Provider-agnostic agent loop + tools.
        cloud.rs              Cloud `Provider` impl.
        local.rs              Local `Provider` impl (owns `Arc<LlmState>`).
        loop_.rs              `run_agent` — streaming, tool dispatch.
        testing.rs            Test doubles for the loop (cfg(test) only).
        tools/                See "Agent tools" below.
        confirm.rs            `ConfirmationGate` trait.
        event.rs              `EventSink` trait + `LogEventSink`.
    models.json               Cloud provider catalog.
  rezon-web/                  Tauri shell — thin wrapper over rezon-core
    src/
      lib.rs                  Builder, command registration, auto-load.
      llm.rs                  `#[tauri::command]` wrappers + `TauriChatSink`.
      embed.rs                Embed lifecycle + event emission.
      search.rs               Search commands.
      vault.rs                Vault commands.
      agent/
        commands.rs           `agent_chat` / `cancel_agent` / `confirm_tool_call`.
        tauri_sink.rs         `EventSink` -> `app.emit("agent-*", …)`.
        tauri_gate.rs         `ConfirmationGate` -> frontend prompt.
    tauri.conf.json           `frontendDist: "../../dist"`.
  rezon-tui/                  Terminal REPL — also thin
    src/
      repl.rs                 Slash-command dispatcher + streaming loop.
      sink.rs                 `TuiChatSink`, `TuiAgentSink`,
                              `TuiConfirmationGate`.
      agent.rs                Builds the agent registry; spawns runs.
      vault.rs                `VaultCtx` (Arc<SearchState> + Arc<EmbedState>).
      input.rs                rustyline editor + tab completion.
      picker.rs               nucleo + crossterm fuzzy picker.
      markdown.rs             Inline markdown -> ANSI renderer.
      conv_index.rs           FTS5 index over conversation history.
      setup.rs                First-run provider/model setup flow.
      spinner.rs              Braille spinner for blocking loads.
      store.rs                Conversations / vault / disabled-tools JSON.
```

The Cargo workspace root is `Cargo.toml`; `make check` / `make test`
cover all three crates.

## Agent tools

Registered in `crates/rezon-core/src/agent/tools/`. The **Confirm**
column is each tool's own `requires_confirmation()`.

| tool | confirm | what it does |
| --- | --- | --- |
| `current_time` | no | Current local time. |
| `file_read` | yes | Read a regular file by absolute path. Capped at 256 KiB; directories and non-regular files (FIFOs, devices) are refused. |
| `web_fetch` | yes | HTTP(S) GET. 15s timeout, 1 MiB body cap applied while reading. Redirects followed only within the same host; a cross-host hop is reported, not chased. |
| `shell_exec` | yes | Run a command via `$SHELL -c`. 60s timeout, process-group kill on overrun, 256 KiB per stream. |
| `search_notes` | no | FTS5 / semantic search over the open vault. |
| `read_note` | no | Read a note by vault-relative path. |
| `write_note` | yes | Create or overwrite a note. |
| `append_note` | yes | Append to a note. |
| `edit_note` | yes | Search-and-replace within a note. |
| `undo_note` | yes | Revert the most recent vault write via the journal. |

The vault tools (`search_notes` through `undo_note`) need an open
vault. Every write goes through `journal.rs`, so `undo_note` and the
history panel can reconstruct what changed.

### Confirmation policy

Tools that declare `requires_confirmation()` always prompt before
dispatch. This is enforced in the backend gate, not in the UI: the
per-tool "Ask / Always / Disable" setting cannot mark a side-effecting
tool as auto-approved. To stop being asked for one, use **Always
allow** in the confirmation dialog — that records a grant backend-side
which lasts until rezon restarts and is not written to disk. `Disable`
is always honoured, since it only removes capability.

`rezon-tui` applies the same floor. It has no "always" affordance, so
side-effecting tools prompt on every call there.

## Quick start

### GUI (rezon-web)

```sh
make install      # bun install (frontend deps)
make dev          # bun run tauri dev --config crates/rezon-web/tauri.conf.json
```

Enter an API key in the right sidebar for whichever cloud provider is
selected; it is saved to the OS keychain. See **API keys** below for
the full resolution order, including the environment-variable path.

### TUI (rezon-tui)

```sh
make run-tui ARGS="--provider openrouter --model anthropic/claude-haiku-4-5"
make run-tui ARGS="--provider local --gguf /path/to/model.gguf"
make run-tui-release ARGS="--agent --provider openrouter \
                            --model openai/gpt-5.4-mini"
```

Inside the REPL, `/help` lists slash commands. Conversations,
disabled-tools, last vault, and command history persist under
`~/Library/Application Support/com.rezon.rezon-tui/`.

Key features:

- Streaming responses with per-turn token stats.
- Multiple conversations (`/conv`, `/new`, `/rename`, `/delete`,
  `/next`, `/prev`).
- Fuzzy picker over conversations and search results (`/conv`,
  `/search`).
- Agent mode (`/agent`) with `current_time`, `file_read`, `shell_exec`,
  `web_fetch`, and `search_notes` (when a vault is open). Tool calls
  show inline; confirmation-required tools prompt `[y/N]`.
- Tool turns persist across restarts so the agent sees its own prior
  calls.
- Vault integration: `/vault <path>`, `/note <path>`, `/find <query>`.
  `/embed <gguf>` loads a separate embedding model; semantic search
  takes over once embeddings are caught up.
- Markdown rendering on assistant responses (raw stream during, then
  re-rendered as bold/italic/code/headings/lists once complete).
- rustyline editing: ↑/↓ history, Ctrl-R reverse search, tab
  completion for slash commands + filesystem paths.

## Architecture notes

### `ChatSink` / `EventSink` / `ConfirmationGate`

The boundaries between core and the two shells are three traits in
`rezon-core`:

- `ChatSink` — token / stats / done callbacks for the non-tool chat
  path. `TauriChatSink` forwards to `app.emit("chat-token"|"chat-stats"
  |"chat-done", …)`; `TuiChatSink` pushes into an mpsc the REPL
  drains.
- `EventSink` (agent path) — token, tool start/end, stats, done,
  cancelled, error.
- `ConfirmationGate` — async `ask(call) -> Approved | Denied`.
  `TauriConfirmationGate` round-trips through a `confirm_tool_call`
  command; `TuiConfirmationGate` writes a `UiEvent::Confirm` and
  blocks on a oneshot the REPL fulfils after reading y/n from stdin.

Frontend event names and payloads are unchanged from before the
workspace split — no migration needed for the existing React UI.

### Local model teardown

`LlmState::shutdown` joins the per-model worker thread before
dropping the `Arc<LlamaBackend>`. Without this, ggml-metal's
process-exit destructor (run from `__cxa_finalize` after `main`
returns) trips `GGML_ASSERT([rsets->data count] == 0)` because the
KV-cache buffers are still alive when the metal device is torn down.

Each loaded local model gets a dedicated worker holding the
`LlamaContext` and reusing the KV cache across turns: only the
divergent suffix of each new prompt is decoded.

### Defaults

Hard-coded in `crates/rezon-core/src/llm.rs`:

- `N_CTX = 4096`
- `MAX_NEW_TOKENS = 1024`
- `N_GPU_LAYERS = 999` (offload everything; Metal feature enabled)
- Sampler chain: `temp(0.7)` then `dist(1234)` (fixed seed)

The model's own chat template (`model.chat_template(None)`) is used to
format the prompt; models without embedded chat-template metadata
cannot chat.

## Develop

```
make install            bun install (frontend deps)
make dev                Tauri GUI dev mode
make build              Tauri GUI release build
make web-dev            Vite only, no Tauri shell
make web-build          Frontend only

make build-tui          rezon-tui (debug)
make build-tui-release  rezon-tui (release)
make run-tui ARGS="…"   Run rezon-tui (debug), forwarding ARGS to the binary
make run-tui-release ARGS="…"

make check              cargo check --workspace
make fmt                cargo fmt --all
make fmt-check          cargo fmt --all -- --check
make lint               cargo clippy --workspace --all-targets -- -D warnings
make test               cargo test --workspace
make clean              rm node_modules dist target …
```

## Requirements

- macOS with Metal for local models (other platforms untested for
  Metal; the cloud providers and `rezon-tui` should build anywhere
  Rust + Tauri prerequisites are available).
- Rust toolchain.
- Bun (frontend deps + Tauri CLI). Only needed for the GUI; `rezon-tui`
  builds with `cargo` alone.
- Tauri 2 prerequisites for the GUI.
- For the local backend: a GGUF model file with chat-template metadata.
- For named cloud backends: an API key, supplied by any of the routes
  in **API keys** below.

## API keys

A key is resolved per request, from the first source that has one:

1. **Runtime** — `rezon-tui --api-key ...`, or the key field for the
   **Other** provider.
2. **OS keychain** — macOS Keychain, Windows Credential Manager,
   secret-service on Linux. Written from the right sidebar's key field,
   under the account `api_key:<provider>`. Shared by both shells.
3. **Environment** — the provider's `env var` from the table above,
   including anything loaded from a `.env` (see below).

Resolution lives in `rezon_core::llm::lookup_api_key`, and the ordering
is deliberate. Runtime wins because it is the most explicit thing a
user can do. The keychain sits ahead of the environment because it is
the only source a *packaged* GUI can read at all: an app launched from
Finder, the dock, or a `.desktop` entry never runs a shell profile, so
nothing exported in `~/.zshrc` is visible to it. The environment stays
last and fully supported — it is how terminal launches, `make dev`, and
CI supply keys.

Keys are write-only from the frontend's perspective. There is no Tauri
command that returns one; the UI can save a key and ask whether one is
configured (`keychain_has`), but never reads the value back, and no key
travels on a chat request payload.

### `.env`

Loaded at startup by both shells, from two optional locations:

- `.env` found by walking up from the current directory — the
  development case.
- `<app config dir>/.env` — the packaged case, where the cwd is
  unpredictable. That is
  `~/Library/Application Support/rezon-tui/.env` on macOS,
  `%APPDATA%\rezon-tui\.env` on Windows,
  `$XDG_CONFIG_HOME/rezon-tui/.env` (or `~/.config/rezon-tui/.env`) on
  Linux.

Variables already present in the environment win over both, so an
explicit `export` always overrides a stale `.env`. Add `.env` to your
global gitignore if you keep one in a checkout.

See `CHANGELOG.md` for history.
