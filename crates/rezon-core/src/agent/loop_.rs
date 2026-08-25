// The agent loop. Provider-agnostic: takes a Provider, a ToolRegistry,
// an EventSink, and an initial message vector; runs until either the
// model returns a final answer, the user cancels, or max_steps is hit.
//
// Tool dispatch is gated by `AgentOpts::gate`: every tool call passes
// through `gate.ask(call, preview)` before the loop emits `ToolStart`.
// Denied calls still produce a tool-result message (with an
// `"error": "denied by user"` body) so the model can react on the
// next turn. The caller owns persistence — the loop only mutates the
// `messages` vector in place.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use futures::StreamExt;
use serde_json::Value;

use crate::agent::confirm::{ConfirmationGate, ConfirmationOutcome};
use crate::agent::delta::{AgentDelta, FinishReason};
use crate::agent::event::{AgentEvent, EventSink};
use crate::agent::message::ChatMessage;
use crate::agent::provider::{Provider, ProviderOpts};
use crate::agent::tool::{Tool, ToolCall, ToolContext, ToolError, ToolRegistry};

#[derive(Clone)]
pub struct AgentOpts {
    pub provider_opts: ProviderOpts,
    pub max_steps: usize,
    /// Gates each tool dispatch on user approval. The default
    /// `AutoApproveGate` makes the loop behave as before; production
    /// rezon passes a `TauriConfirmationGate` that prompts the user.
    pub gate: Arc<dyn ConfirmationGate>,
}

#[derive(Debug)]
pub enum AgentOutcome {
    Final(String),
    Cancelled,
}

