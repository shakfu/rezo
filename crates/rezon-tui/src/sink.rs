// Unified UI event channel + sink implementations. Chat and agent
// paths both stream events into the REPL through a single mpsc so the
// loop can `tokio::select!` against one receiver.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use rezon_core::agent::{
    decide, AgentEvent, ConfirmationGate, ConfirmationOutcome, EventSink, GateDecision, ToolCall,
    ToolPermission,
};
use rezon_core::llm::{ChatMsg, ChatSink, ChatStats};
use serde_json::Value;
use tokio::sync::{mpsc::UnboundedSender, oneshot};

#[derive(Debug, Clone)]
pub struct StatsLite {
    pub prompt_tokens: Option<u32>,
    pub gen_tokens: u32,
    pub duration_ms: u64,
}

#[derive(Debug)]
pub enum UiEvent {
    Token(String),
    /// Reasoning ("thinking") delta. The REPL renders this in dim
    /// italic when `show_thinking` is on; otherwise it's dropped.
    Thinking(String),
    Stats(StatsLite),
    Done,
    Error(String),
    ToolStart {
        name: String,
    },
    ToolEnd {
        ok: bool,
        summary: String,
    },
    /// Tool awaiting user approval. The agent task blocks on `tx`
    /// until the REPL writes `true` / `false`. Dropping `tx` reads
    /// as denial.
    Confirm {
        name: String,
        arguments: String,
        /// Optional pre-rendered preview from `Tool::preview`. When
        /// `Some`, the REPL shows this in place of the raw JSON args.
        preview: Option<String>,
        tx: oneshot::Sender<bool>,
    },
    /// Final agent-loop message vector, serialised back to
    /// `ChatMsg`. The REPL replaces the active conversation's
    /// `messages` with this snapshot so the next agent run sees the
    /// real assistant `tool_calls` + tool-role replies rather than
    /// just the pretty pills shown live.
    AgentHistory(Vec<ChatMsg>),
}

pub struct TuiChatSink {
    tx: UnboundedSender<UiEvent>,
}

impl TuiChatSink {
    pub fn new(tx: UnboundedSender<UiEvent>) -> Self {
        Self { tx }
    }
}

impl ChatSink for TuiChatSink {
    fn on_token(&self, delta: &str) {
        let _ = self.tx.send(UiEvent::Token(delta.to_string()));
    }
    fn on_stats(&self, stats: &ChatStats) {
        let _ = self.tx.send(UiEvent::Stats(StatsLite {
            prompt_tokens: stats.prompt_tokens,
            gen_tokens: stats.gen_tokens,
            duration_ms: stats.duration_ms,
        }));
    }
    fn on_done(&self, _full: &str) {
        let _ = self.tx.send(UiEvent::Done);
    }
}

pub struct TuiAgentSink {
    tx: UnboundedSender<UiEvent>,
}

impl TuiAgentSink {
    pub fn new(tx: UnboundedSender<UiEvent>) -> Self {
        Self { tx }
    }
}

impl EventSink for TuiAgentSink {
    fn emit(&self, event: AgentEvent) {
        let ui = match event {
            AgentEvent::Token(s) => UiEvent::Token(s),
            // Forward thinking deltas to the REPL; the REPL decides
            // whether to render them based on the active
            // conversation's `show_thinking` setting.
            AgentEvent::Thinking(s) => UiEvent::Thinking(s),
            AgentEvent::ToolStart { name, .. } => UiEvent::ToolStart { name },
            AgentEvent::ToolEnd {
                ok, result, error, ..
            } => UiEvent::ToolEnd {
                ok,
                summary: summarize_tool_result(ok, result.as_ref(), error.as_deref()),
            },
            AgentEvent::Stats(s) => UiEvent::Stats(StatsLite {
                prompt_tokens: s.prompt_tokens,
                gen_tokens: s.gen_tokens,
                duration_ms: s.duration_ms,
            }),
            // The agent loop's `Done` is suppressed here — the
            // spawn block in `agent.rs` sends `AgentHistory` then
            // `Done` after `run_agent` returns, so the REPL gets
            // history persisted before its terminator fires.
            AgentEvent::Done { .. } => return,
            AgentEvent::Cancelled => UiEvent::Error("cancelled".to_string()),
            AgentEvent::Error(e) => UiEvent::Error(e),
        };
        let _ = self.tx.send(ui);
    }
}

