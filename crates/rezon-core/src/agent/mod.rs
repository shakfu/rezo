// Provider-agnostic agent loop + supporting types.
//
// The Tauri-specific pieces (TauriEventSink, TauriConfirmationGate, the
// LocalProvider that reaches into LlmState, and the actual tool
// implementations) live in their respective shell crates (rezon-web
// today; rezon-tui in a later phase).

pub mod cloud;
pub mod confirm;
pub mod delta;
pub mod event;
pub mod local;
pub mod loop_;
pub mod message;
pub mod provider;
#[cfg(test)]
pub mod testing;
pub mod tool;
pub mod tools;

pub use cloud::CloudProvider;
pub use confirm::{
    decide, next_confirmation_id, AutoApproveGate, ConfirmationGate, ConfirmationOutcome,
    GateDecision, ToolPermission,
};
pub use delta::{AgentDelta, FinishReason, StreamStats};
pub use event::{AgentEvent, EventSink, LogEventSink};
pub use local::LocalProvider;
pub use loop_::{run_agent, AgentOpts, AgentOutcome};
pub use message::ChatMessage;
pub use provider::{Provider, ProviderOpts};
pub use tool::{Tool, ToolCall, ToolContext, ToolError, ToolRegistry};
pub use tools::{register_core_tools, register_search_notes};