/// Run a single agent session. Mutates `messages` to reflect the
/// assistant turn(s) and tool-result turn(s) produced during the run.
pub async fn run_agent(
    provider: Arc<dyn Provider>,
    registry: Arc<ToolRegistry>,
    sink: Arc<dyn EventSink>,
    messages: &mut Vec<ChatMessage>,
    opts: AgentOpts,
) -> Result<AgentOutcome> {
    let cancel = opts.provider_opts.cancel.clone();

    for _step in 1..=opts.max_steps {
        if cancel.load(Ordering::Relaxed) {
            sink.emit(AgentEvent::Cancelled);
            return Ok(AgentOutcome::Cancelled);
        }

        let tools = registry.openai_schemas();
        let mut stream = provider
            .stream(messages.as_slice(), &tools, &opts.provider_opts)
            .await?;

        let mut acc = TurnAccumulator::default();
        // A provider failure mid-turn is captured rather than
        // propagated with `?`. Bailing here would discard `acc` —
        // including content the user has already watched stream into
        // the bubble — and leave `messages` without the assistant turn,
        // so the next request would be built from a history that
        // disagrees with the screen. Record it, finish assembling the
        // partial turn below, then fail.
        let mut stream_err: Option<anyhow::Error> = None;
        while let Some(item) = stream.next().await {
            let delta = match item {
                Ok(d) => d,
                Err(e) => {
                    stream_err = Some(e);
                    break;
                }
            };
            match delta {
                AgentDelta::Content(s) => {
                    acc.content.push_str(&s);
                    sink.emit(AgentEvent::Token(s));
                }
                AgentDelta::Thinking(s) => {
                    acc.thinking.push_str(&s);
                    sink.emit(AgentEvent::Thinking(s));
                }
                AgentDelta::ToolCallStart { index, id, name } => {
                    acc.tool_calls.insert(
                        index,
                        ToolCallBuilder {
                            id,
                            name,
                            args: String::new(),
                        },
                    );
                }
                AgentDelta::ToolCallArgs { index, fragment } => {
                    if let Some(b) = acc.tool_calls.get_mut(&index) {
                        b.args.push_str(&fragment);
                    }
                }
                AgentDelta::ToolCallEnd { .. } => {
                    // Index already complete in `acc.tool_calls`; nothing to do.
                }
                AgentDelta::Stats(s) => sink.emit(AgentEvent::Stats(s)),
                AgentDelta::Done { finish_reason } => {
                    acc.finish_reason = finish_reason;
                    break;
                }
            }
        }

        // Build the assistant turn from the accumulated state.
        let assistant_calls: Vec<ToolCall> = acc
            .tool_calls
            .into_values()
            .map(|b| ToolCall {
                id: b.id,
                name: b.name,
                arguments: b.args,
            })
            .collect();
        messages.push(ChatMessage::Assistant {
            content: acc.content.clone(),
            tool_calls: assistant_calls.clone(),
        });

        // Partial turn is now in `messages`; surface the failure. The
        // sink gets an explicit Error so a UI that only listens for
        // events does not simply see the stream go quiet.
        if let Some(e) = stream_err {
            sink.emit(AgentEvent::Error(e.to_string()));
            return Err(e);
        }

        match acc.finish_reason {
            FinishReason::Cancelled => {
                sink.emit(AgentEvent::Cancelled);
                return Ok(AgentOutcome::Cancelled);
            }
            FinishReason::Stop | FinishReason::Length | FinishReason::Other(_) => {
                sink.emit(AgentEvent::Done {
                    content: acc.content.clone(),
                });
                return Ok(AgentOutcome::Final(acc.content));
            }
            FinishReason::ToolCalls => { /* fall through to dispatch */ }
        }

        if assistant_calls.is_empty() {
            // Provider signaled tool_calls but emitted none. Treat as final
            // to avoid a loop with no progress.
            sink.emit(AgentEvent::Done {
                content: acc.content.clone(),
            });
            return Ok(AgentOutcome::Final(acc.content));
        }

        for call in &assistant_calls {
            if cancel.load(Ordering::Relaxed) {
                sink.emit(AgentEvent::Cancelled);
                return Ok(AgentOutcome::Cancelled);
            }

            // Ask the user (or the gate's policy) for approval
            // before announcing ToolStart. Denied calls still emit a
            // ToolEnd so the UI's pill collapses to an error state,
            // and a tool message is appended to the history so the
            // model can react.
            // Render the preview once per call before the gate
            // prompt. Failure to parse the model's `arguments` JSON
            // is non-fatal — the gate just shows the raw args, which
            // is what it would have done before the preview hook
            // existed.
            let preview = registry.get(&call.name).and_then(|tool| {
                serde_json::from_str::<Value>(&call.arguments)
                    .ok()
                    .and_then(|v| tool.preview(&v))
            });
            let outcome = opts.gate.ask(call, preview.as_deref()).await;
            if matches!(outcome, ConfirmationOutcome::Denied) {
                sink.emit(AgentEvent::ToolStart {
                    id: call.id.clone(),
                    name: call.name.clone(),
                });
                sink.emit(AgentEvent::ToolEnd {
                    id: call.id.clone(),
                    ok: false,
                    result: None,
                    error: Some("denied by user".to_string()),
                });
                let content = serde_json::to_string(&serde_json::json!({
                    "error": "denied by user"
                }))
                .unwrap_or_else(|_| "{\"error\":\"denied\"}".to_string());
                messages.push(ChatMessage::Tool {
                    tool_call_id: call.id.clone(),
                    content,
                });
                continue;
            }

            sink.emit(AgentEvent::ToolStart {
                id: call.id.clone(),
                name: call.name.clone(),
            });

            let result = dispatch_one(&registry, call, &cancel).await;
            match &result {
                Ok(value) => sink.emit(AgentEvent::ToolEnd {
                    id: call.id.clone(),
                    ok: true,
                    result: Some(value.clone()),
                    error: None,
                }),
                Err(e) => sink.emit(AgentEvent::ToolEnd {
                    id: call.id.clone(),
                    ok: false,
                    result: None,
                    error: Some(e.to_string()),
                }),
            }

            // Append a tool message regardless of success so the model
            // can recover from errors on the next turn.
            let content = match &result {
                Ok(v) => v.to_string(),
                Err(e) => serde_json::to_string(&serde_json::json!({
                    "error": e.to_string()
                }))
                .unwrap_or_else(|_| "{\"error\":\"<unserializable>\"}".to_string()),
            };
            messages.push(ChatMessage::Tool {
                tool_call_id: call.id.clone(),
                content,
            });
        }
    }

    let msg = format!("agent exceeded max_steps={}", opts.max_steps);
    sink.emit(AgentEvent::Error(msg.clone()));
    Err(anyhow!(msg))
}

