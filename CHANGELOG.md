# Changelog

All notable changes to this project. Format loosely follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Security
- **Confirmation floor.** `Tool::requires_confirmation()` was never consulted; policy came entirely from a frontend-supplied map, so anything able to call `agent_chat` could auto-approve `shell_exec`. The rule now lives in `agent::confirm::decide` and only a backend-recorded grant clears the floor. `"disable"` still honoured at face value. 12-row decision table under test.

- **Vault symlink escape.** `vault::within` compared lexically, so a symlink inside the vault pointing out of it passed and `fs::write` followed. `resolve_path` walks the path resolving symlinks as it goes — forwards, so `..` after a link lands where the kernel puts it, not where string-cancellation would. Root canonicalized too (a macOS home is usually itself a symlink). Links staying inside still work.

- **CSP set** (was `null`): `default-src 'self'`, `object-src 'none'`, `frame-ancestors 'none'`, IPC-only `connect-src`. `style-src` keeps `'unsafe-inline'` for React/Tailwind; `font-src` allows `data:` because KaTeX inlines a face and blocks math without it.

- **Keys write-only from the frontend**: `keychain_get` → `keychain_has`, no `apiKey` on the chat payload, no plaintext key in the webview.

- **`web_fetch` stops at cross-host redirects**, returning the 3xx with `redirectedTo`. Same-host hops still follow. Chasing them fetched a destination the user never approved.

### Added
- **Agent test doubles** (`agent/testing.rs`, `cfg(test)`): `ScriptedProvider`, `RecordingSink`, `ScriptedGate`, `FakeTool` — drive `run_agent` with no model, network, or GGUF.

- **`.env` loading** (`core/dotenv.rs`), both shells: cwd-upward, then `<app config dir>/.env` for packaged builds. Exported variables win over both.

- **`rezon_core::secrets`** — credential store moved out of the Tauri shell so `rezon-tui` shares it; a `SecretStore` trait keeps key resolution testable without the real keychain.

- **`llm::lookup_api_key` + `KeySource`** — runtime → keychain → environment.

- **`grant_tool_always` / `revoke_tool_always` / `tool_always_grants`** commands, and "Always allow" in the confirm dialog. Grants are process-local, not persisted.

- **API-key field for named providers** in the sidebar, replacing a `<ENV_VAR> not set` notice that offered no way to act on it.

- **CI** (`.github/workflows/ci.yml`); `make typecheck` and `make ci`.

- **Tests: 112 → 218**, covering `run_agent`, `cloud.rs`, `shell_exec` (including the orphan kill `process_group(0)` exists for), `web_fetch`, `file_read`, vault containment, gate policy, and key resolution.

- **README**: agent-tools table, confirmation policy, API keys.

### Changed
- **`resolve_cloud_config` deduplicated** — `rezon-web` carried a byte-identical copy. Core's now takes `impl Into<CloudConfigInput<'a>>`, so call sites are unchanged.

- **Conversation persistence debounced** (400 ms). It ran per streamed token, each one a `JSON.stringify` over the whole conversation set plus a synchronous write. Flushes at end-of-turn, `beforeunload`, unmount.

- **`web_fetch` caps during the read** (`bytes_stream`, `MAX_BYTES + 1` so the boundary is decidable); URL parsed rather than prefix-matched; client built once.

- **`file_read` bounds the read** and refuses directories and non-regular files — a FIFO previously hung forever. `size` is now bytes returned; true length moved to `fileSize`.

- **`rezon-tui` honours per-tool policy** — it prompted for every call, `current_time` included. Same `decide` table as the GUI, and it gained the `Disable` handling it never had.

- **`[workspace.package]`** is the single source for version, edition, license, repository; all five version strings now read 0.1.1.

- **`make test` includes `fmt --check`** — which surfaced 60 hunks of pre-existing drift, cleared in a separate mechanical pass.

- **`shell_exec` timeout is a field** (`Default` = 60s) so the overrun path is testable in milliseconds.

### Fixed
- **`make test` was red on `main`** — two `useless_borrows_in_formatting` errors, one in each copy of the duplicated resolver.

- **Mid-stream provider errors discarded the partial turn.** `run_agent` bailed with `?`, dropping content already on screen and leaving `messages` without the assistant turn, so the next request disagreed with the display. Now assembles the partial turn, emits `AgentEvent::Error`, then returns.

- **Named-provider API keys were unusable in a packaged build.** Resolution read only the env var, and a GUI launched from Finder or the dock never runs a shell profile — so the key was invisible and the sidebar offered no input. Keychain now precedes environment; environment still serves terminal launches, `make dev`, and CI.

- **`LlmState` poisoning bricked local inference** — one panic under a guard made every later call panic. `lock_recover` recovers via `into_inner()`. Shutdown used `if let Ok(..)`, skipping teardown precisely when the ggml-metal drop ordering still had to happen.

- **Vite watched the Rust tree.** The scaffold's `src-tauri` ignore path does not exist here, so `crates/` and `target/` were watched.

- **Vault containment errors** now name the resolved path, not just the literal one — which read as nonsense for a symlink.

### Removed
- Unreachable agent scaffolding: `ToolError::Denied`, `ToolError::Cancelled`, `ToolResult`, `AgentEvent::ToolConfirm`, `ToolContext.workdir`. None caught by `-D warnings`, all being `pub`.

- `keychain_get`, replaced by `keychain_has`.

### Added
- **Vault redo** (`crates/rezon-core/src/journal.rs::redo_last_op`). Reapplies the most-recent `Op::Undo`: restores its `before_sha` content (the disk state immediately before the undo ran) and records a fresh `Op::Write` tagged `tool: "redo"`. No new `Op` variant — a tagged write keeps the chain linear, so `undo→redo→ undo→redo` cycles cleanly and a fresh write between undo and redo invalidates the redo stack (text-editor convention). Surface in all three shells: `/redo` TUI command, `vault_redo` Tauri command returning `RedoReport { path, targetUndoId, wasCreation }`, `↷ Redo` button in the NotesView editor toolbar. Three new tests cover round-trip, intervening-write invalidation, and the deletion-undo redo path.

- **Vault history panel** (`src/notes/JournalPanel.tsx`). Modal dialog backed by a new `vault_journal_recent(vault, limit?)` Tauri command (default 100 rows, clamped 1..=1000) over
  `journal::recent_entries`. Renders `When | Kind | Tool | Path`
  newest-first; `write` rows tinted green, `undo` rows red to match the diff-preview palette. Same-day timestamps as `HH:MM`, older as `YYYY-MM-DD HH:MM`. Triggered from a new `History` button in NotesView's editor toolbar. Read-only — undo / redo remain the action surface for v1; per-row "revert from here" is a future addition once multi-step undo lands in core.

- **`JournalEntryDto`** — frontend-stable shape over the core's `#[serde(tag="op")]` enum. Flattens into a camelCase
  `{ id, ts, tool, path, kind: "write"|"undo", beforeSha,
  afterSha, targetId? }` so the frontend never pattern-matches on serde tags.

