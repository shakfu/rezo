// Tauri commands: agent_chat and cancel_agent.
//
// Phase 3 supports cloud providers only. Local-model tool calling
// arrives in phase 4 (extends the existing llm worker thread).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::oneshot;

use crate::agent::{
    cloud::CloudProvider, confirm::ConfirmationGate, local::LocalProvider, loop_::AgentOutcome,
    run_agent, tauri_gate::TauriConfirmationGate, tauri_sink::TauriEventSink, AgentOpts,
    ChatMessage, Provider, ProviderOpts, ToolRegistry,
};
use rezon_core::agent::tools::{register_core_tools, register_search_notes, register_write_note};
use rezon_core::embed::EmbedState;
use rezon_core::llm;
use rezon_core::search::SearchState;

/// Tracks the cancel flag for the in-flight agent run, if any, plus
/// the table of pending tool-confirmation oneshots. One active run at
/// a time, mirroring `LlmState`'s pattern; starting a new run cancels
/// any previous one.
#[derive(Default)]
pub struct AgentState {
    cancel: Mutex<Option<Arc<AtomicBool>>>,
    /// Map of confirmation_id -> oneshot sender. The gate inserts on
    /// prompt; `confirm_tool_call` (or shutdown) removes and resolves.
    pending_confirms: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    /// Tools the user has explicitly granted standing auto-approval,
    /// for tools that declare `requires_confirmation()`.
    ///
    /// This is deliberately *not* the frontend's `tool_permissions`
    /// map. That map is a per-request argument, so a frontend bug or a
    /// bad state restore could set `shell_exec` to "always" and the
    /// backend would have no way to tell that apart from a real user
    /// decision. A grant only lands here via `grant_tool_always`,
    /// which is its own command and its own user action, and it is
    /// process-local: it does not survive a restart, by design. See
    /// `TauriConfirmationGate::ask` for how the two combine.
    always_grants: Mutex<std::collections::HashSet<String>>,
}

impl AgentState {
    pub fn shutdown(&self) {
        if let Ok(g) = self.cancel.lock() {
            if let Some(c) = g.as_ref() {
                c.store(true, Ordering::Relaxed);
            }
        }
        // Drop any pending confirms; the receiver side will see the
        // sender closed and treat it as denial.
        if let Ok(mut g) = self.pending_confirms.lock() {
            g.clear();
        }
    }

    /// Record a standing auto-approval for `tool`. Called only from
    /// the `grant_tool_always` command.
    pub fn grant_always(&self, tool: String) {
        if let Ok(mut g) = self.always_grants.lock() {
            g.insert(tool);
        }
    }

    /// Withdraw a standing auto-approval.
    pub fn revoke_always(&self, tool: &str) {
        if let Ok(mut g) = self.always_grants.lock() {
            g.remove(tool);
        }
    }

    /// Whether the user has granted `tool` standing auto-approval.
    /// A poisoned lock reads as "no grant" — failing closed is the
    /// only safe direction for an authorization check.
    pub fn has_always_grant(&self, tool: &str) -> bool {
        self.always_grants
            .lock()
            .map(|g| g.contains(tool))
            .unwrap_or(false)
    }

