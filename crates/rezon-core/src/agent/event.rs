// What the agent loop emits to the UI. Decoupled from Tauri via the
// EventSink trait so the loop can be tested or driven from a CLI
// without an AppHandle.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::delta::StreamStats;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Visible content delta. Drives the chat bubble.
    Token(String),

    /// Reasoning/thinking delta. Per design decision #4 the UI shows
    /// only an active "thinking..." indicator while these stream;
    /// content is buffered for on-demand expansion when the block
    /// closes.
    Thinking(String),

    /// Tool dispatch starting. UI shows a pill with the tool name
    /// and a running spinner. Per design decision #5 args are NOT
    /// rendered live.
    ToolStart { id: String, name: String },

    /// Tool dispatch complete. Pill collapses to ok/error state; user
    /// can expand to see args + result.
    ToolEnd {
        id: String,
        ok: bool,
        result: Option<Value>,
        error: Option<String>,
    },

    /// Per-turn stream stats (token counts, timing).
    Stats(StreamStats),

    /// Final assistant text. Loop terminated cleanly.
    Done { content: String },

    /// Run was cancelled mid-flight.
    Cancelled,

    /// Loop bailed because of an unrecoverable error.
    Error(String),
}

/// Sink for `AgentEvent`s. Implementations:
///   - `TauriEventSink` (phase 3): forwards to `app.emit` calls
///   - `LogEventSink` (this file): prints to stdout, used by example
///     binaries and tests
pub trait EventSink: Send + Sync {
    fn emit(&self, event: AgentEvent);
}

/// Stdout-printing sink for examples and tests. Each event renders
/// on its own line with a tag so it is easy to scan in a terminal.
pub struct LogEventSink;

impl EventSink for LogEventSink {
    fn emit(&self, event: AgentEvent) {
        match event {
            AgentEvent::Token(s) => print!("{s}"),
            AgentEvent::Thinking(s) => eprint!("\x1b[2m{s}\x1b[0m"),
            AgentEvent::ToolStart { id, name } => {
                println!("\n[tool-start] id={id} name={name}");
            }
            AgentEvent::ToolEnd {
                id,
                ok,
                result,
                error,
            } => {
                if ok {
                    println!(
                        "[tool-end ok] id={id} result={}",
                        result.unwrap_or(Value::Null)
                    );
                } else {
                    println!("[tool-end err] id={id} error={}", error.unwrap_or_default());
                }
            }
            AgentEvent::Stats(s) => println!("\n[stats] {s:?}"),
            AgentEvent::Done { .. } => println!("\n[done]"),
            AgentEvent::Cancelled => println!("\n[cancelled]"),
            AgentEvent::Error(e) => println!("\n[error] {e}"),
        }
        use std::io::Write;
        std::io::stdout().flush().ok();
    }
}
