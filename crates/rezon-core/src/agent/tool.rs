// Provider-neutral Tool trait + registry.
//
// Tools are leaf functions the model can call. The same trait is used
// for the cloud and local agent paths. Schemas are emitted in OpenAI
// shape because both backends consume that format directly:
//   - async-openai's `tools` field
//   - llama-cpp-2's `apply_chat_template_with_tools_oaicompat`

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single tool the model can invoke. Implementations describe their
/// schema and dispatch synchronously w.r.t. the model turn (they may
/// of course do async I/O internally).
#[async_trait]
pub trait Tool: Send + Sync {
    /// Stable identifier surfaced verbatim to the model. Must match
    /// the OpenAI function-name constraints: a-z, A-Z, 0-9, '_', '-',
    /// length <= 64.
    fn name(&self) -> &str;

    /// One-line natural-language description used by the model for
    /// tool selection. Keep it tight and concrete.
    fn description(&self) -> &str;

    /// JSON Schema for the parameters object. Returned as a
    /// `serde_json::Value` so adapters can wrap it into either
    /// OpenAI's function-calling shape or any other backend format.
    fn parameters(&self) -> Value;

    /// Whether this tool requires explicit user confirmation before
    /// dispatch. Default false; override to true for any tool that
    /// has side effects beyond reading information (shell exec,
    /// file write, network mutations).
    fn requires_confirmation(&self) -> bool {
        false
    }

    /// Optional human-readable preview of what dispatch will do,
    /// given the parsed `args`. Confirmation UIs render this in
    /// place of the raw JSON args when it's `Some`. By convention:
    ///   * Lines starting with `+ ` are additions (green-tinted)
    ///   * Lines starting with `- ` are removals (red-tinted)
    ///   * Lines starting with `  ` (two spaces) are unchanged
    ///     context. Other lines render as plain text.
    /// Returning `None` falls back to the raw arguments JSON.
    fn preview(&self, _args: &Value) -> Option<String> {
        None
    }

    /// Execute the tool. `args` is the parsed parameters object as
    /// JSON; implementations are responsible for argument validation
    /// and coercion. The returned `Value` is serialized into a `tool`
    /// role message for the next turn's prompt.
    async fn dispatch(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError>;
}

/// Ambient state passed to every tool invocation. Tools should poll
/// `ctx.cancel` for long-running operations and abort promptly.
pub struct ToolContext {
    pub cancel: Arc<AtomicBool>,
}

/// Reasons a tool dispatch can fail. The agent loop maps each variant
/// onto a `tool` role message describing the failure so the model can
/// recover (e.g. retry with different arguments).
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// Arguments did not match the tool's schema.
    #[error("argument: {0}")]
    Argument(String),

    /// Tool raised an error during execution.
    #[error("runtime: {0}")]
    Runtime(#[from] anyhow::Error),
}

/// Identifies a single tool invocation within a turn. `id` is provider-
/// assigned (matches the `tool_call_id` used to thread the result back
/// into the conversation). `arguments` is the raw JSON string the
/// model emitted; tools parse it themselves so they can produce good
/// argument-error messages.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON string the model emitted. Tools parse this in their
    /// dispatch implementation; keeping it as a string avoids losing
    /// information when the model emits invalid JSON.
    pub arguments: String,
}

/// Set of tools available for an agent run. Per decision #2 there is
/// no per-conversation tool gate; the registry is constructed once
/// and reused for every run.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }

    pub fn tools(&self) -> impl Iterator<Item = &Arc<dyn Tool>> {
        self.tools.values()
    }

    /// Drop tools whose names appear in `exclude`. Used by the Tauri
    /// command path to honor the user's per-tool "disable" setting
    /// before handing the registry to the loop.
    pub fn without(mut self, exclude: &[String]) -> Self {
        for name in exclude {
            self.tools.remove(name);
        }
        self
    }

    /// OpenAI-shaped tools array, ready to feed both `async-openai`'s
    /// `tools` request field and llama-cpp-2's
    /// `apply_chat_template_with_tools_oaicompat` `tools_json` arg.
    pub fn openai_schemas(&self) -> Vec<Value> {
        self.tools
            .values()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name(),
                        "description": t.description(),
                        "parameters": t.parameters(),
                    }
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::register_core_tools;

    #[test]
    fn registry_register_get_and_names() {
        let mut reg = ToolRegistry::new();
        register_core_tools(&mut reg);
        let names: Vec<&str> = reg.names().collect();
        for expected in ["current_time", "file_read", "shell_exec", "web_fetch"] {
            assert!(
                names.contains(&expected),
                "missing tool `{expected}` in {names:?}"
            );
        }
        assert!(reg.get("current_time").is_some());
        assert!(reg.get("does-not-exist").is_none());
    }

    #[test]
    fn registry_without_drops_named_tools() {
        let mut reg = ToolRegistry::new();
        register_core_tools(&mut reg);
        let trimmed = reg.without(&["shell_exec".to_string(), "web_fetch".to_string()]);
        let names: Vec<&str> = trimmed.names().collect();
        assert!(!names.contains(&"shell_exec"));
        assert!(!names.contains(&"web_fetch"));
        assert!(names.contains(&"current_time"));
    }

    #[test]
    fn openai_schemas_shape() {
        let mut reg = ToolRegistry::new();
        register_core_tools(&mut reg);
        let schemas = reg.openai_schemas();
        assert!(!schemas.is_empty());
        for s in &schemas {
            assert_eq!(s.get("type").and_then(|v| v.as_str()), Some("function"));
            let func = s.get("function").expect("function payload");
            assert!(func.get("name").and_then(|v| v.as_str()).is_some());
            assert!(func.get("description").and_then(|v| v.as_str()).is_some());
            assert!(func.get("parameters").is_some());
        }
    }
}
