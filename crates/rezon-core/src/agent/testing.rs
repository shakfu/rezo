//! Test doubles for driving `run_agent` without a model, a network,
//! or a GGUF file.
//!
//! `Provider::stream` returns a `BoxStream`, so a provider that just
//! replays a canned delta sequence is enough to exercise every branch
//! of the loop: tool-call reassembly, the gate, cancellation, the
//! `max_steps` bail, and mid-stream provider failure.
//!
//! Compiled only under `cfg(test)`. If these ever need to be shared
//! with an integration test or another crate, move the module behind a
//! `testing` feature rather than making it unconditionally public —
//! `ScriptedProvider` asserts on misuse and is not fit for production.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use serde_json::{json, Value};

use crate::agent::confirm::{ConfirmationGate, ConfirmationOutcome};
use crate::agent::delta::AgentDelta;
use crate::agent::event::{AgentEvent, EventSink};
use crate::agent::message::ChatMessage;
use crate::agent::provider::{Provider, ProviderOpts};
use crate::agent::tool::{Tool, ToolCall, ToolContext, ToolError};

/// One scripted item in a turn: either a delta to yield or an error to
/// fail the stream with. `anyhow::Error` is not `Clone`, so the error
/// case carries a message and is rebuilt at yield time.
#[derive(Debug, Clone)]
pub enum Scripted {
    Delta(AgentDelta),
    Err(String),
}

impl From<AgentDelta> for Scripted {
    fn from(d: AgentDelta) -> Self {
        Scripted::Delta(d)
    }
}

/// Replays a pre-scripted delta sequence per turn.
///
/// Each call to `stream` pops the next turn's script, so a multi-step
/// agent run is expressed as a `Vec` of `Vec`s. Running out of scripted
/// turns is a panic rather than a silent empty stream: it means the
/// loop iterated more times than the test expected, which is exactly
/// the kind of bug these tests exist to catch.
pub struct ScriptedProvider {
    turns: Mutex<VecDeque<Vec<Scripted>>>,
    /// Messages as seen at the start of each turn. Lets a test assert
    /// on what the loop actually fed back to the model — the tool
    /// result threading is easy to get wrong and invisible otherwise.
    seen: Mutex<Vec<Vec<ChatMessage>>>,
}

impl ScriptedProvider {
    pub fn new(turns: Vec<Vec<Scripted>>) -> Arc<Self> {
        Arc::new(Self {
            turns: Mutex::new(turns.into()),
            seen: Mutex::new(Vec::new()),
        })
    }

    /// Messages the loop passed in, one entry per turn.
    pub fn seen_turns(&self) -> Vec<Vec<ChatMessage>> {
        self.seen.lock().unwrap().clone()
    }

    /// How many turns were actually requested.
    pub fn turns_taken(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn stream(
        &self,
        messages: &[ChatMessage],
        _tools: &[Value],
        _opts: &ProviderOpts,
    ) -> Result<BoxStream<'static, Result<AgentDelta>>> {
        self.seen.lock().unwrap().push(messages.to_vec());
        let script = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .expect("ScriptedProvider ran out of turns: the loop iterated more than expected");
        let items = script.into_iter().map(|s| match s {
            Scripted::Delta(d) => Ok(d),
            Scripted::Err(msg) => Err(anyhow!(msg)),
        });
        Ok(Box::pin(stream::iter(items.collect::<Vec<_>>())))
    }
}

/// Collects every event the loop emits, in order.
#[derive(Default)]
pub struct RecordingSink {
    events: Mutex<Vec<AgentEvent>>,
}

impl RecordingSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn events(&self) -> Vec<AgentEvent> {
        self.events.lock().unwrap().clone()
    }

    /// Discriminant names in emission order, for terse assertions on
    /// event *shape* without matching every payload.
    pub fn kinds(&self) -> Vec<&'static str> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| match e {
                AgentEvent::Token(_) => "token",
                AgentEvent::Thinking(_) => "thinking",
                AgentEvent::ToolStart { .. } => "tool_start",
                AgentEvent::ToolEnd { .. } => "tool_end",
                AgentEvent::Stats(_) => "stats",
                AgentEvent::Done { .. } => "done",
                AgentEvent::Cancelled => "cancelled",
                AgentEvent::Error(_) => "error",
            })
            .collect()
    }

    /// Concatenated `Token` payloads — the visible assistant text.
    pub fn text(&self) -> String {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Token(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }
}

impl EventSink for RecordingSink {
    fn emit(&self, event: AgentEvent) {
        self.events.lock().unwrap().push(event);
    }
}

/// Gate with a scripted verdict per call, recording what it was asked.
pub struct ScriptedGate {
    outcomes: Mutex<VecDeque<ConfirmationOutcome>>,
    /// Fallback once the scripted outcomes run out.
    default: ConfirmationOutcome,
    asked: Mutex<Vec<(String, Option<String>)>>,
}