- **`read_note(path)` agent tool** (`crates/rezon-core/src/agent/tools/read_note.rs`). Read-only, no confirmation. Returns `{ vault, path, content }` for a note at the given vault-relative path. Pairs with `search_notes` (which returns snippets, not full bodies) to close the agent's read loop: search → read → write/append/edit. Registered together with `search_notes` in `register_search_notes` — both depend on `SearchState::active_vault`, both are read-only. Two tests.

- **Wikilink expansion in chat + system prompts** (`crates/rezon-core/src/wikilink.rs`).
  Read-only `[[Target]]` / `[[Folder/Target]]` / `[[Target|Alias]]` markers
  in user messages and the system prompt resolve against the active vault via `vault_resolve_wikilink`, and the resolved note bodies are appended to the message as a `<context>\n## <rel path>\n…\n</context>` block. Stored conversation keeps the raw markers (so `/history` reads naturally and edits re-resolve next turn); expansion happens only at the send boundary and only on the system + most-recent user turn so prompt caching survives prior turns. Unresolved targets surface as `UiEvent::Error` in the TUI and a `chat-warning` Tauri event for the frontend. Eight unit tests (`scan_*`, `expand_*`).

- **Vault-write agent tools** (`crates/rezon-core/src/agent/tools/write_note.rs`): `write_note(path, content, overwrite=false)`, `append_note(path, content, create_if_missing=false)`, `edit_note(path, find, replace)`, `undo_note()`. All four gate on user confirmation, path-normalize (strip leading `/`, reject `..`, auto-append `.md`), call `vault_index_touch` so changes are searchable immediately, and report the journal outcome (entry id + git_committed + warning?) in their return payload. `edit_note`'s `find` must match exactly once — 0 or N≥2 matches error with a hint to pass a longer / more specific snippet. Registered together via `register_write_note(reg, search)` alongside `search_notes` when a vault is open.

- **`Tool::preview` method** (`crates/rezon-core/src/agent/tool.rs`). Optional `fn preview(&self, args: &Value) -> Option<String>` returning a diff-shaped string (`+ ` add lines, `- ` remove lines, anything else context). Agent loop computes the preview once per call from the registry + parsed args, then passes `Option<&str>` into `ConfirmationGate::ask`. TUI prompt and Tauri `agent-tool-confirm` event both surface it; UIs render the preview in place of raw arguments JSON when present. `WriteNote`/`AppendNote`/`EditNote`/ `UndoNote` all override it.

- **Edit journal + git versioning** (`crates/rezon-core/src/journal.rs`). Append-only `<vault>/.rezon-history/log.jsonl` records every mutation (write / undo) with sha256 before+after pointers; content snapshots live deduped in `<vault>/.rezon-history/blobs/<sha>`. After each successful record, opportunistic `git add <file> .gitignore && git commit -q -m "rezon: <tool> <rel_path>"` against the vault's git repo (auto-`git init`'d if missing). `.rezon-history/` is added to `.gitignore` on first write so the journal stays out of the user's git log. Hook + signing config respected; failures are non-fatal and surface via `JournalOutcome::git_warning`. `last_undoable(vault)` walks the log skipping any entry already targeted by a subsequent `Op::Undo`. Eight unit tests covering record, dedup, gitignore idempotency, GC, and skip-git.

- **`.rezon-skip-git` sentinel** — touch `<vault>/.rezon-skip-git` to opt this vault out of the git auto-init/auto-commit (journal still records). Useful for vaults nested inside an outer repo where rezon shouldn't create a second one. Silent skip — no warning when the sentinel is present.

- **Journal GC** — `journal::gc(vault_root, max_entries)` keeps the most-recent N entries (default 500), atomically rewrites `log.jsonl` via temp+rename, then prunes any blob no surviving entry references. Triggered automatically at the end of every `record_write` / `record_undo`; failures are non-fatal.

- **`undo_note` agent tool + `/undo` TUI command + `vault_undo` Tauri command**. All three revert the most-recent journaled mutation: `undo_note` is agent-callable (confirmation-gated, with a diff preview); `/undo` is the user-direct entry point in the REPL; `vault_undo` is exposed to the frontend (returns `{ path, targetId }` on success). Each appends a new `Op::Undo` entry so the chain is walkable.

- **First-launch setup wizard** (`crates/rezon-tui/src/setup.rs`) and `/setup` command. On first launch (or whenever required fields are missing in `conversations.json`), prompts via rustyline for: models dir, vault dir, output dir, and default provider. Each prompt pre-fills the current value as editable text (`readline_with_initial`) — `<enter>` accepts, edit-in-place changes, empty input clears, ctrl-c aborts. Path prompts wire in `FilenameCompleter`; provider prompt completes from the static word list (local / openai / anthropic / openrouter / other). Wizard fires when `setup_complete: bool` is false; `/setup` re-runs on demand. Defaults follow env vars (`REZON_{MODELS,VAULT,OUTPUT}_DIR`, `REZON_PROVIDER`) when set, otherwise fall back to `~/Documents/Rezon/{models,vault,exports}` for the three path fields. Persisted in `StoreFile` as `setup_complete`, `models_dir`, `output_dir`, `default_provider` (all `#[serde(default)]` so upgrades are silent).

- **Slash-command pickers**. `/provider` and `/model` (no args) open the embedded fuzzy picker. `/provider` lists `local` + every cloud provider key from `models.json` with the current marked `*`; selecting clears the per-conv model override so the new provider's default kicks in. `/model` lists the current provider's recommended models for cloud, or scans the configured models dir for `*.gguf` files when provider is `local` — the picker load-and-set on selection via `spinner::with_spinner`. `/models` (no args) defaults to listing the current provider's catalog rather than first asking which provider.