fn summarize_tool_result(ok: bool, result: Option<&Value>, error: Option<&str>) -> String {
    if !ok {
        return error.unwrap_or("error").to_string();
    }
    match result {
        Some(v) => {
            let s = v.to_string();
            if s.chars().count() > 200 {
                let truncated: String = s.chars().take(200).collect();
                format!("{truncated}…")
            } else {
                s
            }
        }
        None => "ok".to_string(),
    }
}

pub struct TuiConfirmationGate {
    tx: UnboundedSender<UiEvent>,
    cancelled: Arc<AtomicBool>,
    /// `tool name -> Tool::requires_confirmation()`, snapshotted from
    /// the registry when the run is spawned.
    confirm_required: HashMap<String, bool>,
    /// Tools the user disabled. The registry already strips these, so
    /// reaching the gate with one means something went wrong upstream;
    /// denying is the safe answer either way.
    disabled: Vec<String>,
}

impl TuiConfirmationGate {
    pub fn new(
        tx: UnboundedSender<UiEvent>,
        cancelled: Arc<AtomicBool>,
        confirm_required: HashMap<String, bool>,
        disabled: Vec<String>,
    ) -> Self {
        Self {
            tx,
            cancelled,
            confirm_required,
            disabled,
        }
    }
}

#[async_trait]
impl ConfirmationGate for TuiConfirmationGate {
    async fn ask(&self, call: &ToolCall, preview: Option<&str>) -> ConfirmationOutcome {
        if self.cancelled.load(Ordering::Relaxed) {
            return ConfirmationOutcome::Denied;
        }

        // Same decision table as the GUI gate, but the default
        // permission is `Always`, not `Ask`.
        //
        // `Ask` means "the user asked to be prompted for this specific
        // tool". The GUI has a per-tool control that can express that;
        // the TUI has no such setting, so defaulting to `Ask` would
        // mean every tool carries a preference nobody set, and
        // `decide(Ask, false, _)` prompts — reinstating the prompt on
        // `current_time` that this gate exists to avoid. `Always`
        // says "no per-tool preference here", which lets the floor be
        // the only thing that prompts: side-effecting tools still
        // stop, because `decide` ignores permission for those.
        let permission = if self.disabled.iter().any(|d| d == &call.name) {
            ToolPermission::Disable
        } else {
            ToolPermission::Always
        };
        let requires_confirmation = self
            .confirm_required
            .get(&call.name)
            .copied()
            .unwrap_or(true);
        match decide(permission, requires_confirmation, false) {
            GateDecision::Approve => return ConfirmationOutcome::Approved,
            GateDecision::Deny => return ConfirmationOutcome::Denied,
            GateDecision::Prompt => {}
        }

        let (tx, rx) = oneshot::channel();
        if self
            .tx
            .send(UiEvent::Confirm {
                name: call.name.clone(),
                arguments: call.arguments.clone(),
                preview: preview.map(str::to_string),
                tx,
            })
            .is_err()
        {
            return ConfirmationOutcome::Denied;
        }
        match rx.await {
            Ok(true) => ConfirmationOutcome::Approved,
            _ => ConfirmationOutcome::Denied,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rezon_core::agent::ToolCall;
    use tokio::sync::mpsc::unbounded_channel;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "c1".to_string(),
            name: name.to_string(),
            arguments: "{}".to_string(),
        }
    }

    fn gate(
        confirm_required: &[(&str, bool)],
        disabled: &[&str],
    ) -> (
        TuiConfirmationGate,
        tokio::sync::mpsc::UnboundedReceiver<UiEvent>,
    ) {
        let (tx, rx) = unbounded_channel();
        let g = TuiConfirmationGate::new(
            tx,
            Arc::new(AtomicBool::new(false)),
            confirm_required
                .iter()
                .map(|(n, r)| (n.to_string(), *r))
                .collect(),
            disabled.iter().map(|s| s.to_string()).collect(),
        );
        (g, rx)
    }

