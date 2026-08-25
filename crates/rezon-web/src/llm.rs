// Thin Tauri wrapper around `rezon_core::llm`. Bridges Tauri's
// `AppHandle` event emission and config-dir resolution onto core's
// `ChatSink` + `&Path` interfaces.

use std::path::PathBuf;
use std::sync::Arc;

use rezon_core::llm::{
    self, ChatMsg, ChatOpts, ChatSink, ChatStats, CloudProviderDef, ModelStatus,
};
use rezon_core::search::SearchState;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

pub use rezon_core::llm::LlmState;

/// Forwards `ChatSink` events as Tauri events with the same names the
/// frontend has always listened on.
struct TauriChatSink {
    app: AppHandle,
}

impl TauriChatSink {
    fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

impl ChatSink for TauriChatSink {
    fn on_token(&self, delta: &str) {
        let _ = self.app.emit("chat-token", delta);
    }
    fn on_stats(&self, stats: &ChatStats) {
        let _ = self.app.emit("chat-stats", stats);
    }
    fn on_done(&self, full: &str) {
        let _ = self.app.emit("chat-done", full);
    }
}

fn config_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map_err(|e| format!("app_config_dir: {e}"))
}

pub fn persist_last_model(app: &AppHandle, path: &str) {
    match config_dir(app) {
        Ok(dir) => llm::persist_last_model(&dir, path),
        Err(e) => eprintln!("persist last_model: {e}"),
    }
}

pub fn read_last_model(app: &AppHandle) -> Option<String> {
    config_dir(app).ok().and_then(|d| llm::read_last_model(&d))
}

pub async fn do_load(app: &AppHandle, path: String) -> Result<ModelStatus, String> {
    let _ = app.emit("model-loading", &path);
    let state = app.state::<Arc<LlmState>>();
    let status = state.load(path.clone()).await?;
    persist_last_model(app, &path);
    let _ = app.emit("model-loaded", &status);
    Ok(status)
}

#[tauri::command]
pub async fn load_model(app: AppHandle, path: String) -> Result<ModelStatus, String> {
    match do_load(&app, path).await {
        Ok(s) => Ok(s),
        Err(e) => {
            let _ = app.emit("model-load-error", &e);
            Err(e)
        }
    }
}

#[tauri::command]
pub fn model_status(state: State<'_, Arc<LlmState>>) -> Result<ModelStatus, String> {
    Ok(state.status())
}

#[tauri::command]
pub fn cancel_chat(state: State<'_, Arc<LlmState>>) {
    state.cancel();
}

#[tauri::command]
pub async fn chat(
    app: AppHandle,
    state: State<'_, Arc<LlmState>>,
    mut messages: Vec<ChatMsg>,
    opts: ChatOpts,
) -> Result<String, String> {
    // Expand `[[wikilink]]` markers against the active vault (if
    // any). System message + most recent user turn only; everything
    // else passes through so prompt caching stays valid across turns.
    // Resolution happens here at the send boundary so storage
    // (frontend state) keeps the raw markers.
    let vault = app.state::<Arc<SearchState>>().active_vault();
    if let Some(v) = vault.as_deref() {
        expand_send_msgs(v, &mut messages, &app);
    }
    let sink: Arc<dyn ChatSink> = Arc::new(TauriChatSink::new(app));
    llm::chat(state.inner().as_ref(), messages, opts, sink).await
}

/// Apply wikilink expansion to a chat message vec destined for the
/// LLM. Mutates the system message (if any, at index 0) and the most
/// recent user message; everything else passes through. Unresolved
/// markers are emitted as a `chat-warning` event so the frontend can
/// surface them next to the conversation.
fn expand_send_msgs(vault: &str, msgs: &mut [ChatMsg], app: &AppHandle) {
    if let Some(first) = msgs.first_mut() {
        if first.role == "system" {
            first.content = expand_field(vault, &first.content, app);
        }
    }
    for msg in msgs.iter_mut().rev() {
        if msg.role == "user" {
            msg.content = expand_field(vault, &msg.content, app);
            break;
        }
    }
}

fn expand_field(vault: &str, text: &str, app: &AppHandle) -> String {
    let r = rezon_core::wikilink::expand(vault, text);
    if !r.unresolved.is_empty() {
        let _ = app.emit(
            "chat-warning",
            format!("wikilink unresolved: {}", r.unresolved.join(", ")),
        );
    }
    r.text
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudProviderInfo {
    pub key: String,
    pub label: String,
    pub env_var: String,
    pub default_model: String,
    pub recommended_models: Vec<String>,
    pub api_key_set: bool,
    pub user_configurable: bool,
}

impl From<&CloudProviderDef> for CloudProviderInfo {
    fn from(p: &CloudProviderDef) -> Self {
        CloudProviderInfo {
            key: p.key.clone(),
            label: p.label.clone(),
            env_var: p.env_var.clone(),
            default_model: p.default_model.clone(),
            recommended_models: p.recommended_models.clone(),
            // Reflects the real resolution chain (keychain, then
            // environment), not just the env var. Reporting only the
            // latter is what made the sidebar claim "OPENAI_API_KEY not
            // set" for a user who had saved a key perfectly well.
            api_key_set: p.user_configurable
                || llm::lookup_api_key(
                    &p.key,
                    &p.env_var,
                    None,
                    &rezon_core::secrets::KeyringStore,
                )
                .is_some(),
            user_configurable: p.user_configurable,
        }
    }
}

#[tauri::command]
pub fn cloud_providers() -> Vec<CloudProviderInfo> {
    llm::cloud_providers_catalog()
        .iter()
        .map(CloudProviderInfo::from)
        .collect()
}

/// A provider's model list: curated entries plus whatever the provider
/// itself reports, with a note on where the latter came from.
///
/// Never fails. The curated list is always a usable answer, so a fetch
/// problem downgrades `source` and fills `error` rather than erroring
/// the command — the dropdown stays populated and the field is free
/// text regardless.
#[tauri::command]
pub async fn provider_models(
    provider: String,
    refresh: bool,
) -> Result<rezon_core::model_catalog::Catalog, String> {
    use rezon_core::model_catalog as mc;

    let def = llm::cloud_provider_def(&provider)
        .ok_or_else(|| format!("unknown provider: {provider}"))?;
    let recommended: Vec<String> = def.recommended_models.clone();

    // `user_configurable` providers have no catalog entry to fall back
    // on and their base URL only exists at runtime, so they are not
    // fetched here; the caller passes the URL explicitly if it wants a
    // list for one.
    if def.user_configurable || def.base_url.is_empty() {
        return Ok(mc::Catalog {
            recommended,
            fetched: Vec::new(),
            source: mc::CatalogSource::RecommendedOnly,
            fetched_at: None,
            error: None,
        });
    }

    let api_key = llm::lookup_api_key(
        &def.key,
        &def.env_var,
        None,
        &rezon_core::secrets::KeyringStore,
    )
    .map(|(k, _)| k);

    let Some(path) = mc::cache_path() else {
        // No config dir: fetch without persisting rather than refusing.
        return Ok(mc::resolve_catalog(
            &def.key,
            &def.base_url,
            api_key.as_deref(),
            &recommended,
            std::path::Path::new(""),
            refresh,
            mc::DEFAULT_TTL_SECS,
        )
        .await);
    };

    Ok(mc::resolve_catalog(
        &def.key,
        &def.base_url,
        api_key.as_deref(),
        &recommended,
        &path,
        refresh,
        mc::DEFAULT_TTL_SECS,
    )
    .await)
}