- **`/export` honors `output_dir`** — bare filename goes there; empty arg auto-names from the conversation title under the configured output dir. Absolute or slash-containing paths still pass through. Parent dirs created as needed.

- **`last_local_model` auto-load**. On startup, when provider is `local` and neither `--gguf` nor `--model <path>` was passed, the TUI falls back to `rezon_core::llm::read_last_model(<config_dir>)` (the same helper the web app already used). Every successful local load — at startup, via `/load`, or via the `/model` picker — calls `persist_last_model` so the next launch resumes the same GGUF.

- **Diff-preview rendering in the confirmation surface**. The TUI's `prompt_yes_no` runs the preview through `colorize_diff` (`+ ` lines green, `- ` red) and lets the y/N prompt sit right below. Frontend (`src/App.tsx`) gains a `DiffPreview` component that renders the same convention in the `ConfirmToolDialog` modal — `bg-success/10 text-success` for `+ `, `bg-danger/10 text-danger` for `- `, monospace block matching the existing argument-render styling. New `--color-success` / `--color-warning` CSS variables (dark + light), surfaced as Tailwind tokens.

- **`chat-warning` banner in the chat tab**. `src/App.tsx` listens for the Tauri event emitted by `crates/rezon-web/src/llm.rs::chat` (and `agent::commands::agent_chat`) when wikilink expansion finds unresolvable targets; renders as a dismissable yellow banner under the message list, clears automatically when the next message is sent.

- **`↶ Undo` button in NotesView** (`src/notes/NotesView.tsx`). Sits in the editor tab strip; flushes any pending debounced saves before invoking `vault_undo`, then refreshes the affected tab's `diskContent` + `liveContent` (or closes the tab if the file was deleted by the revert). Refreshes the file tree to pick up creates/deletes from the undo.

- **`vault-warning` toast in NotesView**. Tauri `vault_write` / `vault_undo` now emit `vault-warning` when the journal returns a git warning (e.g. pre-commit hook rejected the auto-commit) or fails outright. Frontend listens via `useEffect` and surfaces a dismissable warning-colored banner below the file editor's error banner.

- **Tests**:

  - `rezon-core` +12: `wikilink::scan_*`, `wikilink::expand_*`, `write_note::normalize_rel_*`, `write_note::render_preview_*`, `write_note::*_preview_*`, `journal::record_write_*`, `journal::ensure_gitignore_is_idempotent`, `journal::blob_dedup_on_identical_content`, `journal::last_undoable_*`, `journal::gc_*`, `journal::skip_git_sentinel_suppresses_commit`. Total: 53.

### Changed
- **Undo consolidated into a single core primitive** (`crates/rezon-core/src/journal.rs::undo_last_op`). Returns `Option<UndoOutcome { target_id, tool, path, was_deletion, journal }>`; `None` when there's nothing to undo (was an `Err` string in three different callers before). The TUI `cmd_undo` (86 → 36 lines), Tauri `vault_undo` (44 → 13), and agent `UndoNote::dispatch` (58 → 24) all delegate. Net ~−180 lines of call-site duplication, +96 lines in the core helper. Side fix: the TUI's old inline `cmd_undo` was the only caller not running `vault_index_touch` after the revert; the consolidated path now does, so search results stay consistent across all three undo entry points. The agent tool's JSON return gained `reverted_tool` and `was_deletion` fields in the process.

- **`make test` runs clippy** (`-D warnings`) after the unit-test pass. Folded the previously-separate `lint` target's check into `test` so every test run gates lint regressions too. Caught during the rollout: `doc_list_item_without_indentation` in `journal::Op` and `repl::colorize_diff` (doc prose rewritten), `needless_range_loop` ×2 in `llm::run_*_with_cache`'s chunked decode (switched to `for (j, tok) in to_add[i..end].iter(). enumerate()`), `collapsible_match` in `expand_agent_messages`. All fixed; lint gate green.

- **Full vault-mutation journaling** in the Tauri surface (`crates/rezon-web/src/vault.rs`). `vault_create` / `vault_delete` / `vault_rename` now write to the journal alongside `vault_write`:

  - `vault_create` → `Op::Write { before: None, after: Some(b"") }`

  - `vault_delete` (file) → `Op::Write { before: Some(content), after: None }`. Pre-image captured before the unlink fires.

  - `vault_delete` (dir) → walks the tree first via the new `snapshot_dir`, snapshots every regular file's bytes, then `record_write` per-file after `remove_dir_all`. `/undo` walks these back one at a time.

  - `vault_rename` (file) → two journal entries: delete on `from`, create on `to`. Two undos reverse the rename; the chain is intact.

  - `vault_mkdir` left un-journaled (empty directories have no content; they surface in git as soon as a file lands inside). Shared `relativize(vault, path)` helper replaces the inline strip-prefix that lived in `vault_write` only. All four mutators emit `vault-warning` Tauri events on git-hook / journal failures.

- **Stale direct deps dropped from `crates/rezon-web/Cargo.toml`**. `futures`, `thiserror`, `reqwest`, `rusqlite`, `sqlite-vec`, `notify`, `sha2`, `libc` removed entirely (transitive through `rezon-core`). `llama-cpp-2`, `encoding_rs`, `async-openai` moved to `[dev-dependencies]` (only `examples/` programs use them directly). Production build graph shrank to just `rezon-core` + the Tauri stack + `async-trait` (still used by the Tauri-side `ConfirmationGate` impl).

- **`ConfirmToolDialog` + `DiffPreview` extracted from `App.tsx`** into a new `src/ConfirmToolDialog.tsx`. App.tsx went from 1,156 to 1,003 lines (−153). Public exports: `ConfirmToolDialog`, `DiffPreview`, `PendingConfirm` type. App.tsx now imports the type and uses it for both the `useState` slot and the `agent-tool-confirm` listener payload — no more inline-shape duplication.

- **Dead Phase-2 stub removed from `agent::loop_`**. Deleted the unreachable `if tool.requires_confirmation() { eprintln!("phase-2 dispatches without prompting") }` block in `dispatch_one` (the gate runs in the main loop before this point) and rewrote the file header to describe the actual contract instead of the Phase-2 caveats.

- **Fresh conversation per launch**. After `Store::load_or_new`, if the active conversation has any user turns, the TUI pushes a new empty conversation and switches to it; the previous one stays in `store.conversations` and is reachable via `/conv`. Persisted immediately so a crash before the first turn doesn't re-resume the old conversation. Skip when the active conv is already empty so consecutive launches don't pile up blanks.

