# TODO

Known gaps and ideas, roughly ordered by usefulness. Not committed scope.

## Inference / backend

- [ ] Cloud: forward sampling params (temperature, top_p, max_tokens) from the UI instead of using API defaults.

- [ ] Cloud: surface non-stream errors with HTTP status / body, not just `format!("{e}")`.

- [x] Persist provider choice + per-provider chosen model across restarts.

- [ ] User-overridable cloud-models config at `~/Library/Application Support/<app>/cloud-models.json`, layering on top of the build-time `src-tauri/models.json` when present.

- [ ] Save the API key entered for the `other` provider (currently it lives only in React state and is lost on app restart). Likely destination: OS keychain.

- [ ] Re-check API-key env vars per request rather than only at app launch, so users don't have to relaunch after exporting a key (`cloud_providers()` only snapshots availability at call time).

- [ ] Load API keys from a `.env` file via [`dotenvy`](https://crates.io/crates/dotenvy). For `tauri dev`, call `dotenvy::dotenv().ok()` early in `lib.rs::run()`. For packaged builds, resolve a path under the app config dir (`dotenvy::from_path(...)`) since the cwd isn't predictable.

- [ ] Configurable sampler: temperature, top-p, top-k, repeat penalty, seed (currently hard-coded `temp(0.7)` + `dist(1234)`).

- [ ] Configurable `n_ctx`, `max_new_tokens`, `n_gpu_layers` (currently hard-coded constants in `llm.rs`).

- [ ] Surface load progress / mmap status; `model-loading` only carries the path.

- [ ] UI affordance to unload the current local model (swap already works: `do_load` drops the previous `LoadedHandle` which closes the channel + joins the worker thread).

- [ ] Graceful error when the model has no embedded chat-template metadata (today it just bubbles up as a string).

- [ ] Handle prompts that overflow `n_ctx` (truncate history vs. error). Now that the KV cache persists across turns, a sliding-window eviction (drop oldest non-system tokens via `clear_kv_cache_seq` + `kv_cache_seq_add`) is more useful than just erroring.

- [ ] Non-macOS build paths (CUDA / CPU-only feature flags).

## Frontend / UX

- [ ] Search across conversations.

- [ ] Export / import conversations as JSON.

- [ ] Edit / regenerate a previous user message (truncate the conversation to that point and re-send).

- [ ] Sidebars: drag to resize (collapse already implemented).

- [ ] Pretty timestamps under each message.

## Tools / agent

- [ ] **`file_read` path sandboxing.** Today the user is the only gate (per-call confirmation). Add a `Settings.fileReadRoot` (or an allow-list of roots) that the tool checks before reading; reject paths outside. Default empty = current behavior.

- [ ] **`web_fetch` domain allow/blocklist.** Same posture: confirmation is the gate today. Add a `Settings.webFetchAllowedHosts` / `webFetchBlockedHosts` so power users can pin the tool to a specific set of sites without re-confirming each call.

- [ ] **Stream large `web_fetch` bodies.** Whole body is buffered before truncation. Fine at the 1 MiB cap; would need rework if the cap is ever raised significantly. Use `Response::bytes_stream()` and stop at `MAX_BYTES`.

- [ ] **"Remember per session" on the confirmation dialog.** Current "Ask" prompts on every call. Add a checkbox in the confirm dialog: "Always allow `web_fetch` for this conversation". Stores in a conversation-scoped trust map (not persisted across sessions).

- [ ] **Trust toggles persisted across sessions.** Same shape as above but persisted; surface a "Revoke" button somewhere. Higher-stakes; worth waiting until the threat model is clearer.

- [ ] **`shell_exec` improvements.** Today: spawn one command, capture stdout/stderr, enforce timeout (kills child on overrun) and per- stream output cap, optional `cwd`. Future: stdin piping, env-var allow-list, mid-run cancel via the agent loop's cancel flag.

- [ ] **Per-conversation tool sets.** Decision #2 was "all tools always available". If users start asking for "research mode" vs "code mode", revisit.

- [ ] **Show reasoning toggle.** Qwen 3's `<think>` blocks are dropped on the floor today (per design decision #4 the indicator is the streaming spinner). Add a collapsible "Show reasoning" affordance under each assistant bubble that did emit thinking deltas.

- [ ] **Truncate large tool results when persisting to localStorage.** The design said to do this; current code stores the full string. Risk: a large `web_fetch` body in conversation history can blow past the localStorage quota.

## Notes / vault

- [ ] **Persist `active_vault` across restarts.** Today it's only set in memory when the FE calls `vault_index_open`. Until the user opens the Notes tab once after launch, the `search_notes` agent tool replies "no vault is open". Save under `app_config_dir` (same pattern as `last_model.txt`) and restore on startup.

- [ ] **Auto-switch the search toggle off Semantic when the embedding model unloads.** Currently the segment becomes disabled but the mode stays selected; queries silently return empty.

- [ ] **Surface KNN distance/score to the model in `search_notes`.** Tool returns ordered results without scores today; adding the raw distance (and the snippet's char range) lets the model rank and cite more precisely.

- [ ] **Persist `vec_chunks` across embedding-model swaps when dim matches.** Today any model change with a different `n_embd` is handled (drop + re-embed); a swap to a same-dim but different-tokenizer model silently produces drift. Record a model fingerprint alongside `embed_dim` in `meta` and require an explicit user re-embed when it changes.

- [ ] **Embedder batching.** `embed_one` runs one decode per chunk. Batch up to N chunks per decode using multiple `seq_id`s — cheap win on Metal once a vault gets large.

- [ ] **Backlinks panel.** vec table now has chunk text + paths; a sibling table indexed by wikilink target opens up Obsidian-style "linked mentions" without much new schema. Probably belongs under `notes` rather than `search`.

- [ ] **Editor jump-to-chunk.** Snippets carry `char_start/char_end` in `chunks` but neither the related panel nor the search results use them; clicking a hit could scroll the Milkdown editor to the matching range.

- [ ] **Hybrid search.** Combine FTS5 BM25 + vector KNN with reciprocal rank fusion (or similar) for a single "Search" mode. Replaces the Text/Semantic toggle.

## Engineering

- [ ] Actual Rust tests (`make test` runs `cargo test` but `llm.rs` has none). In particular: a regression test that loading + dropping a model releases the metal device cleanly (the `GGML_ASSERT([rsets->data count] == 0)` crash that bit us on close). Probably needs a tiny GGUF fixture and a dedicated thread to host `LlamaContext`.

- [ ] Frontend tests / typecheck in CI.

- [ ] CI workflow (fmt-check, clippy, cargo test, vite build).

- [ ] Replace `eprintln!` with structured logging (`tracing`).

- [ ] Audit `unwrap()` on the std `Mutex` in `LlmState` for poisoning.

- [ ] Consider a typed event payload for `chat-token` instead of bare string (e.g. include a turn id so late events can't bleed into a new turn).