impl ScriptedGate {
    /// Approves everything, but records each ask.
    pub fn approving() -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(VecDeque::new()),
            default: ConfirmationOutcome::Approved,
            asked: Mutex::new(Vec::new()),
        })
    }

    /// Denies everything.
    pub fn denying() -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(VecDeque::new()),
            default: ConfirmationOutcome::Denied,
            asked: Mutex::new(Vec::new()),
        })
    }

    /// Answers calls in order from `outcomes`, then falls back to
    /// approving.
    pub fn scripted(outcomes: Vec<ConfirmationOutcome>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(outcomes.into()),
            default: ConfirmationOutcome::Approved,
            asked: Mutex::new(Vec::new()),
        })
    }

    /// `(tool_name, preview)` for each call, in order.
    pub fn asked(&self) -> Vec<(String, Option<String>)> {
        self.asked.lock().unwrap().clone()
    }

    pub fn ask_count(&self) -> usize {
        self.asked.lock().unwrap().len()
    }
}

#[async_trait]
impl ConfirmationGate for ScriptedGate {
    async fn ask(&self, call: &ToolCall, preview: Option<&str>) -> ConfirmationOutcome {
        self.asked
            .lock()
            .unwrap()
            .push((call.name.clone(), preview.map(str::to_string)));
        self.outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(self.default)
    }
}

/// Configurable tool. Records its invocations and returns a canned
/// result, an error, or sets a flag so a test can assert the tool was
/// (or was not) reached.
pub struct FakeTool {
    name: String,
    requires_confirmation: bool,
    preview: Option<String>,
    /// `Ok` value to return, or an error message to fail with.
    result: std::result::Result<Value, String>,
    calls: Mutex<Vec<Value>>,
    /// Flipped the first time `dispatch` runs. The clearest signal
    /// that a denied call really did not execute.
    dispatched: AtomicBool,
}

impl FakeTool {
    pub fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_string(),
            requires_confirmation: false,
            preview: None,
            result: Ok(json!({"ok": true})),
            calls: Mutex::new(Vec::new()),
            dispatched: AtomicBool::new(false),
        })
    }

    /// Tool whose dispatch always fails, for the error-threading path.
    pub fn failing(name: &str, msg: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_string(),
            requires_confirmation: false,
            preview: None,
            result: Err(msg.to_string()),
            calls: Mutex::new(Vec::new()),
            dispatched: AtomicBool::new(false),
        })
    }

    /// Tool that offers a `preview()`, to check the loop renders it
    /// once and hands it to the gate.
    pub fn with_preview(name: &str, preview: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_string(),
            requires_confirmation: true,
            preview: Some(preview.to_string()),
            result: Ok(json!({"ok": true})),
            calls: Mutex::new(Vec::new()),
            dispatched: AtomicBool::new(false),
        })
    }

    pub fn was_dispatched(&self) -> bool {
        self.dispatched.load(Ordering::Relaxed)
    }

    /// Parsed arguments for each dispatch, in order.
    pub fn calls(&self) -> Vec<Value> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl Tool for FakeTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "fake tool for tests"
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    fn requires_confirmation(&self) -> bool {
        self.requires_confirmation
    }

    fn preview(&self, _args: &Value) -> Option<String> {
        self.preview.clone()
    }

    async fn dispatch(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> std::result::Result<Value, ToolError> {
        self.dispatched.store(true, Ordering::Relaxed);
        self.calls.lock().unwrap().push(args);
        match &self.result {
            Ok(v) => Ok(v.clone()),
            Err(msg) => Err(ToolError::Runtime(anyhow!(msg.clone()))),
        }
    }
}

// ---- Script-building shorthands -------------------------------------

/// A turn that streams `text` and stops.
pub fn turn_final(text: &str) -> Vec<Scripted> {
    use crate::agent::delta::FinishReason;
    vec![
        AgentDelta::Content(text.to_string()).into(),
        AgentDelta::Done {
            finish_reason: FinishReason::Stop,
        }
        .into(),
    ]
}

/// A turn that requests one tool call, with `args` split across two
/// fragments so the test also covers argument reassembly.
pub fn turn_tool_call(id: &str, name: &str, args: &str) -> Vec<Scripted> {
    use crate::agent::delta::FinishReason;
    let (a, b) = args.split_at(args.len() / 2);
    vec![
        AgentDelta::ToolCallStart {
            index: 0,
            id: id.to_string(),
            name: name.to_string(),
        }
        .into(),
        AgentDelta::ToolCallArgs {
            index: 0,
            fragment: a.to_string(),
        }
        .into(),
        AgentDelta::ToolCallArgs {
            index: 0,
            fragment: b.to_string(),
        }
        .into(),
        AgentDelta::Done {
            finish_reason: FinishReason::ToolCalls,
        }
        .into(),
    ]
}