- **llama.cpp / ggml log noise muted**. `LlamaBackend::void_logs()` is called immediately after `LlamaBackend::init()` in `crates/rezon-core/src/llm.rs::ensure_backend`. The global no-op callback suppresses `ggml_metal_*`, `llama_model_loader:`, `print_info:`, `load_tensors:`, `llama_context:`, `llama_kv_cache:`, and `ggml_metal_library_compile_pipeline:` output. The TUI also emits `\x1b[2J\x1b[3J\x1b[H` immediately before `print_banner()` to wipe any stderr that beat `void_logs`, gated on `IsTerminal`.

- **Provider-precedence rule at TUI startup**. If `--provider` was left at the CLI default (`openrouter`), the stored `default_provider` from the setup wizard wins; an explicitly passed `--provider X` still overrides for the session. Local-model auto-load now also checks `cli.gguf`, then `cli.model` (if it ends in `.gguf`), then `read_last_model(<config_dir>)`.

- **`ConfirmationGate::ask` signature**. `async fn ask(&self, call: &ToolCall, preview: Option<&str>) -> ConfirmationOutcome`. `AutoApproveGate`, `TuiConfirmationGate`, `TauriConfirmationGate` updated; `UiEvent::Confirm` carries `preview: Option<String>` and `agent-tool-confirm` Tauri event carries `preview?: string`.

- **`StoreFile` schema**. New fields, all `#[serde(default)]` so existing stores migrate silently: `setup_complete: bool`, `models_dir: Option<String>`, `output_dir: Option<String>`, `default_provider: Option<String>`.

- **`store::config_dir()` is now `pub`** so the wizard and the agent-tool path can compute defaults relative to the same dir `Store` uses.

### Fixed
- **Crash on first-turn chat with a long conversation history** (`GGML_ASSERT(n_tokens_all <= cparams.n_batch) failed`). `crates/rezon-core/src/llm.rs::run_chat_with_cache` and `run_agent_with_cache` previously called `ctx.decode(&mut batch)` with the entire `to_add` slice in one go. With `n_batch = 2048`, any rendered prompt over ~2048 tokens (trivial for a 19-message history) tripped the assert and aborted the process. Both paths now chunk `to_add` into `ctx.n_batch()`-sized slices and decode each in turn; the `logits=true` flag is set only on the prompt's final token of the final chunk.

