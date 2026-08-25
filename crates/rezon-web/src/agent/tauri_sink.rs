// EventSink implementation that forwards AgentEvents to the
// frontend via Tauri's app.emit. Each variant is mapped to a
// distinct event name with a camelCase JSON payload.

use serde_json::json;
use tauri::{AppHandle, Emitter};

use crate::agent::event::{AgentEvent, EventSink};

pub struct TauriEventSink {
    app: AppHandle,
}

impl TauriEventSink {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

impl EventSink for TauriEventSink {
    fn emit(&self, event: AgentEvent) {
        let app = &self.app;
        match event {
            AgentEvent::Token(s) => {
                let _ = app.emit("agent-token", &s);
            }
            AgentEvent::Thinking(s) => {
                let _ = app.emit("agent-thinking", &s);
            }
            AgentEvent::ToolStart { id, name } => {
                let _ = app.emit("agent-tool-start", &json!({ "id": id, "name": name }));
            }
            AgentEvent::ToolEnd {
                id,
                ok,
                result,
                error,
            } => {
                let _ = app.emit(
                    "agent-tool-end",
                    &json!({
                        "id": id,
                        "ok": ok,
                        "result": result,
                        "error": error,
                    }),
                );
            }
            AgentEvent::Stats(s) => {
                let _ = app.emit("agent-stats", &s);
            }
            AgentEvent::Done { content } => {
                let _ = app.emit("agent-done", &content);
            }
            AgentEvent::Cancelled => {
                let _ = app.emit("agent-cancelled", ());
            }
            AgentEvent::Error(e) => {
                let _ = app.emit("agent-error", &e);
            }
        }
    }
}
