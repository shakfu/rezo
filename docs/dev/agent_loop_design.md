# Agent Loop and Tool Trait: Design Sketch

A sketch for the rezon agent module. The goal is single-agent, multi-tool, streaming, provider-agnostic - covering OpenAI, Anthropic, OpenRouter, and local llama-cpp-2 with the same loop.

This is a design artifact, not committed code. Type signatures are illustrative; names and exact shapes are open for discussion.

## Module layout

```
crates/rezon-core/src/      // provider-agnostic agent loop + types
  agent/
    mod.rs           // public API
crates/rezon-web/src/        // Tauri shell
  agent/
    mod.rs           // re-exports + Tauri commands (agent_chat, cancel_agent)
    tool.rs          // Tool trait, ToolRegistry, ToolCall, ToolResult
    delta.rs         // normalized AgentDelta enum
    provider.rs      // Provider trait
    cloud.rs         // CloudProvider: async-openai → AgentDelta stream
    local.rs         // LocalProvider: llama-cpp-2 → AgentDelta stream
    loop_.rs         // run_agent: the loop itself
    confirm.rs       // confirmation flow for destructive tools
    tools/
      mod.rs         // registry assembly, feature gates
      file_read.rs   // safe-by-default
      shell_exec.rs  // requires_confirmation = true
      web_fetch.rs
      ...
```

`agent` is a sibling of the existing `llm` module. The `llm` module's local worker thread is reused; the `agent` module wraps it for the tool-aware code path.

## Core types

### `AgentDelta` - the normalized stream

Both the cloud adapter and local adapter produce a stream of these. The loop is written once against this enum. Modeled on the *intersection* of what OpenAI streaming chunks and `ChatParseStateOaicompat` deltas provide.

```rust
pub enum AgentDelta {
    /// Visible text fragment for the assistant turn.
    Content(String),

    /// Reasoning/thinking block fragment. Qwen 3's <think>...</think>,
    /// or Anthropic's thinking blocks once we move off OpenAI-compat.
    /// UI may render distinctly or hide.
    Thinking(String),

    /// First chunk of a tool call. Carries id + name. Args may be
    /// empty here and arrive in subsequent ToolCallArgs.
    ToolCallStart { index: u32, id: String, name: String },

    /// Argument fragment for an in-progress tool call.
    ToolCallArgs { index: u32, fragment: String },

    /// Optional explicit end-of-tool-call marker. Some streams omit
    /// this; the loop detects completion via the parent stream ending
    /// or via a finish-reason signal.
    ToolCallEnd { index: u32 },

    /// Provider-specific stats (token counts, cache usage).
    Stats(StreamStats),

    /// End of this assistant turn. finish_reason indicates whether
    /// tools were requested or the turn is final.
    Done { finish_reason: FinishReason },
}

pub enum FinishReason {
    Stop,        // model returned a final answer
    ToolCalls,   // model requested tools; loop should dispatch and continue
    Length,      // hit token limit
    Cancelled,
    Other(String),
}
```

### `Tool` - the trait every backend tool implements

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    /// Stable identifier. Surfaced verbatim to the model.
    fn name(&self) -> &str;

    /// One-line description used by the model for tool selection.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's parameters object. Provider-neutral;
    /// adapters wrap it into OpenAI's {"type":"function","function":{...}}
    /// or any other backend's tool-definition shape.
    fn parameters(&self) -> serde_json::Value;

    /// Whether this tool needs explicit user confirmation before dispatch.
    /// Default false; override to true for shell exec, file write, etc.
    fn requires_confirmation(&self) -> bool { false }

    /// Execute the tool. Args are the parsed parameters object. Return
    /// any JSON-serializable result; the loop will wrap it into a
    /// `tool` role message for the next turn.
    ///
    /// Receives the cancel flag so long-running tools can abort
    /// promptly when the user cancels the agent.
    async fn dispatch(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<serde_json::Value, ToolError>;
}

pub struct ToolContext {
    pub cancel: Arc<AtomicBool>,
    pub app: AppHandle,
    /// Optional working directory or other ambient state.
    pub workdir: Option<PathBuf>,
}