- **`/models` no longer asks for provider first** when called with no argument — defaults to the current effective provider (per the user's mental model of "show models for the thing I'm using now"). Explicit `/models <key>` still works for browsing a different provider's catalog.

- **Hard error on `--provider local` without a GGUF path removed**. Startup-load failures are now soft: the REPL boots regardless, with a `note: provider is local but no model loaded — use /model to pick one` message. The user can pick via the `/model` picker and the model auto-loads.

- **`rezon-tui` crate** — sequential REPL chat shell over `rezon-core`, shipped as the `rezon-tui` binary. Uses plain stdin/stdout (no alternate-screen takeover) so terminal scrollback / copy-paste / piping all work. clap CLI mirrors the GUI's provider/model knobs: `--provider {local,openai,anthropic,openrouter,other}`, `--model`, `--gguf`, `--base-url`, `--api-key`, `--system`, `--agent`, `--max-steps`.

- **Slash-command surface** in `rezon-tui`: `/help`, `/exit`, `/new`, `/conv [n]`, `/next`, `/prev`, `/rename`, `/delete`, `/agent`, `/chat`, `/model`, `/provider`, `/max-steps`, `/system [text]`, `/load <gguf>`, `/history`, `/search <query>` (full-text search across all conversations, newest first, with cap), `/tools` /
  `/tools enable|disable <name>` (runtime tool gate; persists),
  `/clear`, `/vault [path|close]`, `/note <path>`, `/find <query>`
  (semantic when an embed model is loaded, FTS5 otherwise), `/embed [<gguf>]`.

- **Conversation persistence**: rezon-tui stores all conversations, the active conversation id, the active vault path, and disabled tools as JSON under `<ProjectDirs::config_dir>/conversations.json` (macOS: `~/Library/Application Support/com.rezon.rezon-tui/`).

- **rustyline line editor** with persistent history at `<config_dir>/history.txt` — arrow keys, Home/End, Ctrl-A/E, Ctrl-W, Ctrl-U/K, Ctrl-R reverse-search, ↑/↓ history. Tab completion for slash commands and (for path-taking verbs `/load`, `/embed`, `/vault`, `/note`) for filesystem paths.

- **Streaming UX**: tokens stream straight to stdout as they arrive. Cancellation via Ctrl-C during a stream (flips `LlmState`/`AgentRunHandle` cancel flags); second Ctrl-C force-exits. Ctrl-D on an empty line exits cleanly. Per-turn stats printed in magenta after each response:
  `[ Prompt: N tok | Generation: N tok @ N.N t/s ]`.

- **Agent mode in the TUI**: `--agent` / `/agent`. Tool calls render inline as `→ name` then `✓ name: {result}` or `✗ name: error`. Tool confirmations prompt inline (`approve tool X with args … [y/N] >`) and block the agent loop on a oneshot. `search_notes` registers automatically when a vault is open.

- **Agent tool turns persist for replay**: `core::llm::ChatMsg` extended with optional `tool_calls` + `tool_call_id` (both `#[serde(default)]`, backward-compatible with existing stores). After each agent run the spawn block snapshots the full `Vec<ChatMessage>` via `UiEvent::AgentHistory` and the REPL replaces the conversation's messages with the structured form, so subsequent agent runs (including across restarts) see the model's prior tool selections + results.

- **Vault + embeddings in the TUI**: opens via `/vault <path>`; auto-opened on next launch via the persisted `active_vault`. `/find` uses semantic search when an embed model is loaded, FTS5 otherwise. `/embed <gguf>` loads a separate embedding model; catch-up loop indexes new chunks in the background.

- **Spinner** for long-running blocking loads (`/load`, `/embed`, startup `--gguf`). Braille frames at 80 ms via a tokio task; cursor hidden during spin, restored on stop; suppressed on non-tty.

- **Cross-platform color**: `anstyle` for style values, `anstream` for output. SGR codes stripped automatically when stdout isn't a
  tty (`rezon-tui --help | cat`); translated to Win32 console API
  calls on legacy Windows; `NO_COLOR` honored.

- **Makefile targets**: `build-tui`, `build-tui-release`, `run-tui ARGS="…"`, `run-tui-release ARGS="…"`.

- **Markdown rendering** for assistant responses in chat mode. Tokens still stream raw for live feedback; on stream end the REPL counts the rows the raw text occupied (terminal-width-aware), scrolls the cursor back with `\x1b[<n>A\r\x1b[J`, and re-emits the formatted version in place. Hand-rolled renderer (no extra deps) handles `**bold**`, `*italic*`, `` `inline code` ``, `#`/`##`/`###` headings, `-`/`*`/`1.` lists, `> blockquotes`, and triple-backtick fenced code blocks (dimmed, 2-col indent, optional language tag noted as `┄ lang`). Skipped when stdout isn't a tty (piped output keeps raw markdown) and in agent mode (where inline tool pills would be clobbered by the re-render).

- **Embedded fuzzy picker** (`crossterm` + `nucleo-matcher`, no alt-screen takeover). Renders below the current cursor, scrolls selection into view, cleans up on exit. Bound to:

  - `/conv` — fuzzy pick over conversation titles (the old plain listing moved to `/conv list`).

  - `/search [query]` — picker over every non-system / non-tool message across all conversations. Optional argument pre-seeds the filter. Enter switches to the picked conversation and prints the matched message in context.

  - `/tools enable` / `/tools disable` with no name argument — picker over the disabled / enabled subset respectively. Keys: typing filters, `↑/↓` move, `Enter` selects, `Esc` / `Ctrl-C` cancel.

- **Tests** — 58 across the workspace, `make test` green.

  - `rezon-core` (25): vault file ops + path-traversal rejection, `vault_resolve_wikilink` modes, list-tree filtering/sorting, `chunk_markdown` paragraph splitting + char-offset coverage, cloud catalog shape + `resolve_cloud_config` paths, `persist/read_last_model` roundtrip, `to_openai_messages` tool-role filtering + unknown-role rejection, `ChatMsg` / `ChatMessage` serde with optional tool_calls + tool_call_id, `ToolRegistry::register`/`get`/`without`/`openai_schemas`. `tempfile` added as a dev-dependency.

  - `rezon-tui` (33): markdown renderer (bold / italic edge-case guard / inline code / headings / lists / blockquotes / fenced code) + `count_rows` wrap-aware row math, `picker::truncate` incl. multi-byte chars + max ∈ {0,1}, `Conversation` auto-titling, `Store` save/reload roundtrip via on-disk JSON, `delete_active` index clamping, `chat_messages_to_msgs` role-preserving conversion, `build_agent_messages` replay (carries `tool_calls` on assistant turns + `tool_call_id` on tool turns; drops orphan tool turns).

- **Per-conversation settings** in `rezon-tui`. Each conversation carries optional overrides for `provider`, `model`, `base_url`, `api_key`, `agent_mode`, and `show_thinking`; the REPL composes effective values per call from `Conversation::settings` falling back to CLI defaults. `/model`, `/provider`, `/agent`, `/chat` now write to the active conversation (persisted) instead of the process. Switching conversations via `/conv` / `/next` / `/prev` automatically picks up the new conversation's settings.

- **`/thinking on|off|toggle`** — surface or hide agent reasoning
  blocks (`<think>` tokens emitted by Qwen3 and some Anthropic prompt-cache responses). Per-conversation; default off. `--show-thinking` CLI flag sets the launch default. Thinking deltas stream in dim mode and an inline `C_RESET` precedes the next content token so the assistant text isn't tinted.

- **`/history` markdown rendering** — prior assistant turns route through `markdown::render` so they show with the same bold / italic / heading / list / code-block formatting as live responses. System / tool / user lines unchanged.

- **`/clear`** intercepted before `handle_command` so it can call `editor.clear_screen()` on the local rustyline editor; the editor's internal layout tracking is reset alongside the visible viewport, so the next prompt aligns cleanly.

- **`/export <path>` / `/import <path>`** — single-conversation round-trip via pretty-printed JSON. Imports get a fresh id (via `store::next_id`, promoted to `pub(crate)`) so a re-imported file can coexist with the original. Tab completion treats the argument as a filesystem path.

- **`/fork`** — duplicate the active conversation. Fresh id, title becomes `"<title> (fork)"`, immediately switched to. Persists.

- **`/models [provider]`** — list a provider's recommended models (from `crates/rezon-core/models.json`). `*` marks the conversation's currently active model; `(default)` marks the catalog default. `local` is a special case that reports the loaded GGUF path. No-arg uses the active conversation's effective provider.

- **Live tokens-per-second in the terminal title** during generation. The REPL writes `\x1b]0;rezon · ~N.N tok/s\x07` throttled to ~5 Hz; rate is approximated from emitted char count / 4 (same heuristic the chat path uses when the provider omits usage). `Token` and `Thinking` deltas both feed the counter. On `Done` / `Error` the title resets to `rezon`. Suppressed when stdout isn't a tty.

- **`/conv` + `/conv list` disambiguation**: each entry now shows a cyan-meta `(N msgs · 2h ago)` suffix. `Conversation` gained `last_used: Option<u64>` (epoch ms) and a `touch()` helper called whenever a turn lands or an import / fork / new is created. The `/conv` picker sorts most-recently-used first while preserving the correct store index on selection.

- **FTS5-backed `/search`**: new module `conv_index.rs` (`ConvIndex`) opens a SQLite database at `<config_dir>/conversations.db` with a `conv_msgs(conv_id UNINDEXED, msg_idx UNINDEXED, role UNINDEXED, content, tokenize='porter unicode61')` virtual table. Rebuilt from the in-memory `Store` on launch; mutated in-place on every turn (`replace_conv`), `/delete` (`delete_conv`), `/import` / `/fork`. `cmd_search` translates the query into FTS5 syntax (`tok*` prefix matching for word tokens, `"phrase"` quoting for tokens with punctuation), and renders the matched snippet with FTS5's `<<` / `>>` highlights. Linear walk still backs the empty-query case and the index-unavailable fallback. Adds `rusqlite = "0.32" features = ["bundled"]` as a direct rezon-tui dep (matches rezon-core's version + features).

