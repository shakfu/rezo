// Agent session orchestration: builds a provider + tool registry +
// event sink + confirmation gate, then runs `rezon_core::agent::
// run_agent`. Lives in its own module so `app.rs` stays focused on UI
// state.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use rezon_core::agent::{
    cloud::CloudProvider,
    local::LocalProvider,
    run_agent,
    tools::{register_core_tools, register_search_notes, register_write_note},
    AgentOpts, ChatMessage, ConfirmationGate, Provider, ProviderOpts, ToolRegistry,
};
use rezon_core::llm::{resolve_cloud_config, ChatMsg, ChatOpts, LlmState};

fn chat_messages_to_msgs(msgs: &[ChatMessage]) -> Vec<ChatMsg> {
    msgs.iter()
        .map(|m| match m {
            ChatMessage::System { content } => ChatMsg {
                role: "system".to_string(),
                content: content.clone(),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            ChatMessage::User { content } => ChatMsg {
                role: "user".to_string(),
                content: content.clone(),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            ChatMessage::Assistant {
                content,
                tool_calls,
            } => ChatMsg {
                role: "assistant".to_string(),
                content: content.clone(),
                tool_calls: tool_calls.clone(),
                tool_call_id: None,
            },
            ChatMessage::Tool {
                tool_call_id,
                content,
            } => ChatMsg {
                role: "tool".to_string(),
                content: content.clone(),
                tool_calls: Vec::new(),
                tool_call_id: Some(tool_call_id.clone()),
            },
        })
        .collect()
}
use tokio::sync::mpsc::UnboundedSender;

use crate::sink::{TuiAgentSink, TuiConfirmationGate, UiEvent};
use crate::vault::VaultCtx;

/// Run a single agent turn. Spawns the agent loop on a tokio task;
/// returns a handle whose `cancel` flag the App flips on Esc/Ctrl-C.
pub struct AgentRunHandle {
    pub cancel: Arc<AtomicBool>,
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_agent_run(
    state: Arc<LlmState>,
    chat_opts: ChatOpts,
    history: Vec<ChatMsg>,
    user_input: String,
    tx: UnboundedSender<UiEvent>,
    max_steps: usize,
    vault: Option<&VaultCtx>,
    disabled_tools: &[String],
) -> Result<AgentRunHandle> {
    let messages = build_agent_messages(history, user_input);
    let (provider, model) = build_provider(&state, &chat_opts)?;
    let cancel = Arc::new(AtomicBool::new(false));

    let mut reg = ToolRegistry::new();
    register_core_tools(&mut reg);
    // The model only sees `search_notes` when a vault context exists;
    // the tool itself rejects calls when no vault is open with a
    // human-readable error, so the model can recover.
    if let Some(v) = vault {
        register_search_notes(&mut reg, v.search.clone(), v.embed.clone());
        // `write_note` is paired with `search_notes`: both depend on
        // a vault being open. Confirmation is enforced inside the
        // tool, not at registration time, so the user always gets a
        // prompt before content lands on disk.
        register_write_note(&mut reg, v.search.clone());
    }
    // User-disabled tools are stripped from the registry so the
    // model never sees them.
    let registry = Arc::new(reg.without(disabled_tools));

    let sink: Arc<dyn rezon_core::agent::EventSink> = Arc::new(TuiAgentSink::new(tx.clone()));
    // Each tool's own confirmation requirement, so the gate can skip
    // prompting for read-only tools instead of interrupting on every
    // call.
    let confirm_required: std::collections::HashMap<String, bool> = registry
        .tools()
        .map(|t| (t.name().to_string(), t.requires_confirmation()))
        .collect();
    let gate: Arc<dyn ConfirmationGate> = Arc::new(TuiConfirmationGate::new(
        tx.clone(),
        cancel.clone(),
        confirm_required,
        disabled_tools.to_vec(),
    ));

    let opts = AgentOpts {
        provider_opts: ProviderOpts {
            model,
            max_tokens: None,
            temperature: None,
            top_p: None,
            cancel: cancel.clone(),
        },
        max_steps,
        gate,
    };

    let tx_outer = tx.clone();
    tokio::spawn(async move {
        let mut messages = messages;
        let result = run_agent(provider, registry, sink, &mut messages, opts).await;
        // Snapshot first — even partial / cancelled runs may have
        // useful intermediate tool turns the user wants persisted.
        let snapshot = chat_messages_to_msgs(&messages);
        let _ = tx_outer.send(UiEvent::AgentHistory(snapshot));
        if let Err(e) = result {
            let _ = tx_outer.send(UiEvent::Error(e.to_string()));
        }
        // Done is the REPL's terminator; send it last so the
        // snapshot is processed before `wait_for_turn` exits.
        let _ = tx_outer.send(UiEvent::Done);
    });

    Ok(AgentRunHandle { cancel })
}

fn build_provider(
    state: &Arc<LlmState>,
    chat_opts: &ChatOpts,
) -> Result<(Arc<dyn Provider>, String)> {
    if chat_opts.provider == "local" {
        let label = chat_opts
            .model
            .clone()
            .unwrap_or_else(|| "local".to_string());
        let provider: Arc<dyn Provider> = Arc::new(LocalProvider::new(state.clone()));
        return Ok((provider, label));
    }
    let (api_key, base_url, model) =
        resolve_cloud_config(chat_opts).map_err(|e| anyhow!("resolve cloud config: {e}"))?;
    let provider: Arc<dyn Provider> = Arc::new(CloudProvider::new(
        api_key,
        base_url,
        chat_opts.provider.clone(),
    ));
    Ok((provider, model))
}

/// Convert the conversation's flat `ChatMsg` history into the
/// `ChatMessage` form the agent loop wants. Tool-role messages from
/// previous turns are *dropped* because we never recorded their
/// `tool_call_id`; the model treats this as a fresh conversation with
/// only text turns visible. Acceptable for a chat-style UI where tool
/// pills are ephemeral UI artifacts, not part of the model-facing
/// transcript.
fn build_agent_messages(history: Vec<ChatMsg>, user_input: String) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = history
        .into_iter()
        .filter_map(|m| match m.role.as_str() {
            "system" => Some(ChatMessage::System { content: m.content }),
            "user" => Some(ChatMessage::User { content: m.content }),
            "assistant" => Some(ChatMessage::Assistant {
                content: m.content,
                // Carry persisted `tool_calls` back into the agent
                // loop so the model sees its own prior tool
                // selections rather than a stripped text-only turn.
                tool_calls: m.tool_calls,
            }),
            "tool" => m.tool_call_id.map(|id| ChatMessage::Tool {
                tool_call_id: id,
                content: m.content,
            }),
            _ => None,
        })
        .collect();
    out.push(ChatMessage::User {
        content: user_input,
    });
    out
}

impl AgentRunHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rezon_core::agent::tool::ToolCall;

    #[test]
    fn chat_messages_to_msgs_preserves_role_content() {
        let msgs = vec![
            ChatMessage::system("you are terse"),
            ChatMessage::user("hi"),
            ChatMessage::Assistant {
                content: "hello".into(),
                tool_calls: vec![],
            },
        ];
        let out = chat_messages_to_msgs(&msgs);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].role, "system");
        assert_eq!(out[1].role, "user");
        assert_eq!(out[2].role, "assistant");
        assert_eq!(out[0].content, "you are terse");
        assert_eq!(out[1].content, "hi");
        assert_eq!(out[2].content, "hello");
        for m in &out {
            assert!(m.tool_calls.is_empty());
            assert!(m.tool_call_id.is_none());
        }
    }

    #[test]
    fn chat_messages_to_msgs_preserves_tool_calls_on_assistant() {
        let msgs = vec![ChatMessage::Assistant {
            content: "calling".into(),
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "current_time".into(),
                arguments: "{}".into(),
            }],
        }];
        let out = chat_messages_to_msgs(&msgs);
        assert_eq!(out[0].tool_calls.len(), 1);
        assert_eq!(out[0].tool_calls[0].id, "call-1");
        assert_eq!(out[0].tool_calls[0].name, "current_time");
        assert!(out[0].tool_call_id.is_none());
    }

    #[test]
    fn chat_messages_to_msgs_tool_role_carries_tool_call_id() {
        let msgs = vec![ChatMessage::Tool {
            tool_call_id: "call-1".into(),
            content: "{\"ok\":true}".into(),
        }];
        let out = chat_messages_to_msgs(&msgs);
        assert_eq!(out[0].role, "tool");
        assert_eq!(out[0].tool_call_id.as_deref(), Some("call-1"));
        assert!(out[0].tool_calls.is_empty());
        assert_eq!(out[0].content, "{\"ok\":true}");
    }

    #[test]
    fn build_agent_messages_appends_user_input() {
        let history = vec![
            ChatMsg {
                role: "system".into(),
                content: "sys".into(),
                ..ChatMsg::default()
            },
            ChatMsg {
                role: "user".into(),
                content: "earlier".into(),
                ..ChatMsg::default()
            },
            ChatMsg {
                role: "assistant".into(),
                content: "ok".into(),
                ..ChatMsg::default()
            },
        ];
        let msgs = build_agent_messages(history, "follow-up".into());
        // 3 carried-over + 1 new user turn = 4.
        assert_eq!(msgs.len(), 4);
        assert!(matches!(&msgs[0], ChatMessage::System { content } if content == "sys"));
        assert!(matches!(&msgs[3], ChatMessage::User { content } if content == "follow-up"));
    }

    #[test]
    fn build_agent_messages_replays_tool_calls_and_tool_turns() {
        // Persisted assistant turn with tool_calls + matching tool
        // result should be threaded back into ChatMessage form so
        // the next agent run sees its own prior call.
        let history = vec![
            ChatMsg {
                role: "assistant".into(),
                content: "calling".into(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "current_time".into(),
                    arguments: "{}".into(),
                }],
                tool_call_id: None,
            },
            ChatMsg {
                role: "tool".into(),
                content: "{\"hour\":12}".into(),
                tool_call_id: Some("call-1".into()),
                tool_calls: vec![],
            },
        ];
        let msgs = build_agent_messages(history, "next".into());
        assert_eq!(msgs.len(), 3);
        match &msgs[0] {
            ChatMessage::Assistant { tool_calls, .. } => {
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].id, "call-1");
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
        match &msgs[1] {
            ChatMessage::Tool { tool_call_id, .. } => {
                assert_eq!(tool_call_id, "call-1");
            }
            other => panic!("expected Tool, got {other:?}"),
        }
    }

    #[test]
    fn build_agent_messages_drops_tool_turn_without_id() {
        // Defensive: if a tool ChatMsg was stored without a
        // tool_call_id (shouldn't happen post-P7e, but might exist
        // in legacy stores), it should be skipped rather than
        // crashing the build.
        let history = vec![ChatMsg {
            role: "tool".into(),
            content: "orphaned".into(),
            tool_call_id: None,
            tool_calls: vec![],
        }];
        let msgs = build_agent_messages(history, "x".into());
        // Orphan tool turn dropped; only the new user turn remains.
        assert_eq!(msgs.len(), 1);
        assert!(matches!(&msgs[0], ChatMessage::User { .. }));
    }
}
