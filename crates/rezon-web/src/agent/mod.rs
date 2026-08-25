// Agent module shim. All providers, the loop, the built-in tools
// (including `search_notes`) live in `rezon_core::agent`. This crate
// adds only the Tauri-specific pieces: the command surface, an
// `EventSink` that forwards to `app.emit`, and a `ConfirmationGate`
// that prompts the frontend.

pub mod commands;
pub mod tauri_gate;
pub mod tauri_sink;

pub use rezon_core::agent::{
    cloud, confirm, delta, event, local, loop_, message, provider, run_agent, tool, tools,
    AgentDelta, AgentEvent, AgentOpts, AgentOutcome, ChatMessage, EventSink, FinishReason,
    LocalProvider, LogEventSink, Provider, ProviderOpts, StreamStats, Tool, ToolCall, ToolContext,
    ToolError, ToolRegistry,
};