pub enum ToolError {
    Argument(String),    // parameters didn't match schema
    Denied,              // user rejected confirmation
    Cancelled,
    Runtime(anyhow::Error),
}
```

### `ToolRegistry` - what's available this session

```rust
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn empty() -> Self { ... }

    pub fn register(&mut self, tool: Arc<dyn Tool>) { ... }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> { ... }

    /// OpenAI-shaped tools array, ready to feed both cloud
    /// (async-openai) and local (apply_chat_template_with_tools_oaicompat)
    /// without per-backend translation.
    pub fn openai_schemas(&self) -> Vec<serde_json::Value> { ... }
}
```

A "tool set" abstraction (e.g. read-only vs read-write, or "research mode" vs "code mode") lives one level up: the caller picks which tools to register before each agent run.

### `Provider` - what every backend implements

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    /// Open a streaming completion. Returns a stream of normalized
    /// deltas. The stream ends with a Done delta carrying finish_reason.
    async fn stream(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        opts: &ProviderOpts,
    ) -> Result<BoxStream<'static, Result<AgentDelta>>>;
}

pub struct ProviderOpts {
    pub model: Option<String>,
    pub max_tokens: Option<u32>,
    pub cancel: Arc<AtomicBool>,
}
```

`ChatMessage` is rezon's existing internal message type, extended with `tool_calls` on assistant turns and a `Tool` role for tool results.

## The loop

```rust
pub async fn run_agent(
    app: AppHandle,
    provider: Arc<dyn Provider>,
    registry: Arc<ToolRegistry>,
    initial: Vec<ChatMessage>,
    opts: AgentOpts,
) -> Result<AgentOutcome> {
    let mut messages = initial;
    let cancel = opts.cancel.clone();

    for step in 1..=opts.max_steps {
        if cancel.load(Ordering::Relaxed) {
            return Ok(AgentOutcome::Cancelled);
        }

        let stream = provider
            .stream(&messages, &registry.openai_schemas(), &opts.provider_opts)
            .await?;
        let assistant = consume_stream(&app, stream, &cancel).await?;

        // Persist the assistant turn (including tool_calls) so the
        // next iteration's prompt has full history.
        messages.push(assistant.clone().into_message());

        match assistant.finish_reason {
            FinishReason::Stop | FinishReason::Length | FinishReason::Other(_) => {
                emit_done(&app, &assistant);
                return Ok(AgentOutcome::Final(assistant.content));
            }
            FinishReason::Cancelled => return Ok(AgentOutcome::Cancelled),
            FinishReason::ToolCalls => { /* fall through to dispatch */ }
        }

        for call in &assistant.tool_calls {
            let tool = match registry.get(&call.name) {
                Some(t) => t,
                None => {
                    messages.push(tool_error_message(&call.id, "unknown tool"));
                    continue;
                }
            };

            if tool.requires_confirmation()
                && !confirm::ask(&app, call).await?
            {
                messages.push(tool_denied_message(&call.id));
                emit_tool_end(&app, &call.id, ToolOutcome::Denied);
                continue;
            }

            emit_tool_start(&app, call);
            let result = tool.dispatch(call.args.clone(), &tool_ctx(&app, &cancel)).await;
            emit_tool_end(&app, &call.id, ToolOutcome::from(&result));
            messages.push(tool_result_message(&call.id, result));
        }
    }

    Err(anyhow!("agent exceeded max_steps={}", opts.max_steps))
}
```

`consume_stream` accumulates the deltas into a structured assistant turn:

```rust
struct AssistantTurn {
    content: String,
    thinking: String,
    tool_calls: Vec<ToolCall>,
    finish_reason: FinishReason,
    stats: Option<StreamStats>,
}

async fn consume_stream(
    app: &AppHandle,
    mut stream: BoxStream<'static, Result<AgentDelta>>,
    cancel: &Arc<AtomicBool>,
) -> Result<AssistantTurn> {
    let mut turn = AssistantTurn::default();
    let mut tool_acc: BTreeMap<u32, ToolCallBuilder> = BTreeMap::new();

    while let Some(delta) = stream.next().await {
        if cancel.load(Ordering::Relaxed) {
            turn.finish_reason = FinishReason::Cancelled;
            break;
        }
        match delta? {
            AgentDelta::Content(s) => { turn.content.push_str(&s); emit_token(app, &s); }
            AgentDelta::Thinking(s) => { turn.thinking.push_str(&s); emit_thinking(app, &s); }
            AgentDelta::ToolCallStart { index, id, name } => {
                tool_acc.insert(index, ToolCallBuilder::start(id, name));
            }
            AgentDelta::ToolCallArgs { index, fragment } => {
                if let Some(b) = tool_acc.get_mut(&index) { b.push(&fragment); }
            }
            AgentDelta::ToolCallEnd { index } => {
                if let Some(b) = tool_acc.remove(&index) { turn.tool_calls.push(b.finish()?); }
            }
            AgentDelta::Stats(s) => turn.stats = Some(s),
            AgentDelta::Done { finish_reason } => {
                turn.finish_reason = finish_reason;
                break;
            }
        }
    }

    // Drain any tool calls that did not receive an explicit ToolCallEnd
    // (cloud adapters often omit it, signalling completion via Done).
    for (_, b) in std::mem::take(&mut tool_acc) {
        turn.tool_calls.push(b.finish()?);
    }

    Ok(turn)
}
```