async fn dispatch_one(
    registry: &ToolRegistry,
    call: &ToolCall,
    cancel: &Arc<AtomicBool>,
) -> Result<Value, ToolError> {
    let tool = registry
        .get(&call.name)
        .ok_or_else(|| ToolError::Argument(format!("unknown tool `{}`", call.name)))?
        .clone();

    let args: Value = if call.arguments.trim().is_empty() {
        Value::Object(Default::default())
    } else {
        serde_json::from_str(&call.arguments).map_err(|e| {
            ToolError::Argument(format!(
                "arguments not valid JSON: {e} (raw: {})",
                call.arguments
            ))
        })?
    };

    let ctx = ToolContext {
        cancel: cancel.clone(),
    };
    dispatch_tool(tool.as_ref(), args, &ctx).await
}

async fn dispatch_tool(
    tool: &dyn Tool,
    args: Value,
    ctx: &ToolContext,
) -> Result<Value, ToolError> {
    tool.dispatch(args, ctx).await
}

struct TurnAccumulator {
    content: String,
    thinking: String,
    tool_calls: BTreeMap<u32, ToolCallBuilder>,
    finish_reason: FinishReason,
}

impl Default for TurnAccumulator {
    fn default() -> Self {
        Self {
            content: String::new(),
            thinking: String::new(),
            tool_calls: BTreeMap::new(),
            // Default to Stop; overwritten when a Done delta arrives.
            finish_reason: FinishReason::Stop,
        }
    }
}

struct ToolCallBuilder {
    id: String,
    name: String,
    args: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::delta::FinishReason;
    use crate::agent::testing::{
        turn_final, turn_tool_call, FakeTool, RecordingSink, Scripted, ScriptedGate,
        ScriptedProvider,
    };

    fn opts(gate: Arc<dyn ConfirmationGate>, max_steps: usize) -> AgentOpts {
        AgentOpts {
            provider_opts: ProviderOpts {
                model: "test-model".to_string(),
                max_tokens: None,
                temperature: None,
                top_p: None,
                cancel: Arc::new(AtomicBool::new(false)),
            },
            max_steps,
            gate,
        }
    }

    fn registry(tool: Arc<dyn Tool>) -> Arc<ToolRegistry> {
        let mut reg = ToolRegistry::new();
        reg.register(tool);
        Arc::new(reg)
    }

    // ---- Happy paths ------------------------------------------------

    #[tokio::test]
    async fn final_answer_returns_without_dispatching() {
        let provider = ScriptedProvider::new(vec![turn_final("hello world")]);
        let sink = RecordingSink::new();
        let gate = ScriptedGate::approving();
        let tool = FakeTool::new("noop");
        let mut messages = vec![ChatMessage::user("hi")];

        let out = run_agent(
            provider.clone(),
            registry(tool.clone()),
            sink.clone(),
            &mut messages,
            opts(gate.clone(), 8),
        )
        .await
        .unwrap();

        assert!(matches!(out, AgentOutcome::Final(ref s) if s == "hello world"));
        assert_eq!(sink.text(), "hello world");
        assert_eq!(sink.kinds(), vec!["token", "done"]);
        assert_eq!(gate.ask_count(), 0, "no tool calls means no gate prompts");
        assert!(!tool.was_dispatched());
        assert_eq!(provider.turns_taken(), 1);
    }

    #[tokio::test]
    async fn tool_call_dispatches_then_second_turn_finalizes() {
        let provider = ScriptedProvider::new(vec![
            turn_tool_call("call-1", "noop", r#"{"x":42}"#),
            turn_final("done thinking"),
        ]);
        let sink = RecordingSink::new();
        let gate = ScriptedGate::approving();
        let tool = FakeTool::new("noop");
        let mut messages = vec![ChatMessage::user("go")];

        let out = run_agent(
            provider.clone(),
            registry(tool.clone()),
            sink.clone(),
            &mut messages,
            opts(gate.clone(), 8),
        )
        .await
        .unwrap();

        assert!(matches!(out, AgentOutcome::Final(_)));
        assert_eq!(provider.turns_taken(), 2);

        // Arguments arrive split across two ToolCallArgs deltas; the
        // loop must concatenate them back into valid JSON before the
        // tool sees them.
        assert_eq!(tool.calls(), vec![serde_json::json!({"x": 42})]);

        assert_eq!(
            sink.kinds(),
            vec!["tool_start", "tool_end", "token", "done"]
        );

        // History threading: assistant turn carrying the call, then a
        // tool turn keyed by the same id, then the final assistant turn.
        assert_eq!(messages.len(), 4);
        match &messages[1] {
            ChatMessage::Assistant { tool_calls, .. } => {
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].id, "call-1");
                assert_eq!(tool_calls[0].arguments, r#"{"x":42}"#);
            }
            other => panic!("expected assistant turn, got {other:?}"),
        }
        match &messages[2] {
            ChatMessage::Tool { tool_call_id, .. } => assert_eq!(tool_call_id, "call-1"),
            other => panic!("expected tool turn, got {other:?}"),
        }

        // The second turn must see the tool result, or the model is
        // answering blind.
        let second = &provider.seen_turns()[1];
        assert!(matches!(second[2], ChatMessage::Tool { .. }));
    }