- **Conversation tests** (3 new in `rezon-tui::conv_index`): prefix vs phrase query construction, empty-query handling, and a full insert / search / delete roundtrip over a temp DB.

- **Markdown renderer expanded**: tables, links, strikethrough, HTML, escapes, and footnote refs all now render. Tables collapse into a box-glyph grid (`│ … │ … │` rows + `├─┼─┤` separator) with column widths computed from the widest cell. Links render as bright-blue anchor text followed by a dim `(url)`. Strikethrough renders with the SGR strikethrough modifier. Raw HTML (e.g. inline `<br>`) renders dim verbatim so it's visible but distinct.

- **Markdown renderer tests** (8 new in `rezon-tui::markdown`): mismatched `*`, backtick escape, stray backticks, links, `\*` escape, HTML pass-through, table grid, strikethrough.

- **`ReplHelper::complete` tests** (8 new in `rezon-tui::input`): empty line, `/` enumerates every command, prefix filters to matching verbs, caret-inside-verb completion, no-completion after non-path-taking verbs (`/exit foo`), `FilenameCompleter` delegation past a path-taking verb (`/load `), non-slash input yields nothing, plus a shared `with_ctx` helper that wires a `rustyline::Context` over an empty `DefaultHistory`. --workspace --all-targets -D warnings` clean. Lints fixed along the way:

  - `manual_flatten` in `core::search::vault_search_impl` — switched to `for hit in rows.flatten()`.

  - `only_used_in_recursion` in `core::vault::read_tree` — dropped the unused `vault: &Path` parameter from the recursive helper.

  - `collapsible_match` on the picker's Up/Down handlers — rewrote as match guards (`KeyCode::Up if state.selected > 0 =>`).

  - `items_after_test_module` in `tui::picker` — moved `cleanup` above the test module.

  - `missing_transmute_annotations` on `core::search::register_sqlite_vec` — `#[allow]` with a comment explaining the cast (both sides are `unsafe extern "C" fn`).

  - `too_many_arguments` on `tui::agent::spawn_agent_run` — `#[allow]`; collapsing into a struct would only push the bag-of-args one layer in.

### Changed
- **Workspace refactor.** Rust code split into a 3-crate Cargo workspace under `crates/`:

  - `rezon-core` — provider-agnostic backend: chat (local llama.cpp + OpenAI-compatible cloud via `async-openai`), agent loop, tools (including `search_notes`), vault file ops, FTS5 + sqlite-vec search index, embedding worker + background catch-up loop. Zero Tauri references.

  - `rezon-web` — thin Tauri shell wrapping `rezon-core` via `TauriChatSink`, `TauriEventSink`, `TauriConfirmationGate`, `#[tauri::command]` wrappers, and config-dir resolution. The frontend (`src/`) is untouched; all command names and event names preserved.

  - `rezon-tui` — terminal REPL described above.

- `src-tauri/` moved to `crates/rezon-web/`. `tauri.conf.json` got `frontendDist: "../../dist"`. The `make dev` / `make build` targets pass `--config $(TAURI_CONF)` so Tauri finds the relocated config; everything else (frontend, dev server, etc.) is unchanged.

- `LlmState`, `SearchState`, `EmbedState` are now registered as `Arc<T>` in Tauri's managed state so they can be shared between the chat command and the agent loop without copies.

- `ChatSink` trait introduced in `rezon-core::llm` to replace the
  previous direct `app.emit("chat-token"|"chat-stats"|"chat-done", …)`
  calls. `TauriChatSink` (in `rezon-web`) preserves the exact event names and payloads.

- `SearchState` now takes its data directory at construction (`SearchState::new(data_dir)`); the previous `Default` impl required an `AppHandle` to resolve `app_data_dir`. `rezon-web` builds it inside `.setup()` after the handle is available.

- `EmbedState::load` no longer emits Tauri events directly; the shell wrapper (`web::embed::do_load_embed`) emits `embed-loading` / `embed-loaded` / `embed-load-error` around the core call. `core::embed::ensure_catchup_started` takes `Arc<EmbedState>` + `Arc<SearchState>` instead of `AppHandle`.

- `core::llm::to_openai_messages` now skips `tool`-role messages (previously errored on unknown role) so a mixed agent/chat conversation history flows through the cloud chat endpoint cleanly. Same filter on the local chat path.

- `core::agent::tool::ToolContext` lost its `app: Option<AppHandle>` field. `agent::tools::search_notes` receives `Arc<SearchState>` + `Arc<EmbedState>` at construction (`register_search_notes(&mut reg, search, embed)`) instead of reaching for them through Tauri state.

- **README** — rewritten for the workspace layout. Now documents both shells (`rezon` GUI + `rezon-tui` REPL), describes the three-crate split with an annotated file tree, has separate Quick-start sections per shell, and lists all `make` targets including the new TUI ones. Build/run instructions for `rezon-tui` make clear it builds with `cargo` alone (no Bun / Tauri prerequisites).

- **`package.json`** — `tauri` npm script now bakes in `--config crates/rezon-web/tauri.conf.json` so `bun run tauri …` works standalone (no need to go through `make dev` / `make build`).

- **`docs/dev/*.md` paths** — 6 stale `src-tauri/` references rerouted to the workspace layout: `crates/rezon-web/examples/` for the spike + ReAct prototype + tool-calling runner; `crates/rezon-core/src/llm.rs` for the worker; the agent loop doc now shows the core / web split (provider-agnostic types in `crates/rezon-core/src/agent/`, Tauri command surface in `crates/rezon-web/src/agent/`).

- `SearchState::close_vault(path)` added in core so the TUI's `/vault close` actually drops the per-vault index + stops its file watcher (the GUI doesn't yet surface this).

### Fixed
- `make dev` / `make build` continue to work after the workspace refactor via the `--config $(TAURI_CONF)` flag passed to the Tauri CLI.

- **Markdown re-render row-count went stale on terminal resize.** `wait_for_turn` now captures `terminal_size()` into `stream_width: Option<u16>` on the first streamed `Token` and passes it into `rerender_markdown`. The visible rows were laid out against that width; re-reading the width at `Done` time after a mid-stream resize used to over- or under-clear and leak stale rows.

- **Spinner felt silent on multi-second loads.** `with_spinner` records start time and appends a dim `(Ns)` suffix to every frame so the user has a continuously-updating progress signal even when the underlying `spawn_blocking` work doesn't emit anything.