## Adapters

### Cloud (`cloud.rs`)

Wraps `async-openai`. Maps `ChatCompletionStreamResponseDelta` chunks to `AgentDelta`:

- `delta.content` -> `AgentDelta::Content`

- `delta.tool_calls[i]` first chunk -> `AgentDelta::ToolCallStart`

- `delta.tool_calls[i]` subsequent chunks -> `AgentDelta::ToolCallArgs`

- `usage` (if `stream_options.include_usage`) -> `AgentDelta::Stats`

- `finish_reason` on the final chunk -> `AgentDelta::Done`

This is a small, mostly mechanical adapter (~150 lines).

### Local (`local.rs`)

Wraps the existing `llama-cpp-2` worker thread. Validated path from the spike (`docs/dev/local_tool_calling.md`):

1. `model.apply_chat_template_with_tools_oaicompat(template, msgs, Some(tools_json), None, true)`

2. **Skip grammar** until the upstream `GGML_ASSERT` bug is fixed (use plain `temp` + `dist` sampling).

3. `result.streaming_state_oaicompat()` for the parser.

4. For each generated token's text: `parse_state.update(piece, true)`.

5. Each returned JSON delta string is parsed and mapped to `AgentDelta`.

Because `ChatParseStateOaicompat` already produces deltas in OpenAI shape (verified empirically with Qwen 3), the mapping is symmetric to the cloud adapter. They share a `parse_oai_delta(json: &str) -> AgentDelta` helper.

The local adapter exposes the same `Provider` interface but runs the heavy work on the worker thread already established in `llm.rs`, sending deltas back through an MPSC channel that the adapter wraps as a `Stream`.

#### Tool-capability gating

If `apply_chat_template_with_tools_oaicompat` returns `parse_tool_calls = false` or `parser = None`, the local adapter refuses tool-aware mode and the loop falls back to the existing text-only chat path. The model picker should mark tool-capable models in the UI; this gating is a runtime safety net.

## Tauri surface

### Commands

```rust
#[tauri::command]
pub async fn agent_chat(
    app: AppHandle,
    state: State<'_, AgentState>,
    messages: Vec<ChatMessage>,
    opts: AgentChatOpts, // includes provider, model, tool_set
) -> Result<String, String>;

#[tauri::command]
pub fn cancel_agent(state: State<'_, AgentState>);

#[tauri::command]
pub async fn confirm_tool_call(
    state: State<'_, AgentState>,
    confirmation_id: String,
    approved: bool,
);
```

### Events

Minimal surface to keep the UI almost-invisible:

| Event | Payload | Purpose |
|---|---|---|
| `agent-token` | `String` | Visible content delta. Drives the chat bubble. |
| `agent-thinking` | `String` | Reasoning/thinking delta. UI may hide or render distinctly. |
| `agent-tool-start` | `{ id, name }` | Show a small inline pill in the message. |
| `agent-tool-end` | `{ id, ok, error? }` | Pill collapses or shows error. |
| `agent-tool-confirm` | `{ confirmation_id, name, args, summary }` | UI prompts user; user calls `confirm_tool_call`. |
| `agent-stats` | `StreamStats` | Token counts, timing. Same shape as today's `chat-stats`. |
| `agent-done` | `String` | Final assistant text. Loop terminated. |

