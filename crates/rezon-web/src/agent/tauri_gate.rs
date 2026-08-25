// Tauri implementation of `ConfirmationGate`.
//
// Two inputs decide each call, and they are not equally trusted:
//
//   1. `permissions` — the per-tool "ask"/"always"/"disable" map the
//      frontend sends with the request. It is a *hint*. It can lower
//      risk (disable, or escalate a harmless tool to a prompt) but it
//      cannot by itself authorize a tool that declares
//      `requires_confirmation()`.
//   2. `AgentState::always_grants` — standing grants recorded
//      backend-side by the `grant_tool_always` command, which is its
//      own explicit user action. This is what can actually clear the
//      floor for a side-effecting tool.
//
// The asymmetry is the point. Without it, anything that can call
// `agent_chat` — a frontend bug, a bad state restore, a compromised
// webview — auto-approves `shell_exec` by putting one string in a map,
// and the backend cannot distinguish that from a real user decision.
//
// The rule itself is `rezon_core::agent::confirm::decide`, shared with
// the TUI gate and tested there as an exhaustive table. This file owns
// only the I/O: gathering the three inputs and, when the answer is
// Prompt, actually asking.
//
// When the decision is Prompt, the gate allocates a confirmation_id,
// registers a oneshot in `AgentState`, emits `agent-tool-confirm`, and
// awaits the user's reply, which `confirm_tool_call` resolves.

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};

use crate::agent::commands::AgentState;
use crate::agent::confirm::{
    decide, next_confirmation_id, ConfirmationGate, ConfirmationOutcome, GateDecision,
    ToolPermission,
};
use crate::agent::tool::ToolCall;

pub struct TauriConfirmationGate {
    app: AppHandle,
    /// Frontend-supplied per-tool permissions for this run. Advisory;
    /// see the module comment. Tools not present default to "ask".
    permissions: HashMap<String, String>,
    /// `tool name -> Tool::requires_confirmation()`, snapshotted from
    /// the registry at construction so the gate does not need to
    /// borrow it per call.
    confirm_required: HashMap<String, bool>,
}

impl TauriConfirmationGate {
    pub fn new(
        app: AppHandle,
        permissions: HashMap<String, String>,
        confirm_required: HashMap<String, bool>,
    ) -> Self {
        Self {
            app,
            permissions,
            confirm_required,
        }
    }
}

#[async_trait]
impl ConfirmationGate for TauriConfirmationGate {
    async fn ask(&self, call: &ToolCall, preview: Option<&str>) -> ConfirmationOutcome {
        let permission = self
            .permissions
            .get(&call.name)
            .map(|s| ToolPermission::parse(s))
            .unwrap_or(ToolPermission::Ask);
        // Absent from the snapshot => treat as side-effecting. An
        // unrecognized tool is not one to wave through.
        let requires_confirmation = self
            .confirm_required
            .get(&call.name)
            .copied()
            .unwrap_or(true);
        let granted = {
            let state = self.app.state::<AgentState>();
            state.has_always_grant(&call.name)
        };

        match decide(permission, requires_confirmation, granted) {
            GateDecision::Approve => ConfirmationOutcome::Approved,
            GateDecision::Deny => ConfirmationOutcome::Denied,
            GateDecision::Prompt => prompt_user(&self.app, call, preview).await,
        }
    }
}

async fn prompt_user(
    app: &AppHandle,
    call: &ToolCall,
    preview: Option<&str>,
) -> ConfirmationOutcome {
    let id = next_confirmation_id();

    // Register synchronously, drop the State borrow before awaiting.
    let rx = {
        let state = app.state::<AgentState>();
        state.register_pending_confirm(id.clone())
    };

    let _ = app.emit(
        "agent-tool-confirm",
        &json!({
            "confirmationId": id,
            "name": call.name,
            "arguments": call.arguments,
            // Preview is omitted (rather than null) when the tool
            // doesn't provide one; the frontend should fall back to
            // rendering `arguments` in that case.
            "preview": preview,
        }),
    );

    match rx.await {
        Ok(approved) => {
            if approved {
                ConfirmationOutcome::Approved
            } else {
                ConfirmationOutcome::Denied
            }
        }
        Err(_) => {
            // Sender dropped (run cancelled / app shutting down).
            let state = app.state::<AgentState>();
            state.cancel_pending_confirm(&id);
            ConfirmationOutcome::Denied
        }
    }
}