    #[tokio::test]
    async fn preview_is_rendered_once_and_passed_to_the_gate() {
        let provider = ScriptedProvider::new(vec![
            turn_tool_call("c1", "previewed", "{}"),
            turn_final("ok"),
        ]);
        let sink = RecordingSink::new();
        let gate = ScriptedGate::approving();
        let tool = FakeTool::with_preview("previewed", "+ added line");
        let mut messages = vec![ChatMessage::user("go")];

        run_agent(
            provider,
            registry(tool),
            sink,
            &mut messages,
            opts(gate.clone(), 8),
        )
        .await
        .unwrap();

        assert_eq!(
            gate.asked(),
            vec![("previewed".to_string(), Some("+ added line".to_string()))]
        );
    }

    // ---- Denial -----------------------------------------------------

    #[tokio::test]
    async fn denied_call_does_not_dispatch_but_still_threads_a_tool_message() {
        let provider = ScriptedProvider::new(vec![
            turn_tool_call("c1", "noop", "{}"),
            turn_final("understood"),
        ]);
        let sink = RecordingSink::new();
        let gate = ScriptedGate::denying();
        let tool = FakeTool::new("noop");
        let mut messages = vec![ChatMessage::user("go")];

        run_agent(
            provider,
            registry(tool.clone()),
            sink.clone(),
            &mut messages,
            opts(gate.clone(), 8),
        )
        .await
        .unwrap();

        assert_eq!(gate.ask_count(), 1);
        assert!(
            !tool.was_dispatched(),
            "a denied call must never reach the tool"
        );

        // The model still needs to see *something* for that call id,
        // or the next request is malformed.
        match &messages[2] {
            ChatMessage::Tool {
                tool_call_id,
                content,
            } => {
                assert_eq!(tool_call_id, "c1");
                assert!(content.contains("denied by user"), "got {content}");
            }
            other => panic!("expected tool turn, got {other:?}"),
        }

        // UI still gets a start/end pair so the pill resolves.
        let kinds = sink.kinds();
        assert_eq!(kinds[0], "tool_start");
        assert_eq!(kinds[1], "tool_end");
    }

    // ---- Failure threading ------------------------------------------