The pill UI is the only required new visual element. Trace expansion (showing every tool call's args + result inline) is a Phase 2 polish.

## Cancellation

Reuses the existing `AtomicBool` pattern from `LlmState`.

- The cancel flag is checked at the top of each loop iteration, before starting a stream, and on every delta consumed.

- Cloud streams: aborting the HTTP request is the simplest path; can also just stop reading and let the client drop.

- Local streams: the worker thread already polls the cancel flag between decode steps. The adapter closes its receiving end of the delta channel; the worker observes channel-disconnect and bails.

- Mid-tool-call cancellation: documented as a known edge case (see `local_tool_calling.md` "Open items"). A partial tool call is dropped; the loop returns `AgentOutcome::Cancelled` without dispatching it.

## Confirmation flow

```rust
async fn ask(app: &AppHandle, call: &ToolCall) -> Result<bool> {
    let id = uuid();
    let summary = format!("Tool: {} args: {}", call.name, truncate(&call.args, 200));
    app.emit("agent-tool-confirm", &Confirm { confirmation_id: id.clone(), name: call.name.clone(), args: call.args.clone(), summary })?;

    let (tx, rx) = oneshot::channel();
    pending_confirms.lock().unwrap().insert(id, tx);

    // The Tauri command `confirm_tool_call` finds this oneshot and resolves it.
    // Honor cancel in case the user closes the dialog or aborts.
    tokio::select! {
        result = rx => Ok(result.unwrap_or(false)),
        _ = wait_cancel(cancel.clone()) => Ok(false),
    }
}
```

"Remember this tool for this session" is intentionally deferred to a later phase. v1 prompts on every destructive call. Persisted always-allow is out of scope until the threat model is clearer.

## Decisions

The following calls have been made; the rest of this document assumes them.

1. **Tool-call persistence**: persist tool calls and their results alongside the assistant turn. UI shows a collapsed pill by default; the user can expand to see args + result. Very large tool_result payloads are truncated when persisted.

2. **Tool selection**: all registered tools are available in every conversation. No tool-set picker. Destructive tools are gated by per-call confirmation (#3) so accidents are bounded.

3. **Confirmation granularity**: per-call. Every invocation of a tool with `requires_confirmation() == true` blocks on a UI prompt. "Remember per session" is deferred to a later phase.

4. **Thinking-block UI**: hidden by default. While thinking deltas stream, the UI shows an active "Thinking..." indicator. When the thinking block closes, the indicator becomes a collapsible "Show reasoning" toggle that reveals the buffered content on demand.

5. **Tool-arg streaming**: pill appears on `ToolCallStart` with the tool name and a running spinner. Argument fragments are NOT rendered live. After dispatch, the pill shows ok/error status; the user can expand to see full args + result.

6. **Local worker reuse**: extend the existing worker thread with a new tool-aware `WorkerRequest` variant. The current `Chat` variant stays untouched. Same `LlamaContext`, same KV-cache reuse, same Metal-teardown discipline.

7. **Cloud `usage` opt-in**: include `stream_options.include_usage = true` and emit one `agent-stats` event per turn. Carries forward the existing `chat-stats` pattern.

## Phasing

A reasonable build order, each step independently reviewable:

1. `tool.rs` + `delta.rs` + a tiny `ToolRegistry`. No loop yet. Unit tests for schema generation and arg parsing.

2. `cloud.rs` + `loop_.rs`. Wire to a Tauri command. Test end-to-end against OpenRouter (same path as today's prototype, just inside the real module).

3. `local.rs`. Reuse the existing worker thread; gate on `parse_tool_calls = true`. Test against Qwen 3 4B.

4. First real tools: `file_read`, `glob`, `web_fetch` (no confirmation). Validate the registry composition.

5. `confirm.rs` + the dialog UI. Add `shell_exec` as the first destructive tool.

6. UI polish: tool pills, thinking-block rendering, optional trace expansion.

## What this design intentionally does not do

- No multi-agent orchestration. Single agent only.

- No native Anthropic / Gemini clients. OpenAI-compat covers all four current providers.

- No vector store or RAG abstraction. Out of scope for v1.

- No grammar-constrained local sampling. Skipped until upstream fix.

- No DAG / tool-composition runtime. Tools are leaf functions; the model orchestrates.

- No framework adoption (rig, AutoAgents). Revisit when a feature request demands it.