    /// Regression test for a bug that shipped and was caught only by
    /// running the app.
    ///
    /// The policy lives in `confirm::decide`, which is exhaustively
    /// tested — but *what this gate feeds it* was not. Passing
    /// `ToolPermission::Ask` as the TUI default looked harmless and is
    /// correct in the GUI, where `Ask` reflects a per-tool setting the
    /// user actually chose. The TUI has no such setting, so every tool
    /// arrived as `Ask` and `decide(Ask, false, _)` prompts — putting
    /// the `current_time` prompt straight back.
    ///
    /// Extracting a pure function makes the rule testable; it does not
    /// make its callers correct. These tests cover the mapping.
    #[tokio::test]
    async fn read_only_tools_are_approved_without_prompting() {
        let (g, mut rx) = gate(&[("current_time", false)], &[]);
        // Bounded: a gate that wrongly prompts here blocks forever
        // waiting for an answer nobody will send, and the failure
        // should read as an assertion, not as a hung suite.
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            g.ask(&call("current_time"), None),
        )
        .await
        .expect("gate prompted for a read-only tool instead of approving it");
        assert_eq!(outcome, ConfirmationOutcome::Approved);
        assert!(
            rx.try_recv().is_err(),
            "a read-only tool must not emit a Confirm event"
        );
    }

    #[tokio::test]
    async fn side_effecting_tools_still_prompt() {
        let (g, mut rx) = gate(&[("shell_exec", true)], &[]);
        // Answer the prompt from the "UI" side so `ask` can finish.
        let task = tokio::spawn(async move {
            match rx.recv().await {
                Some(UiEvent::Confirm { name, tx, .. }) => {
                    assert_eq!(name, "shell_exec");
                    let _ = tx.send(true);
                    true
                }
                other => panic!("expected Confirm, got {other:?}"),
            }
        });
        let outcome = g.ask(&call("shell_exec"), None).await;
        assert_eq!(outcome, ConfirmationOutcome::Approved);
        assert!(task.await.unwrap(), "gate must have prompted");
    }

    #[tokio::test]
    async fn a_denied_prompt_denies_the_call() {
        let (g, mut rx) = gate(&[("shell_exec", true)], &[]);
        tokio::spawn(async move {
            if let Some(UiEvent::Confirm { tx, .. }) = rx.recv().await {
                let _ = tx.send(false);
            }
        });
        assert_eq!(
            g.ask(&call("shell_exec"), None).await,
            ConfirmationOutcome::Denied
        );
    }

    #[tokio::test]
    async fn disabled_tools_are_denied_without_prompting() {
        let (g, mut rx) = gate(&[("shell_exec", true)], &["shell_exec"]);
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            g.ask(&call("shell_exec"), None),
        )
        .await
        .expect("gate prompted for a disabled tool instead of denying it");
        assert_eq!(outcome, ConfirmationOutcome::Denied);
        assert!(rx.try_recv().is_err(), "disable must not prompt");
    }

    #[tokio::test]
    async fn an_unknown_tool_prompts_rather_than_being_waved_through() {
        let (g, mut rx) = gate(&[("current_time", false)], &[]);
        tokio::spawn(async move {
            if let Some(UiEvent::Confirm { tx, .. }) = rx.recv().await {
                let _ = tx.send(false);
            }
        });
        assert_eq!(
            g.ask(&call("mystery_tool"), None).await,
            ConfirmationOutcome::Denied
        );
    }

    #[tokio::test]
    async fn a_cancelled_run_denies_without_prompting() {
        let (tx, mut rx) = unbounded_channel();
        let cancelled = Arc::new(AtomicBool::new(true));
        let g = TuiConfirmationGate::new(tx, cancelled, HashMap::new(), Vec::new());
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            g.ask(&call("shell_exec"), None),
        )
        .await
        .expect("gate prompted despite the run being cancelled");
        assert_eq!(outcome, ConfirmationOutcome::Denied);
        assert!(rx.try_recv().is_err());
    }
}