    /// Tools currently holding a standing grant, sorted. Lets the
    /// settings UI show what has been granted and offer a revoke.
    pub fn always_grants(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .always_grants
            .lock()
            .map(|g| g.iter().cloned().collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    /// Allocate a oneshot for a pending confirmation. Returns the
    /// receiver; the gate awaits this. The sender is stored under
    /// `id` until `confirm_tool_call` resolves it (or the run is
    /// cancelled).
    pub fn register_pending_confirm(&self, id: String) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        if let Ok(mut g) = self.pending_confirms.lock() {
            g.insert(id, tx);
        }
        rx
    }

    /// Drop a pending confirmation entry without resolving it (e.g.
    /// the gate's await observed an error). The receiver has already
    /// been consumed; this is just cleanup of the map.
    pub fn cancel_pending_confirm(&self, id: &str) {
        if let Ok(mut g) = self.pending_confirms.lock() {
            g.remove(id);
        }
    }

    fn take_pending_confirm(&self, id: &str) -> Option<oneshot::Sender<bool>> {
        self.pending_confirms
            .lock()
            .ok()
            .and_then(|mut g| g.remove(id))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentChatOpts {
    /// Cloud provider key: "openai" | "anthropic" | "openrouter" | "other".
    pub provider: String,
    pub model: Option<String>,
    /// Required when `provider == "other"`.
    pub base_url: Option<String>,
    /// Optional override; named providers normally read their key from env.
    pub api_key: Option<String>,
    /// Hard cap on agent loop iterations. Defaults to 8.
    pub max_steps: Option<usize>,
    pub max_tokens: Option<u32>,
    /// Cloud sampler tuning. `None` defers to the provider default.
    /// Mirrors `ChatOpts::temperature` / `top_p` for the agent path.
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    /// Per-tool permissions resolved on the frontend:
    /// "ask" | "always" | "disable". Tools mapped to "disable" are
    /// filtered out of the registry. The remaining map drives the
    /// confirmation gate: "always" auto-approves, "ask" prompts the
    /// user. Missing entries default to "ask".
    #[serde(default)]
    pub tool_permissions: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub requires_confirmation: bool,
}

/// Snapshot of registered tools. Used by the Settings UI to render
/// the per-tool permission list. We build a transient registry with
/// dummy state Arcs since the catalog only inspects each tool's
/// schema fields (name/description/requires_confirmation), never
/// dispatches.
#[tauri::command]
pub fn tools_catalog(
    search: State<'_, Arc<SearchState>>,
    embed: State<'_, Arc<EmbedState>>,
) -> Vec<ToolInfo> {
    let mut reg = ToolRegistry::new();
    register_core_tools(&mut reg);
    register_search_notes(&mut reg, search.inner().clone(), embed.inner().clone());
    register_write_note(&mut reg, search.inner().clone());
    reg.tools()
        .map(|t| ToolInfo {
            name: t.name().to_string(),
            description: t.description().to_string(),
            requires_confirmation: t.requires_confirmation(),
        })
        .collect()
}

#[tauri::command]
pub async fn agent_chat(
    app: AppHandle,
    state: State<'_, AgentState>,
    messages: Vec<ChatMessage>,
    opts: AgentChatOpts,
) -> Result<String, String> {
    let (provider, model): (Arc<dyn Provider>, String) = if opts.provider == "local" {
        // The local model's GGUF path is what `model` carries today
        // for the existing chat command; for the agent path the model
        // identity comes from whatever GGUF is loaded, so we surface
        // a synthetic label for stats rather than rejecting a missing
        // model field.
        let label = opts.model.clone().unwrap_or_else(|| "local".to_string());
        let llm_state = app.state::<Arc<llm::LlmState>>().inner().clone();
        (Arc::new(LocalProvider::new(llm_state)), label)
    } else {
        let (api_key, base_url, model) = llm::resolve_cloud_config(&opts)?;
        let label = opts.provider.clone();
        (
            Arc::new(CloudProvider::new(api_key, base_url, label)),
            model,
        )
    };
    // "disable" tools are stripped from the registry so the model
    // never sees them; the rest are passed to the gate, which decides
    // per-call whether to prompt or auto-approve.
    let disabled: Vec<String> = opts
        .tool_permissions
        .iter()
        .filter(|(_, v)| v.as_str() == "disable")
        .map(|(k, _)| k.clone())
        .collect();
    let registry = {
        let mut reg = ToolRegistry::new();
        register_core_tools(&mut reg);
        let search = app.state::<Arc<SearchState>>().inner().clone();
        let embed = app.state::<Arc<EmbedState>>().inner().clone();
        register_search_notes(&mut reg, search.clone(), embed);
        register_write_note(&mut reg, search);
        Arc::new(reg.without(&disabled))
    };
    let sink = Arc::new(TauriEventSink::new(app.clone()));

    // Replace any existing cancel slot with a fresh flag for this run.
    // If a prior run was still active, signal it to abort.
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut g = state.cancel.lock().unwrap();
        if let Some(prev) = g.replace(cancel.clone()) {
            prev.store(true, Ordering::Relaxed);
        }
    }

    // Snapshot each registered tool's own confirmation requirement.
    // This is the floor the frontend's permission map cannot lower;
    // see `TauriConfirmationGate`.
    let confirm_required: HashMap<String, bool> = registry
        .tools()
        .map(|t| (t.name().to_string(), t.requires_confirmation()))
        .collect();

    let gate: Arc<dyn ConfirmationGate> = Arc::new(TauriConfirmationGate::new(
        app.clone(),
        opts.tool_permissions.clone(),
        confirm_required,
    ));

    let agent_opts = AgentOpts {
        provider_opts: ProviderOpts {
            model,
            max_tokens: opts.max_tokens,
            temperature: opts.temperature,
            top_p: opts.top_p,
            cancel: cancel.clone(),
        },
        max_steps: opts.max_steps.unwrap_or(8),
        gate,
    };

    let mut messages = messages;
    // Wikilink expansion mirrors the non-agent `chat` command:
    // system + last user message get a `<context>` block appended
    // for any resolvable `[[Target]]` references. Past turns are
    // left untouched so prompt caching survives.
    let vault = app.state::<Arc<SearchState>>().active_vault();
    if let Some(v) = vault.as_deref() {
        expand_agent_messages(v, &mut messages, &app);
    }
    let result = run_agent(provider, registry, sink, &mut messages, agent_opts).await;

    // Clear the active cancel slot only if it is still ours; concurrent
    // re-entry may have already replaced it.
    {
        let mut g = state.cancel.lock().unwrap();
        if let Some(active) = g.as_ref() {
            if Arc::ptr_eq(active, &cancel) {
                *g = None;
            }
        }
    }

    match result {
        Ok(AgentOutcome::Final(s)) => Ok(s),
        Ok(AgentOutcome::Cancelled) => Err("cancelled".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub fn cancel_agent(state: State<'_, AgentState>) {
    let g = state.cancel.lock().unwrap();
    if let Some(c) = g.as_ref() {
        c.store(true, Ordering::Relaxed);
    }
}

/// Frontend's reply to an `agent-tool-confirm` event. Resolves the
/// pending oneshot held by `AgentState`; the gate's `await` then
/// proceeds with Approved or Denied. No-op if `confirmation_id` is
/// unknown (e.g. the run was cancelled before the user replied).
#[tauri::command]
pub fn confirm_tool_call(state: State<'_, AgentState>, confirmation_id: String, approved: bool) {
    if let Some(tx) = state.take_pending_confirm(&confirmation_id) {
        let _ = tx.send(approved);
    }
}

/// Record a standing auto-approval for `tool`, so a tool that
/// declares `requires_confirmation()` stops prompting.
///
/// This exists as its own command precisely so the grant is a
/// distinct user action rather than a field in the per-request
/// options blob. `TauriConfirmationGate` will not clear the
/// confirmation floor on the strength of `tool_permissions` alone.
///
/// Grants are process-local and are not persisted: a restart returns
/// every tool to prompting. Persisting them is a bigger decision than
/// this change should make on its own (see TODO.md, "Trust toggles
/// persisted across sessions").
#[tauri::command]
pub fn grant_tool_always(state: State<'_, AgentState>, tool: String) {
    state.grant_always(tool);
}

/// Withdraw a standing auto-approval.
#[tauri::command]
pub fn revoke_tool_always(state: State<'_, AgentState>, tool: String) {
    state.revoke_always(&tool);
}

/// Tools currently holding a standing auto-approval, sorted. The
/// settings UI reads this to show what has been granted; it is the
/// authoritative answer, unlike the frontend's own permission map.
#[tauri::command]
pub fn tool_always_grants(state: State<'_, AgentState>) -> Vec<String> {
    state.always_grants()
}

/// Bridge `AgentChatOpts` onto the shared cloud-config resolver in
/// `rezon_core::llm`. The resolution rules (env-var lookup for named
/// providers, required base URL for `other`, default-model fallback)
/// live there so the agent path and the plain-chat path cannot drift.
impl<'a> From<&'a AgentChatOpts> for llm::CloudConfigInput<'a> {
    fn from(o: &'a AgentChatOpts) -> Self {
        llm::CloudConfigInput {
            provider: &o.provider,
            model: o.model.as_deref(),
            base_url: o.base_url.as_deref(),
            api_key: o.api_key.as_deref(),
        }
    }
}

/// Apply `wikilink::expand` to the system message and the most-recent
/// user turn. Tool / assistant turns and older user turns pass
/// through untouched so the LLM provider's prompt cache stays valid.
/// Unresolved markers surface via a `chat-warning` event.
fn expand_agent_messages(vault: &str, msgs: &mut [ChatMessage], app: &AppHandle) {
    if let Some(ChatMessage::System { content }) = msgs.first_mut() {
        *content = apply_expand(vault, content, app);
    }
    for msg in msgs.iter_mut().rev() {
        if let ChatMessage::User { content } = msg {
            *content = apply_expand(vault, content, app);
            break;
        }
    }
}

fn apply_expand(vault: &str, text: &str, app: &AppHandle) -> String {
    let r = rezon_core::wikilink::expand(vault, text);
    if !r.unresolved.is_empty() {
        let _ = app.emit(
            "chat-warning",
            format!("wikilink unresolved: {}", r.unresolved.join(", ")),
        );
    }
    r.text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(provider: &str, model: Option<&str>, base_url: Option<&str>) -> AgentChatOpts {
        AgentChatOpts {
            provider: provider.to_string(),
            model: model.map(str::to_string),
            base_url: base_url.map(str::to_string),
            api_key: None,
            max_steps: None,
            max_tokens: None,
            temperature: None,
            top_p: None,
            tool_permissions: HashMap::new(),
        }
    }

    // The agent path and the plain-chat path share one resolver
    // (`llm::resolve_cloud_config`) via the `From` impl above. These
    // tests exist to catch a re-divergence: if someone reintroduces a
    // local copy, the `From` impl goes unused and these stop covering
    // the code that actually runs.
    #[test]
    fn agent_opts_resolve_other_provider() {
        let o = opts("other", Some("my-model"), Some("http://localhost:11434/v1"));
        let (key, base, model) = llm::resolve_cloud_config(&o).unwrap();
        // `other` with no key falls back to the placeholder rather
        // than erroring — local servers usually want no auth.
        assert_eq!(key, "no-key");
        assert_eq!(base, "http://localhost:11434/v1");
        assert_eq!(model, "my-model");
    }

    #[test]
    fn agent_opts_other_requires_base_url() {
        let o = opts("other", Some("m"), None);
        let err = llm::resolve_cloud_config(&o).unwrap_err();
        assert!(err.contains("base URL"), "unexpected error: {err}");
    }

    #[test]
    fn agent_opts_unknown_provider_errors() {
        let o = opts("nope", Some("m"), None);
        let err = llm::resolve_cloud_config(&o).unwrap_err();
        assert!(err.contains("unknown provider"), "unexpected error: {err}");
    }
}