- **Markdown renderer mis-fires.** Swapped the hand-rolled state-machine parser for `pulldown-cmark` (default-features off; `ENABLE_TABLES`/`STRIKETHROUGH`/`TASKLISTS`/`FOOTNOTES`). Fixes:

  - `1 * 2 * 3` and similar arithmetic-looking prose no longer triggers spurious italic — CommonMark's flanking rules refuse the run.

  - Trailing/mismatched `*` and stray backticks pass through as literal characters instead of swallowing the rest of the line.

  - Backslash escapes (`\*`, `` \` ``, `\\`) now render as the intended literal character.

  - HTML and `[text](url)` are no longer rendered verbatim — they get distinct styles (dim for HTML, bright-blue text + dim URL for links). Public API (`render` / `count_rows`) is unchanged; the carried- over hand-roll tests still pass alongside the 8 new ones.

## [Older entries]

### Changed
- Migrated all headless-component usage from Radix (`@radix-ui/react-dialog`, `@radix-ui/react-alert-dialog`, `@radix-ui/react-tooltip`) to [Base UI](https://base-ui.com) (`@base-ui/react@1.4.1`), the consolidation effort by the same authors. Single dep, consistent API. Component name shifts: Radix `Overlay` → Base UI `Backdrop`; `Content` → `Popup`; Tooltip needs an extra `Positioner` wrapper between `Portal` and `Popup`. `Tooltip.Provider` props renamed (`delayDuration` → `delay`, `skipDelayDuration` → `timeout`). `Tooltip.Trigger` no longer needs `asChild` — it renders a `<button>` itself, so `className`/`onClick` go directly on the trigger.

- Provider and Theme native `<select>` elements replaced with Base UI `Select` for visual consistency with the rest of the themed/Base-UI-styled UI (no more OS-default chevron jumping out against the rest of the app). New shared wrapper at `src/Select.tsx` used by both the right-sidebar Provider field and the SettingsDrawer Theme field; takes `{ value, label }` items and a string `onValueChange`.

- Recommended-models field for cloud providers replaced the `<select>` + free-text `<input>` pair with a single Base UI `Combobox`. Typing filters the list; an explicit "No matches — press Enter to use as-is" empty state preserves the free-text override path. The Other-provider three-input stack (model + base URL + API key) is unchanged.

- Adopted Tailwind CSS v4 via `@tailwindcss/vite`. Component styling migrated from hand-rolled CSS classes (`rs-`, `conv-`, `msg-`, etc.) to Tailwind utility classes inline in JSX. The CSS-variables theming is preserved and surfaced as Tailwind color tokens via `@theme inline { --color-bg: var(--bg); ... }`, so utilities like `bg-bg` / `text-fg` / `border-border` automatically follow `[data-theme="..."]` switches without needing the `dark:` variant. `App.css` shrank to roughly: theme variables, markdown-content rules (`.md p`, `.md h1`...) that target generated HTML, code-block wrapper, and Radix `data-state` keyframes.

### Fixed
- AlertDialog / SettingsDrawer flashed in the top-left quadrant for one animation cycle before settling in the center after the Tailwind migration. Cause: Tailwind v4 centers via the modern `translate:` property (`-translate-x-1/2 -translate-y-1/2`), but the dialog pop-in/out keyframes wrote to the legacy `transform: translate(...)`. The two properties compose additively, double-translating the dialog off-center while the animation ran. Keyframes now animate only `scale` + `opacity`, leaving translation entirely to Tailwind.

- Crash on app close inside `__cxa_finalize` → `ggml_metal_device_free` → `GGML_ASSERT([rsets->data count] == 0) failed`. Cause: the worker thread refactor for KV-cache reuse stored only the `mpsc::Sender` in `LoadedHandle` and discarded the `JoinHandle`, so on `RunEvent::Exit` we'd close the channel and immediately return — the worker (and its `LlamaContext`) could still be alive when C++ static destructors ran on the main thread, leaving live resource sets on the metal device. Fix: `LoadedHandle` now stores `Option<JoinHandle>` and its `Drop` impl closes the channel and joins the worker before returning. `LlmState::shutdown` also flips the cancel flag so an in-flight chat aborts immediately rather than running to `MAX_NEW_TOKENS`. Same join-on-drop also runs on model swap, so the previous model's worker is fully torn down before the new one takes over.

### Added
- Multiple conversations with a left sidebar. Conversations have their own title, system prompt, message history, and timestamps. Sorted most-recent first; rename inline (pencil icon) and delete (trash icon, confirms). All persisted to `localStorage` along with the currently selected conversation id; conversation titles are auto-derived from the first user message.

- Editable per-conversation system prompt in the right sidebar. New conversations seed from `settings.defaultSystemPrompt`.

- Settings drawer (Settings button in the sidebar) with theme (system/light/dark — applied via `data-theme` on `:root` and CSS variables), font size slider (12–20px, applied via root `font-size`), and default system prompt for new conversations. Persisted to `localStorage`.

- Copy buttons: per-message (revealed on row hover, copies the raw content) and per-code-block (revealed on `<pre>` hover, reads `pre.innerText` so it strips the highlight markup).

- Token / timing stats per assistant message. Backend emits a `chat-stats` event before `chat-done`:

  - Local: exact `promptTokens`, `cachedTokens`, `genTokens`, plus `durationMs` measured from the start of `run_chat_with_cache`.

  - Cloud: requests `stream_options.include_usage`; uses the final chunk's usage when present, otherwise falls back to a `len/4` char estimate so the row is never blank. Also includes `durationMs`. Rendered as a compact monospace line under each message: `1234 prompt (1100 cached) · 234 gen · 25.0 tok/s · 9.4s · local`.

- Better banner / error states. Load errors render in a dedicated banner with a dismiss button; chat errors render as a distinct error-styled assistant bubble (monospace, muted-red background) instead of mixing with normal markdown content.

### Changed
- Adopted Radix UI primitives for the bits where accessibility matters most: `@radix-ui/react-dialog` powers the SettingsDrawer (focus trap, ESC-to-close, scroll lock, portal, ARIA), `@radix-ui/react-alert-dialog` replaces the native `confirm()` on conversation delete, and `@radix-ui/react-tooltip` (wrapped in a top-level `Tooltip.Provider`) labels icon-only buttons (sidebar collapse/expand, "+" new chat in the collapsed left strip). The primitives are unstyled — CSS lives in `App.css` keyed off Radix's `data-state` attributes for fade/pop animations. No styling-framework migration: still vanilla CSS + CSS variables.

- Three-pane layout: left sidebar (conversations + settings), center (chat log + input), right sidebar (provider, model, system prompt). Both sidebars are collapsible (chevron toggle, persisted to settings); collapsed strips show only an expand button (left also keeps a "+" shortcut for new chat). Provider, model row, and the per-conversation system prompt textarea moved out of the chat header into a new `RightSidebar` component. Provider selection is now a single dropdown rather than a radio list.

- Frontend split into modules: `types.ts`, `storage.ts`, `Sidebar.tsx`, `RightSidebar.tsx`, `SettingsDrawer.tsx`, `MessageBody.tsx` (extracted from `App.tsx`). `App.tsx` is now an orchestrator wiring them together.

- The whole UI is themed via CSS variables (`--bg`, `--fg`, `--accent`, `--border`, `--code-bg`, etc.) — no more hard-coded `#4a7dff` / `rgba(127,127,127,...)` etc. scattered through `App.css`.

### Added (earlier in this Unreleased)
- KV cache reuse across turns for the local backend. Each loaded model now owns a dedicated worker thread that holds the `LlamaContext` and a shadow `Vec<LlamaToken>` of tokens currently in the KV cache. On each request the worker tokenizes the new full prompt, finds the longest common prefix with the cached tokens, calls `clear_kv_cache_seq` to truncate the KV cache to that point, and decodes only the divergent suffix before sampling. Continuing the same conversation now re-decodes ~zero prompt tokens instead of the full history.

- Stop button to abort an in-flight chat. New `cancel_chat` Tauri command flips an `AtomicBool` on `LlmState` that the local generation loop and the cloud stream loop both poll between iterations. The flag is reset at the start of each new `chat`. While streaming, the chat-input Send button is replaced by a red Stop button.

### Changed
- `LlmState.loaded` switched from `tokio::sync::Mutex<Option<Loaded>>` to `std::sync::Mutex<Option<LoadedHandle>>`. The handle holds the path and an `mpsc::Sender<WorkerRequest>` for the worker thread, which owns the model and context (`LlamaContext<'a>` is `<'a>`-tied to `LlamaModel` and not `Send`, so the only safe way to keep it alive between turns is to pin it to a single thread). `model_status` is now a sync command.

- Cloud providers via [`async-openai`](https://github.com/64bit/async-openai) 0.36 (`chat-completion` feature). Four OpenAI-compatible providers ship out of the box. The first three are env-driven with hard-coded base URL, default model, and recommended-model list; the last is fully user-configurable:

  - **OpenAI** — `OPENAI_API_KEY`, `https://api.openai.com/v1`

  - **Anthropic** — `ANTHROPIC_API_KEY`, `https://api.anthropic.com/v1`

  - **OpenRouter** — `OPENROUTER_API_KEY`, `https://openrouter.ai/api/v1`

  - **Other** — user-configurable: model, base URL, and API key are entered in the UI and sent in `chat` opts. Intended for OpenAI-compatible servers like Ollama (`http://localhost:11434/v1`), LM Studio, or `llama.cpp` `server`. The API key is optional; an empty field is sent as `"no-key"` so servers that don't authenticate ignore it.

- Provider selector in the UI spans Local + the four cloud providers (later changed to a dropdown — see the [Unreleased] entry above). Named providers' cloud row shows a recommended-models dropdown alongside a free-text override (the text input is canonical and sent to the backend). The `other` row instead stacks three free-text inputs: model, base URL, API key (password field).

- New Tauri command `cloud_providers()` returns the static provider table plus per-provider `apiKeySet` and `userConfigurable`. Replaces `openai_available()`.

- `chat` command takes `opts: { provider, model?, baseUrl?, apiKey? }` (camelCase). `baseUrl` and `apiKey` are only consumed when the provider is user-configurable (i.e. `other`). **Breaking**: the previous `openaiModel` / `openaiBaseUrl` fields are gone — for the named providers, base URL is hard-coded.

- All cloud providers stream deltas through the same `chat-token` / `chat-done` events as the local backend.

- Markdown rendering for assistant messages via `react-markdown` with `remark-gfm`.

- KaTeX math rendering (`remark-math` + `rehype-katex`), including normalization of `\(...\)` and `\[...\]` LaTeX delimiters to `$...$` / `$$...$$` so they are picked up by `remark-math`. Skips fenced code blocks.

- Syntax highlighting for code blocks via `rehype-highlight` (highlight.js github-dark theme).

- Auto-close of unbalanced fenced code blocks during streaming so partial responses render correctly.

### Changed
- `package.json` gained `react-markdown`, `remark-gfm`, `remark-math`, `rehype-katex`, `rehype-highlight`, `katex`, `highlight.js`.

## [0.1.0] - initial

### Added
- Tauri 2 + React 19 + Vite scaffold.

- Rust backend in `src-tauri/src/llm.rs` wrapping `llama-cpp-2` 0.1.146 with the `metal` feature.

- `LlmState` holding `LlamaBackend` and the currently loaded `LlamaModel`, with explicit `shutdown()` on `RunEvent::Exit` to drop the model before the backend and avoid ggml-metal static-destructor races.

- Tauri commands: `load_model`, `model_status`, `chat`.

- Frontend events: `model-loading`, `model-loaded`, `model-load-error`, `chat-token`, `chat-done`.

- Persistence of the last-loaded model path under the app config dir (`last_model.txt`) and auto-load on startup.

- Streaming token generation: prompt is tokenized, decoded once, then sampled token-by-token (`temp(0.7)` -> `dist(1234)`) up to `MAX_NEW_TOKENS` or EOG, with each piece emitted as a `chat-token` event.

- UTF-8 incremental decoder (`encoding_rs`) so multi-byte tokens aren't truncated mid-codepoint.

- Use of model-embedded chat template via `model.chat_template(None)` + `apply_chat_template`.

- React UI: model path input, file picker (`@tauri-apps/plugin-dialog`, `.gguf` filter), Load button, chat log with role labels, textarea input with Enter-to-send / Shift+Enter for newline, auto-scroll on new content.

- `Makefile` with `install`, `dev`, `build`, `web-dev`, `web-build`, `check`, `fmt`, `fmt-check`, `lint`, `test`, `clean` targets.

- MIT `LICENSE`.