    #[tokio::test]
    async fn tool_error_is_threaded_back_so_the_model_can_recover() {
        let provider = ScriptedProvider::new(vec![
            turn_tool_call("c1", "boom", "{}"),
            turn_final("recovered"),
        ]);
        let sink = RecordingSink::new();
        let tool = FakeTool::failing("boom", "disk on fire");
        let mut messages = vec![ChatMessage::user("go")];

        let out = run_agent(
            provider,
            registry(tool),
            sink.clone(),
            &mut messages,
            opts(ScriptedGate::approving(), 8),
        )
        .await
        .unwrap();

        assert!(matches!(out, AgentOutcome::Final(ref s) if s == "recovered"));
        match &messages[2] {
            ChatMessage::Tool { content, .. } => {
                assert!(content.contains("disk on fire"), "got {content}")
            }
            other => panic!("expected tool turn, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_tool_is_an_argument_error_not_a_panic() {
        let provider = ScriptedProvider::new(vec![
            turn_tool_call("c1", "does_not_exist", "{}"),
            turn_final("ok"),
        ]);
        let sink = RecordingSink::new();
        let mut messages = vec![ChatMessage::user("go")];

        run_agent(
            provider,
            registry(FakeTool::new("noop")),
            sink,
            &mut messages,
            opts(ScriptedGate::approving(), 8),
        )
        .await
        .unwrap();

        match &messages[2] {
            ChatMessage::Tool { content, .. } => {
                assert!(content.contains("unknown tool"), "got {content}")
            }
            other => panic!("expected tool turn, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_tool_arguments_error_without_dispatching() {
        let provider = ScriptedProvider::new(vec![
            turn_tool_call("c1", "noop", "{not json"),
            turn_final("ok"),
        ]);
        let sink = RecordingSink::new();
        let tool = FakeTool::new("noop");
        let mut messages = vec![ChatMessage::user("go")];

        run_agent(
            provider,
            registry(tool.clone()),
            sink,
            &mut messages,
            opts(ScriptedGate::approving(), 8),
        )
        .await
        .unwrap();

        assert!(!tool.was_dispatched());
        match &messages[2] {
            ChatMessage::Tool { content, .. } => {
                assert!(content.contains("not valid JSON"), "got {content}")
            }
            other => panic!("expected tool turn, got {other:?}"),
        }
    }

    // ---- Cancellation -----------------------------------------------

    #[tokio::test]
    async fn cancel_before_first_turn_returns_cancelled() {
        let provider = ScriptedProvider::new(vec![]);
        let sink = RecordingSink::new();
        let o = opts(ScriptedGate::approving(), 8);
        o.provider_opts.cancel.store(true, Ordering::Relaxed);
        let mut messages = vec![ChatMessage::user("go")];

        let out = run_agent(
            provider.clone(),
            registry(FakeTool::new("noop")),
            sink.clone(),
            &mut messages,
            o,
        )
        .await
        .unwrap();

        assert!(matches!(out, AgentOutcome::Cancelled));
        assert_eq!(sink.kinds(), vec!["cancelled"]);
        assert_eq!(
            provider.turns_taken(),
            0,
            "must not open a stream after cancel"
        );
    }

    #[tokio::test]
    async fn cancelled_finish_reason_short_circuits_the_turn() {
        let provider = ScriptedProvider::new(vec![vec![
            AgentDelta::Content("partial".to_string()).into(),
            AgentDelta::Done {
                finish_reason: FinishReason::Cancelled,
            }
            .into(),
        ]]);
        let sink = RecordingSink::new();
        let tool = FakeTool::new("noop");
        let mut messages = vec![ChatMessage::user("go")];

        let out = run_agent(
            provider,
            registry(tool.clone()),
            sink.clone(),
            &mut messages,
            opts(ScriptedGate::approving(), 8),
        )
        .await
        .unwrap();

        assert!(matches!(out, AgentOutcome::Cancelled));
        assert!(!tool.was_dispatched());
        // The partial text is still recorded in history even though the
        // turn was abandoned.
        assert!(matches!(
            &messages[1],
            ChatMessage::Assistant { content, .. } if content == "partial"
        ));
    }

    // ---- Bail-outs --------------------------------------------------

    #[tokio::test]
    async fn max_steps_exhaustion_errors_and_emits_error_event() {
        // Every turn asks for another tool call, so the loop can only
        // stop by hitting the cap.
        let provider = ScriptedProvider::new(vec![
            turn_tool_call("c1", "noop", "{}"),
            turn_tool_call("c2", "noop", "{}"),
        ]);
        let sink = RecordingSink::new();
        let mut messages = vec![ChatMessage::user("go")];

        let err = run_agent(
            provider.clone(),
            registry(FakeTool::new("noop")),
            sink.clone(),
            &mut messages,
            opts(ScriptedGate::approving(), 2),
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("max_steps=2"), "got {err}");
        assert_eq!(provider.turns_taken(), 2);
        assert_eq!(*sink.kinds().last().unwrap(), "error");
    }

    #[tokio::test]
    async fn tool_calls_finish_reason_with_no_calls_finalizes_instead_of_looping() {
        // A provider can claim `tool_calls` and then emit none. Without
        // the guard this spins until max_steps with zero progress.
        let provider = ScriptedProvider::new(vec![vec![
            AgentDelta::Content("nothing to call".to_string()).into(),
            AgentDelta::Done {
                finish_reason: FinishReason::ToolCalls,
            }
            .into(),
        ]]);
        let sink = RecordingSink::new();
        let mut messages = vec![ChatMessage::user("go")];

        let out = run_agent(
            provider.clone(),
            registry(FakeTool::new("noop")),
            sink,
            &mut messages,
            opts(ScriptedGate::approving(), 8),
        )
        .await
        .unwrap();

        assert!(matches!(out, AgentOutcome::Final(ref s) if s == "nothing to call"));
        assert_eq!(provider.turns_taken(), 1);
    }

    // ---- Mid-stream provider failure (review finding 5.5) -----------

    #[tokio::test]
    async fn mid_stream_error_preserves_partial_text_and_emits_error() {
        let provider = ScriptedProvider::new(vec![vec![
            AgentDelta::Content("partial answ".to_string()).into(),
            Scripted::Err("connection reset".to_string()),
        ]]);
        let sink = RecordingSink::new();
        let mut messages = vec![ChatMessage::user("go")];

        let out = run_agent(
            provider,
            registry(FakeTool::new("noop")),
            sink.clone(),
            &mut messages,
            opts(ScriptedGate::approving(), 8),
        )
        .await;

        // The loop must not swallow the failure...
        let err = out.expect_err("mid-stream provider failure must surface");
        assert!(err.to_string().contains("connection reset"), "got {err}");

        // ...must tell the UI rather than just going quiet...
        assert!(
            sink.kinds().contains(&"error"),
            "expected an Error event, got {:?}",
            sink.kinds()
        );

        // ...and must not drop text the user already saw on screen.
        assert!(
            matches!(&messages[1], ChatMessage::Assistant { content, .. } if content == "partial answ"),
            "partial assistant text was dropped: {:?}",
            messages
        );
    }
}

/// Gate-policy tests.
///
/// The shells' real gates (`TauriConfirmationGate`, `TuiConfirmationGate`)
/// need an `AppHandle` / an mpsc channel, so they are not constructible
/// here. What *is* testable in `rezon-core` is the invariant both are
/// written against: the loop always consults the gate, never dispatches
/// a denied call, and asks exactly once per call. A gate that ignores
/// `requires_confirmation()` — the bug the shells had — shows up as an
/// unexpected `was_dispatched()`.
#[cfg(test)]
mod gate_policy_tests {
    use super::*;
    use crate::agent::testing::{
        turn_final, turn_tool_call, FakeTool, RecordingSink, ScriptedGate, ScriptedProvider,
    };

    fn run_with(
        gate: Arc<dyn ConfirmationGate>,
        tool: Arc<dyn Tool>,
    ) -> impl std::future::Future<Output = Result<AgentOutcome>> {
        let provider =
            ScriptedProvider::new(vec![turn_tool_call("c1", "t", "{}"), turn_final("ok")]);
        let mut reg = ToolRegistry::new();
        reg.register(tool);
        let opts = AgentOpts {
            provider_opts: ProviderOpts {
                model: "m".to_string(),
                max_tokens: None,
                temperature: None,
                top_p: None,
                cancel: Arc::new(AtomicBool::new(false)),
            },
            max_steps: 8,
            gate,
        };
        async move {
            let mut messages = vec![ChatMessage::user("go")];
            run_agent(
                provider,
                Arc::new(reg),
                RecordingSink::new(),
                &mut messages,
                opts,
            )
            .await
        }
    }

    #[tokio::test]
    async fn every_tool_call_reaches_the_gate_exactly_once() {
        let gate = ScriptedGate::approving();
        let tool = FakeTool::new("t");
        run_with(gate.clone(), tool.clone()).await.unwrap();
        assert_eq!(gate.ask_count(), 1);
        assert!(tool.was_dispatched());
    }

    #[tokio::test]
    async fn a_denying_gate_is_authoritative_over_the_tools_own_declaration() {
        // `FakeTool::new` declares requires_confirmation() == false.
        // A gate may still refuse it: the tool's declaration is a
        // floor, not a ceiling.
        let gate = ScriptedGate::denying();
        let tool = FakeTool::new("t");
        run_with(gate.clone(), tool.clone()).await.unwrap();
        assert_eq!(gate.ask_count(), 1);
        assert!(
            !tool.was_dispatched(),
            "gate denial must win over a tool that declares itself safe"
        );
    }

    #[tokio::test]
    async fn confirmation_required_tool_is_not_dispatched_when_denied() {
        let gate = ScriptedGate::denying();
        // `with_preview` sets requires_confirmation() == true.
        let tool = FakeTool::with_preview("t", "+ writes a file");
        run_with(gate.clone(), tool.clone()).await.unwrap();
        assert!(!tool.was_dispatched());
        assert_eq!(
            gate.asked(),
            vec![("t".to_string(), Some("+ writes a file".to_string()))]
        );
    }
}
