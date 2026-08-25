use std::num::NonZeroU32;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex as StdMutex};
use std::time::Instant;

use anyhow::{anyhow, Result};
use async_openai::{
    config::OpenAIConfig,
    types::chat::{
        ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
        ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs,
        ChatCompletionStreamOptions, CreateChatCompletionRequestArgs,
    },
    Client,
};
use futures::StreamExt;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaModel};
use llama_cpp_2::openai::OpenAIChatTemplateParams;
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::token::LlamaToken;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::agent::delta::{AgentDelta, FinishReason, StreamStats};
use crate::agent::tool::ToolCall;
use crate::secrets::{cloud_api_key_account, KeyringStore, SecretStore};

const N_CTX: u32 = 4096;
const MAX_NEW_TOKENS: i32 = 1024;
const N_GPU_LAYERS: u32 = 999;

/// Lifecycle events for a single chat turn. The shell wires these to
/// whatever transport it uses (Tauri events for rezon-web, an mpsc
/// channel the UI thread drains for rezon-tui). Replaces the previous
/// hard-coded `app.emit("chat-token", ...)` calls.
pub trait ChatSink: Send + Sync {
    fn on_token(&self, delta: &str);
    fn on_stats(&self, stats: &ChatStats);
    fn on_done(&self, full: &str);
}

/// Sink that drops every event. Useful for tests and non-streaming
/// callers.
pub struct NullChatSink;

impl ChatSink for NullChatSink {
    fn on_token(&self, _: &str) {}
    fn on_stats(&self, _: &ChatStats) {}
    fn on_done(&self, _: &str) {}
}

#[derive(Default)]
pub struct LlmState {
    backend: StdMutex<Option<Arc<LlamaBackend>>>,
    loaded: StdMutex<Option<LoadedHandle>>,
    cancel: Arc<AtomicBool>,
}

/// Lock a `LlmState` mutex, recovering from poisoning.
///
/// These mutexes guard handles — an `Arc<LlamaBackend>` and a
/// `LoadedHandle` — not a multi-field invariant that a panic could
/// leave half-updated. So poisoning here carries no information: the
/// data is exactly as valid as it was before whichever unrelated panic
/// happened to occur while the guard was held.
///
/// `.lock().unwrap()` was the previous behaviour, and its failure mode
/// is bad out of proportion to the cause: one panic anywhere in the
/// process while a guard is held makes *every* subsequent local
/// inference call panic for the rest of the session, with no way back
/// short of restarting the app. Recovering via `into_inner` keeps the
/// model usable.
fn lock_recover<T>(m: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl LlmState {
    fn ensure_backend(&self) -> Result<Arc<LlamaBackend>> {
        let mut guard = lock_recover(&self.backend);
        if let Some(b) = guard.as_ref() {
            return Ok(b.clone());
        }
        // llama.cpp + ggml dump *a lot* of progress detail to stderr
        // by default — model metadata, per-tensor allocation, metal
        // kernel compilation, etc. The TUI puts a spinner on stdout
        // during load and a clean banner afterwards, so this noise
        // just trashes the UI. `void_logs` swaps in a no-op log
        // callback globally (the upstream binding is process-wide,
        // not per-backend).
        let mut b = LlamaBackend::init().map_err(|e| anyhow!("backend init: {e}"))?;
        b.void_logs();
        let b = Arc::new(b);
        *guard = Some(b.clone());
        Ok(b)
    }

    /// Snapshot of the currently-loaded model.
    pub fn status(&self) -> ModelStatus {
        let guard = lock_recover(&self.loaded);
        match guard.as_ref() {
            Some(l) => ModelStatus {
                loaded: true,
                path: Some(l.path.clone()),
            },
            None => ModelStatus {
                loaded: false,
                path: None,
            },
        }
    }

    /// Signal any in-flight chat to abort. The next request resets
    /// the flag to false before dispatching.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Reset the cancel flag and hand out a clone the caller can use
    /// to observe future cancellations.
    pub fn arm_cancel(&self) -> Arc<AtomicBool> {
        self.cancel.store(false, Ordering::Relaxed);
        self.cancel.clone()
    }

    /// Submit a tool-aware streaming chat to the loaded local model's
    /// worker. Returns a receiver that yields `AgentDelta` values; the
    /// channel closes when generation finishes. Used by
    /// `agent::local::LocalProvider`.
    pub fn agent_chat_stream(
        &self,
        messages_json: String,
        tools_json: String,
        cancel: Arc<AtomicBool>,
    ) -> std::result::Result<UnboundedReceiver<std::result::Result<AgentDelta, String>>, String>
    {
        let sender = {
            let g = lock_recover(&self.loaded);
            g.as_ref()
                .ok_or_else(|| "no model loaded".to_string())?
                .sender
                .as_ref()
                .ok_or_else(|| "model worker exited".to_string())?
                .clone()
        };
        let (tx, rx) = unbounded_channel();
        sender
            .send(WorkerRequest::AgentChat {
                messages_json,
                tools_json,
                cancel,
                deltas: tx,
            })
            .map_err(|_| "model worker exited".to_string())?;
        Ok(rx)
    }

    /// Submit a non-tool chat to the loaded local model. Tokens stream
    /// to `sink` as they are produced.
    pub async fn local_chat(
        &self,
        messages: Vec<ChatMsg>,
        cancel: Arc<AtomicBool>,
        sink: Arc<dyn ChatSink>,
    ) -> Result<String, String> {
        let sender = {
            let guard = lock_recover(&self.loaded);
            guard
                .as_ref()
                .ok_or_else(|| "no model loaded".to_string())?
                .sender
                .as_ref()
                .ok_or_else(|| "model worker exited".to_string())?
                .clone()
        };

        let (respond_tx, respond_rx) = tokio::sync::oneshot::channel();
        sender
            .send(WorkerRequest::Chat {
                messages,
                cancel,
                sink,
                respond: respond_tx,
            })
            .map_err(|_| "model worker exited".to_string())?;

        respond_rx
            .await
            .map_err(|_| "model worker dropped response".to_string())?
    }

    /// Load a GGUF model. Replaces any previously-loaded model.
    pub async fn load(&self, path: String) -> std::result::Result<ModelStatus, String> {
        let backend = self.ensure_backend().map_err(|e| e.to_string())?;
        let path_for_load = path.clone();
        let backend_for_load = backend.clone();
        let model =
            tokio::task::spawn_blocking(move || -> std::result::Result<LlamaModel, String> {
                let params = LlamaModelParams::default().with_n_gpu_layers(N_GPU_LAYERS);
                LlamaModel::load_from_file(&backend_for_load, Path::new(&path_for_load), &params)
                    .map_err(|e| format!("load_from_file: {e}"))
            })
            .await
            .map_err(|e| e.to_string())??;

        let model = Arc::new(model);
        let (sender, join) = spawn_worker(model, backend);
        self.cancel.store(true, Ordering::Relaxed);
        {
            let mut guard = lock_recover(&self.loaded);
            *guard = Some(LoadedHandle {
                path: path.clone(),
                sender: Some(sender),
                join: Some(join),
            });
        }
        Ok(ModelStatus {
            loaded: true,
            path: Some(path),
        })
    }

    pub fn shutdown(&self) {
        // Order matters: ggml-metal's process-exit destructor (run via
        // __cxa_finalize after main returns) asserts that no resource
        // sets are alive on the metal device. So drop the worker (which
        // joins the thread and releases the LlamaContext) before
        // dropping the backend Arc.
        //
        // These also go through `lock_recover` rather than skipping on
        // a poisoned lock. Silently doing nothing here is the worst
        // available option: a panic elsewhere is exactly the situation
        // where the teardown ordering still has to happen, and skipping
        // it trades a recoverable poison for the `GGML_ASSERT` abort at
        // process exit.
        self.cancel.store(true, Ordering::Relaxed);
        *lock_recover(&self.loaded) = None;
        *lock_recover(&self.backend) = None;
    }
}

struct LoadedHandle {
    path: String,
    sender: Option<mpsc::Sender<WorkerRequest>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Drop for LoadedHandle {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

enum WorkerRequest {
    Chat {
        messages: Vec<ChatMsg>,
        cancel: Arc<AtomicBool>,
        sink: Arc<dyn ChatSink>,
        respond: tokio::sync::oneshot::Sender<std::result::Result<String, String>>,
    },
    AgentChat {
        messages_json: String,
        tools_json: String,
        cancel: Arc<AtomicBool>,
        deltas: UnboundedSender<std::result::Result<AgentDelta, String>>,
    },
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ChatMsg {
    pub role: String,
    pub content: String,
    /// Set on assistant turns produced by the agent loop. Empty for
    /// plain chat turns. Persisted so subsequent agent runs can show
    /// the model its own prior tool selections.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Set on tool-role messages — the id of the assistant
    /// `tool_calls` entry this is the result for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelStatus {
    pub loaded: bool,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatStats {
    pub provider: String,
    pub prompt_tokens: Option<u32>,
    pub cached_tokens: Option<u32>,
    pub gen_tokens: u32,
    pub duration_ms: u64,
}

/// Persist the path of the most-recently-loaded model under
/// `<config_dir>/last_model.txt`. Errors are logged, not returned.
pub fn persist_last_model(config_dir: &Path, path: &str) {
    if let Err(e) = std::fs::create_dir_all(config_dir) {
        eprintln!("persist last_model: mkdir {config_dir:?}: {e}");
        return;
    }
    let p = config_dir.join("last_model.txt");
    if let Err(e) = std::fs::write(&p, path) {
        eprintln!("persist last_model to {p:?}: {e}");
    }
}

pub fn read_last_model(config_dir: &Path) -> Option<String> {
    let p = config_dir.join("last_model.txt");
    let s = std::fs::read_to_string(p).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatOpts {
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    /// Sampler temperature. `None` lets the provider use its
    /// default (typically ~1.0). Applied only on cloud paths
    /// today; local llama.cpp uses its own hard-coded sampler
    /// chain (TODO.md tracks the user-configurable sampler for
    /// the local path).
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Nucleus sampling cutoff. Same `None`-means-provider-default
    /// semantics.
    #[serde(default)]
    pub top_p: Option<f32>,
    /// Maximum tokens the provider may emit in the response. Maps
    /// to OpenAI's `max_completion_tokens` in newer SDKs / models;
    /// async-openai 0.36 still uses the legacy `max_tokens` field.
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudProviderDef {
    pub key: String,
    pub label: String,
    pub env_var: String,
    pub base_url: String,
    pub default_model: String,
    pub recommended_models: Vec<String>,
    pub user_configurable: bool,
}

#[derive(Debug, Deserialize)]
struct ModelsConfig {
    providers: Vec<CloudProviderDef>,
}

const MODELS_JSON: &str = include_str!("../models.json");

pub fn cloud_providers_catalog() -> &'static [CloudProviderDef] {
    static CACHE: std::sync::OnceLock<Vec<CloudProviderDef>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        let cfg: ModelsConfig =
            serde_json::from_str(MODELS_JSON).expect("models.json failed to parse at startup");
        cfg.providers
    })
}

pub fn cloud_provider_def(key: &str) -> Option<&'static CloudProviderDef> {
    cloud_providers_catalog().iter().find(|p| p.key == key)
}

/// Borrowed view of the provider-selection fields that cloud
/// resolution actually needs. `ChatOpts` (plain chat) and the Tauri
/// agent path's `AgentChatOpts` both carry these four fields plus
/// their own extras; converting into this view lets both share one
/// `resolve_cloud_config` rather than keeping two copies in step by
/// hand.
#[derive(Debug, Clone, Copy)]
pub struct CloudConfigInput<'a> {
    pub provider: &'a str,
    pub model: Option<&'a str>,
    pub base_url: Option<&'a str>,
    pub api_key: Option<&'a str>,
}

impl<'a> From<&'a ChatOpts> for CloudConfigInput<'a> {
    fn from(o: &'a ChatOpts) -> Self {
        CloudConfigInput {
            provider: &o.provider,
            model: o.model.as_deref(),
            base_url: o.base_url.as_deref(),
            api_key: o.api_key.as_deref(),
        }
    }
}

/// Where a resolved API key came from. Reported so a UI can say
/// *which* source answered rather than just "configured".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// Passed explicitly for this request (TUI `--api-key`, or the
    /// `other` provider's runtime field).
    Runtime,
    /// OS credential store.
    Keychain,
    /// Process environment, which includes anything a `.env` loaded.
    Environment,
    /// No key was found and none was needed — the `other` provider
    /// pointed at a local server that wants no auth.
    None,
}

/// Look up a cloud provider's API key.
///
/// Precedence is runtime override, then keychain, then environment,
/// and the order is deliberate:
///
/// * **Runtime** first because it is the most explicit thing the user
///   can do — a `--api-key` flag should not be silently overruled by
///   something saved months ago.
/// * **Keychain** before environment because it is the only source a
///   *packaged* GUI can actually read. An app launched from Finder,
///   the dock, or a `.desktop` entry never runs a shell profile, so
///   `~/.zshrc` exports are invisible to it. Reading only the
///   environment (the previous behaviour) meant an installed build
///   could not be given a key at all.
/// * **Environment** last, so terminal launches, `make dev`, CI, and
///   `.env` files all keep working unchanged. It is the developer
///   path and stays fully supported; it is just no longer the *only*
///   path.
///
/// A keychain that is locked or unavailable yields `None` rather than
/// an error, so the environment still gets its turn.
pub fn lookup_api_key(
    provider_key: &str,
    env_var: &str,
    runtime: Option<&str>,
    store: &dyn SecretStore,
) -> Option<(String, KeySource)> {
    if let Some(k) = runtime.map(str::trim).filter(|s| !s.is_empty()) {
        return Some((k.to_string(), KeySource::Runtime));
    }
    if let Some(k) = store
        .get(&cloud_api_key_account(provider_key))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return Some((k, KeySource::Keychain));
    }
    if !env_var.is_empty() {
        if let Some(k) = std::env::var(env_var)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            return Some((k, KeySource::Environment));
        }
    }
    None
}

/// Resolve api_key + base_url + model for a non-`local` provider,
/// consulting the real OS credential store.
///
/// Takes anything convertible into `CloudConfigInput`, so callers
/// keep passing their own options struct by reference.
pub fn resolve_cloud_config<'a>(
    opts: impl Into<CloudConfigInput<'a>>,
) -> std::result::Result<(String, String, String), String> {
    resolve_cloud_config_with(opts, &KeyringStore)
}

/// `resolve_cloud_config` with an injectable secret store.
///
/// Tests use this so they neither read nor depend on the state of the
/// developer's login keychain.
pub fn resolve_cloud_config_with<'a>(
    opts: impl Into<CloudConfigInput<'a>>,
    store: &dyn SecretStore,
) -> std::result::Result<(String, String, String), String> {
    let opts: CloudConfigInput<'a> = opts.into();
    let def = cloud_provider_def(opts.provider)
        .ok_or_else(|| format!("unknown provider: {}", opts.provider))?;

    let found = lookup_api_key(&def.key, &def.env_var, opts.api_key, store);

    let (api_key, base_url) = if def.user_configurable {
        let base_url = opts
            .base_url
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "base URL is required".to_string())?
            .to_string();
        // A user-configured endpoint is usually a local server that
        // wants no auth, so an absent key is normal here.
        let api_key = found
            .map(|(k, _)| k)
            .unwrap_or_else(|| "no-key".to_string());
        (api_key, base_url)
    } else {
        let (api_key, _src) = found.ok_or_else(|| {
            format!(
                "no API key for {}: set it in Settings, or export {}",
                def.label, def.env_var
            )
        })?;
        (api_key, def.base_url.clone())
    };

    let model = opts
        .model
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            if def.default_model.is_empty() {
                None
            } else {
                Some(def.default_model.to_string())
            }
        })
        .ok_or_else(|| "model is required".to_string())?;

    Ok((api_key, base_url, model))
}

/// Provider-routing chat entrypoint. Picks local or cloud based on
/// `opts.provider`. Streaming events flow through `sink`.
pub async fn chat(
    state: &LlmState,
    messages: Vec<ChatMsg>,
    opts: ChatOpts,
    sink: Arc<dyn ChatSink>,
) -> std::result::Result<String, String> {
    let cancel = state.arm_cancel();

    if opts.provider == "local" {
        return state.local_chat(messages, cancel, sink).await;
    }
    let (api_key, base_url, model) = resolve_cloud_config(&opts)?;
    run_cloud_chat(
        messages,
        model,
        api_key,
        base_url,
        opts.provider.clone(),
        SamplerOpts {
            temperature: opts.temperature,
            top_p: opts.top_p,
            max_tokens: opts.max_tokens,
        },
        cancel,
        sink,
    )
    .await
}

/// User-tunable sampler parameters. Threaded through `run_cloud_chat`
/// so the chat path doesn't grow yet another long argument list.
/// `None` on any field defers to the provider's default.
#[derive(Debug, Clone, Default)]
pub struct SamplerOpts {
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_tokens: Option<u32>,
}

fn to_openai_messages(
    messages: Vec<ChatMsg>,
) -> std::result::Result<Vec<ChatCompletionRequestMessage>, String> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        match m.role.as_str() {
            "system" => out.push(
                ChatCompletionRequestSystemMessageArgs::default()
                    .content(m.content)
                    .build()
                    .map_err(|e| format!("system msg: {e}"))?
                    .into(),
            ),
            "user" => out.push(
                ChatCompletionRequestUserMessageArgs::default()
                    .content(m.content)
                    .build()
                    .map_err(|e| format!("user msg: {e}"))?
                    .into(),
            ),
            "assistant" => out.push(
                ChatCompletionRequestAssistantMessageArgs::default()
                    .content(m.content)
                    .build()
                    .map_err(|e| format!("assistant msg: {e}"))?
                    .into(),
            ),
            // Tool turns only make sense paired with a `tools` array
            // and matching tool_calls on the surrounding assistant
            // message — neither of which the plain chat path emits.
            // Drop them silently so a mixed chat/agent history can
            // still be sent through the chat endpoint.
            "tool" => continue,
            other => return Err(format!("unknown role: {other}")),
        }
    }
    Ok(out)
}

// 8 args is over clippy's default of 7. Collapsing into a struct
// would only push the bag-of-args one layer in and obscure the
// call site, so we silence rather than gild the lily — same
// rationale as `spawn_agent_run` in rezon-tui.
#[allow(clippy::too_many_arguments)]
async fn run_cloud_chat(
    messages: Vec<ChatMsg>,
    model: String,
    api_key: String,
    base_url: String,
    provider: String,
    sampler: SamplerOpts,
    cancel: Arc<AtomicBool>,
    sink: Arc<dyn ChatSink>,
) -> std::result::Result<String, String> {
    let started = Instant::now();
    let openai_msgs = to_openai_messages(messages)?;
    let mut builder = CreateChatCompletionRequestArgs::default();
    builder
        .model(&model)
        .messages(openai_msgs)
        .stream_options(ChatCompletionStreamOptions {
            include_usage: Some(true),
            include_obfuscation: None,
        });
    // Only set sampler fields when the caller supplied one — leaving
    // them unset on the builder forwards the provider's default
    // rather than zero-ing them. Sanity-clamp temperature and top_p
    // to [0,2] / [0,1] to defend against UI bugs sending nonsense.
    if let Some(t) = sampler.temperature {
        builder.temperature(t.clamp(0.0, 2.0));
    }
    if let Some(p) = sampler.top_p {
        builder.top_p(p.clamp(0.0, 1.0));
    }
    if let Some(n) = sampler.max_tokens {
        builder.max_tokens(n.max(1));
    }
    let request = builder.build().map_err(|e| format!("build request: {e}"))?;

    let cfg = OpenAIConfig::new()
        .with_api_key(api_key)
        .with_api_base(base_url);
    let client = Client::with_config(cfg);

    let mut stream = client
        .chat()
        .create_stream(request)
        .await
        .map_err(|e| format!("create_stream: {e}"))?;

    let mut full = String::new();
    let mut prompt_tokens: Option<u32> = None;
    let mut gen_tokens: Option<u32> = None;
    while let Some(result) = stream.next().await {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        match result {
            Ok(response) => {
                if let Some(usage) = response.usage.as_ref() {
                    prompt_tokens = Some(usage.prompt_tokens);
                    gen_tokens = Some(usage.completion_tokens);
                }
                for choice in response.choices {
                    if let Some(content) = choice.delta.content {
                        if !content.is_empty() {
                            full.push_str(&content);
                            sink.on_token(&content);
                        }
                    }
                }
            }
            Err(e) => return Err(format!("stream: {e}")),
        }
    }

    let stats = ChatStats {
        provider,
        prompt_tokens,
        cached_tokens: None,
        gen_tokens: gen_tokens.unwrap_or_else(|| (full.len() as u32).div_ceil(4)),
        duration_ms: started.elapsed().as_millis() as u64,
    };
    sink.on_stats(&stats);
    sink.on_done(&full);
    Ok(full)
}

fn spawn_worker(
    model: Arc<LlamaModel>,
    backend: Arc<LlamaBackend>,
) -> (mpsc::Sender<WorkerRequest>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<WorkerRequest>();
    let join = std::thread::spawn(move || worker_loop(model, backend, rx));
    (tx, join)
}

fn worker_loop(
    model: Arc<LlamaModel>,
    backend: Arc<LlamaBackend>,
    rx: mpsc::Receiver<WorkerRequest>,
) {
    let model_ref: &LlamaModel = &model;
    let ctx_params = LlamaContextParams::default().with_n_ctx(NonZeroU32::new(N_CTX));
    let mut ctx = match model_ref.new_context(&backend, ctx_params) {
        Ok(c) => c,
        Err(e) => {
            let err = format!("new_context: {e}");
            eprintln!("{err}");
            while let Ok(req) = rx.recv() {
                if let WorkerRequest::Chat { respond, .. } = req {
                    let _ = respond.send(Err(err.clone()));
                }
            }
            return;
        }
    };

    let mut cached: Vec<LlamaToken> = Vec::new();

    while let Ok(req) = rx.recv() {
        match req {
            WorkerRequest::Chat {
                messages,
                cancel,
                sink,
                respond,
            } => {
                let result = run_chat_with_cache(
                    sink.as_ref(),
                    model_ref,
                    &mut ctx,
                    &mut cached,
                    messages,
                    cancel,
                );
                let _ = respond.send(result);
            }
            WorkerRequest::AgentChat {
                messages_json,
                tools_json,
                cancel,
                deltas,
            } => {
                let _ = run_agent_with_cache(
                    model_ref,
                    &mut ctx,
                    &mut cached,
                    &messages_json,
                    &tools_json,
                    cancel,
                    &deltas,
                );
            }
        }
    }
}

fn run_chat_with_cache(
    sink: &dyn ChatSink,
    model: &LlamaModel,
    ctx: &mut LlamaContext<'_>,
    cached: &mut Vec<LlamaToken>,
    messages: Vec<ChatMsg>,
    cancel: Arc<AtomicBool>,
) -> std::result::Result<String, String> {
    let started = Instant::now();
    let chat_msgs: Vec<LlamaChatMessage> = messages
        .into_iter()
        // `tool` turns belong to the agent path and aren't part of
        // the local llama chat template's known roles; skip them so a
        // mixed-mode conversation history doesn't crash the template
        // renderer.
        .filter(|m| m.role != "tool")
        .map(|m| {
            LlamaChatMessage::new(m.role, m.content)
                .map_err(|e| format!("invalid chat message: {e}"))
        })
        .collect::<std::result::Result<_, _>>()?;

    let template = model
        .chat_template(None)
        .map_err(|e| format!("model has no chat_template metadata: {e}"))?;
    let prompt = model
        .apply_chat_template(&template, &chat_msgs, true)
        .map_err(|e| format!("apply_chat_template: {e}"))?;

    let new_tokens = model
        .str_to_token(&prompt, AddBos::Always)
        .map_err(|e| format!("str_to_token: {e}"))?;

    let mut common = 0usize;
    while common < cached.len() && common < new_tokens.len() && cached[common] == new_tokens[common]
    {
        common += 1;
    }

    if common < cached.len() {
        ctx.clear_kv_cache_seq(Some(0), Some(common as u32), None)
            .map_err(|e| format!("clear_kv_cache_seq: {e}"))?;
        cached.truncate(common);
    }

    let to_add = &new_tokens[common..];
    let prompt_len = new_tokens.len() as i32;
    let n_ctx_i = ctx.n_ctx() as i32;
    let max_new = MAX_NEW_TOKENS.min(n_ctx_i - prompt_len - 8).max(0);
    if max_new == 0 {
        return Err("prompt fills the context window".to_string());
    }
    if to_add.is_empty() {
        return Err("no new tokens to decode".to_string());
    }

    // `llama_decode` aborts when a single batch carries more tokens
    // than `cparams.n_batch`. For long histories the rendered prompt
    // easily exceeds that (a 19-message chat trivially crosses 2048
    // tokens), so we split `to_add` into n_batch-sized chunks and
    // decode each in turn. Only the final token of the *whole*
    // prompt needs `logits=true` — the sampler reads logits off the
    // last position of the last chunk.
    let n_batch = ctx.n_batch() as usize;
    let chunk_cap = n_batch.max(1);
    let mut batch = LlamaBatch::new(chunk_cap, 1);
    let last_idx = to_add.len() - 1;
    let mut i = 0usize;
    while i < to_add.len() {
        let end = (i + chunk_cap).min(to_add.len());
        batch.clear();
        for (j, tok) in to_add[i..end].iter().enumerate() {
            let abs_j = i + j;
            let pos = common as i32 + abs_j as i32;
            let is_last = abs_j == last_idx;
            batch
                .add(*tok, pos, &[0], is_last)
                .map_err(|e| format!("batch.add prompt: {e}"))?;
        }
        ctx.decode(&mut batch)
            .map_err(|e| format!("decode prompt: {e}"))?;
        i = end;
    }
    cached.extend_from_slice(to_add);

    let mut sampler =
        LlamaSampler::chain_simple([LlamaSampler::temp(0.7), LlamaSampler::dist(1234)]);

    let mut decoder = encoding_rs::UTF_8.new_decoder();
    let mut full = String::new();
    let mut n_cur = prompt_len;
    let mut produced = 0;

    while produced < max_new {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let token = sampler.sample(ctx, batch.n_tokens() - 1);
        sampler.accept(token);

        if model.is_eog_token(token) {
            break;
        }

        let bytes = model
            .token_to_piece_bytes(token, 64, false, None)
            .map_err(|e| format!("token_to_piece_bytes: {e}"))?;
        let mut piece = String::with_capacity(bytes.len() + 4);
        let _ = decoder.decode_to_string(&bytes, &mut piece, false);

        if !piece.is_empty() {
            full.push_str(&piece);
            sink.on_token(&piece);
        }

        batch.clear();
        batch
            .add(token, n_cur, &[0], true)
            .map_err(|e| format!("batch.add gen: {e}"))?;
        n_cur += 1;
        produced += 1;
        ctx.decode(&mut batch)
            .map_err(|e| format!("decode gen: {e}"))?;
        cached.push(token);
    }

    let stats = ChatStats {
        provider: "local".to_string(),
        prompt_tokens: Some(prompt_len as u32),
        cached_tokens: Some(common as u32),
        gen_tokens: produced as u32,
        duration_ms: started.elapsed().as_millis() as u64,
    };
    sink.on_stats(&stats);
    sink.on_done(&full);
    Ok(full)
}

/// Tool-aware streaming chat against the loaded local model.
fn run_agent_with_cache(
    model: &LlamaModel,
    ctx: &mut LlamaContext<'_>,
    cached: &mut Vec<LlamaToken>,
    messages_json: &str,
    tools_json: &str,
    cancel: Arc<AtomicBool>,
    deltas: &UnboundedSender<std::result::Result<AgentDelta, String>>,
) -> std::result::Result<(), String> {
    let started = Instant::now();

    let template = model
        .chat_template(None)
        .map_err(|e| format!("chat_template: {e}"))?;

    let params = OpenAIChatTemplateParams {
        messages_json,
        tools_json: Some(tools_json),
        tool_choice: None,
        json_schema: None,
        grammar: None,
        reasoning_format: None,
        chat_template_kwargs: None,
        add_generation_prompt: true,
        use_jinja: true,
        parallel_tool_calls: true,
        enable_thinking: false,
        add_bos: false,
        add_eos: false,
        parse_tool_calls: true,
    };
    let tmpl = model
        .apply_chat_template_oaicompat(&template, &params)
        .map_err(|e| format!("apply_chat_template_oaicompat: {e}"))?;

    if !tmpl.parse_tool_calls || tmpl.parser.is_none() {
        let msg = format!(
            "model's chat template does not declare tool support (parse_tool_calls={}, parser={})",
            tmpl.parse_tool_calls,
            tmpl.parser.is_some()
        );
        let _ = deltas.send(Err(msg.clone()));
        return Err(msg);
    }

    let prompt_tokens = model
        .str_to_token(&tmpl.prompt, AddBos::Always)
        .map_err(|e| format!("str_to_token: {e}"))?;

    let mut common = 0usize;
    while common < cached.len()
        && common < prompt_tokens.len()
        && cached[common] == prompt_tokens[common]
    {
        common += 1;
    }
    if common < cached.len() {
        ctx.clear_kv_cache_seq(Some(0), Some(common as u32), None)
            .map_err(|e| format!("clear_kv_cache_seq: {e}"))?;
        cached.truncate(common);
    }

    let to_add = &prompt_tokens[common..];
    let prompt_len = prompt_tokens.len() as i32;
    let n_ctx_i = ctx.n_ctx() as i32;
    let max_new = MAX_NEW_TOKENS.min(n_ctx_i - prompt_len - 8).max(0);
    if max_new == 0 {
        let msg = "prompt fills the context window".to_string();
        let _ = deltas.send(Err(msg.clone()));
        return Err(msg);
    }
    if to_add.is_empty() {
        let msg = "no new tokens to decode".to_string();
        let _ = deltas.send(Err(msg.clone()));
        return Err(msg);
    }

    // `llama_decode` aborts when a single batch carries more tokens
    // than `cparams.n_batch`. For long histories the rendered prompt
    // easily exceeds that (a 19-message chat trivially crosses 2048
    // tokens), so we split `to_add` into n_batch-sized chunks and
    // decode each in turn. Only the final token of the *whole*
    // prompt needs `logits=true` — the sampler reads logits off the
    // last position of the last chunk.
    let n_batch = ctx.n_batch() as usize;
    let chunk_cap = n_batch.max(1);
    let mut batch = LlamaBatch::new(chunk_cap, 1);
    let last_idx = to_add.len() - 1;
    let mut i = 0usize;
    while i < to_add.len() {
        let end = (i + chunk_cap).min(to_add.len());
        batch.clear();
        for (j, tok) in to_add[i..end].iter().enumerate() {
            let abs_j = i + j;
            let pos = common as i32 + abs_j as i32;
            let is_last = abs_j == last_idx;
            batch
                .add(*tok, pos, &[0], is_last)
                .map_err(|e| format!("batch.add prompt: {e}"))?;
        }
        ctx.decode(&mut batch)
            .map_err(|e| format!("decode prompt: {e}"))?;
        i = end;
    }
    cached.extend_from_slice(to_add);

    let mut sampler =
        LlamaSampler::chain_simple([LlamaSampler::temp(0.7), LlamaSampler::dist(1234)]);

    let mut parse_state = tmpl
        .streaming_state_oaicompat()
        .map_err(|e| format!("streaming_state_oaicompat: {e}"))?;

    let mut decoder = encoding_rs::UTF_8.new_decoder();
    let mut n_cur = prompt_len;
    let mut produced: i32 = 0;
    let mut emitted_tool_calls = false;
    let mut hit_eos = false;
    let mut cancelled = false;

    while produced < max_new {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        let token = sampler.sample(ctx, batch.n_tokens() - 1);
        sampler.accept(token);

        if model.is_eog_token(token) {
            hit_eos = true;
            break;
        }

        let bytes = model
            .token_to_piece_bytes(token, 64, false, None)
            .map_err(|e| format!("token_to_piece_bytes: {e}"))?;
        let mut piece = String::with_capacity(bytes.len() + 4);
        let _ = decoder.decode_to_string(&bytes, &mut piece, false);

        if !piece.is_empty() {
            let parsed = parse_state
                .update(&piece, true)
                .map_err(|e| format!("parse_state.update: {e}"))?;
            for json in parsed {
                for d in oai_delta_to_agent_deltas(&json, &mut emitted_tool_calls) {
                    if deltas.send(Ok(d)).is_err() {
                        return Ok(());
                    }
                }
            }
        }

        batch.clear();
        batch
            .add(token, n_cur, &[0], true)
            .map_err(|e| format!("batch.add gen: {e}"))?;
        n_cur += 1;
        produced += 1;
        ctx.decode(&mut batch)
            .map_err(|e| format!("decode gen: {e}"))?;
        cached.push(token);
    }

    if let Ok(final_parsed) = parse_state.update("", false) {
        for json in final_parsed {
            for d in oai_delta_to_agent_deltas(&json, &mut emitted_tool_calls) {
                let _ = deltas.send(Ok(d));
            }
        }
    }

    let stats = StreamStats {
        provider: "local".to_string(),
        prompt_tokens: Some(prompt_len as u32),
        cached_tokens: Some(common as u32),
        gen_tokens: produced as u32,
        duration_ms: started.elapsed().as_millis() as u64,
    };
    let _ = deltas.send(Ok(AgentDelta::Stats(stats)));

    let finish_reason = if cancelled {
        FinishReason::Cancelled
    } else if emitted_tool_calls {
        FinishReason::ToolCalls
    } else if !hit_eos && produced >= max_new {
        FinishReason::Length
    } else {
        FinishReason::Stop
    };
    let _ = deltas.send(Ok(AgentDelta::Done { finish_reason }));
    Ok(())
}

fn oai_delta_to_agent_deltas(json: &str, emitted_tool_calls: &mut bool) -> Vec<AgentDelta> {
    let v: Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();

    if let Some(content) = v.get("content").and_then(Value::as_str) {
        if !content.is_empty() {
            out.push(AgentDelta::Content(content.to_string()));
        }
    }

    if let Some(tcs) = v.get("tool_calls").and_then(Value::as_array) {
        for tc in tcs {
            *emitted_tool_calls = true;
            let index = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
            let id = tc.get("id").and_then(Value::as_str).map(str::to_string);
            let func = tc.get("function");
            let name = func
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let args = func
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .map(str::to_string);

            if let Some(id) = id {
                out.push(AgentDelta::ToolCallStart {
                    index,
                    id,
                    name: name.unwrap_or_default(),
                });
                if let Some(args) = args {
                    if !args.is_empty() {
                        out.push(AgentDelta::ToolCallArgs {
                            index,
                            fragment: args,
                        });
                    }
                }
            } else if let Some(args) = args {
                if !args.is_empty() {
                    out.push(AgentDelta::ToolCallArgs {
                        index,
                        fragment: args,
                    });
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::test_support::MapStore;
    use tempfile::TempDir;

    #[test]
    fn cloud_catalog_has_expected_providers() {
        let catalog = cloud_providers_catalog();
        let keys: Vec<&str> = catalog.iter().map(|p| p.key.as_str()).collect();
        // models.json ships with these four; the catalog parse panics
        // at startup if the file is malformed, so reaching this
        // assertion already proves the JSON shape is valid.
        for k in ["openai", "anthropic", "openrouter", "other"] {
            assert!(keys.contains(&k), "missing provider `{k}` in {keys:?}");
        }
        // `other` is user-configurable; named providers aren't.
        let other = cloud_provider_def("other").unwrap();
        assert!(other.user_configurable);
        let openai = cloud_provider_def("openai").unwrap();
        assert!(!openai.user_configurable);
        assert!(openai
            .recommended_models
            .iter()
            .any(|m| m.starts_with("gpt-")));
    }

    #[test]
    fn cloud_provider_def_unknown_returns_none() {
        assert!(cloud_provider_def("does-not-exist").is_none());
    }

    #[test]
    fn resolve_cloud_config_other_requires_base_url() {
        let opts = ChatOpts {
            provider: "other".into(),
            model: Some("foo".into()),
            ..Default::default()
        };
        let err = resolve_cloud_config_with(&opts, &MapStore::empty()).unwrap_err();
        assert!(err.contains("base URL"), "got: {err}");
    }

    #[test]
    fn resolve_cloud_config_other_happy_path() {
        let opts = ChatOpts {
            provider: "other".into(),
            model: Some("llama3".into()),
            base_url: Some("http://localhost:11434/v1".into()),
            // empty key allowed for local OpenAI-compat servers
            ..Default::default()
        };
        let (key, base, model) = resolve_cloud_config_with(&opts, &MapStore::empty()).unwrap();
        assert_eq!(key, "no-key");
        assert_eq!(base, "http://localhost:11434/v1");
        assert_eq!(model, "llama3");
    }

    #[test]
    fn resolve_cloud_config_unknown_provider_errors() {
        let opts = ChatOpts {
            provider: "wat".into(),
            ..Default::default()
        };
        let err = resolve_cloud_config_with(&opts, &MapStore::empty()).unwrap_err();
        assert!(err.contains("unknown provider"), "got: {err}");
    }

    #[test]
    fn persist_and_read_last_model_roundtrip() {
        let dir = TempDir::new().unwrap();
        // Reading an absent file returns None.
        assert!(read_last_model(dir.path()).is_none());
        // Persist then read.
        persist_last_model(dir.path(), "/models/llama3.gguf");
        assert_eq!(
            read_last_model(dir.path()).as_deref(),
            Some("/models/llama3.gguf")
        );
        // Empty file is treated as None.
        std::fs::write(dir.path().join("last_model.txt"), "   \n").unwrap();
        assert!(read_last_model(dir.path()).is_none());
    }

    #[test]
    fn to_openai_messages_drops_tool_role() {
        // The chat path's openai-mapping should skip tool turns
        // (they belong to the agent path) so a mixed history can
        // still flow through the cloud endpoint.
        let msgs = vec![
            ChatMsg {
                role: "system".into(),
                content: "you are terse".into(),
                ..Default::default()
            },
            ChatMsg {
                role: "user".into(),
                content: "hi".into(),
                ..Default::default()
            },
            ChatMsg {
                role: "assistant".into(),
                content: "hello".into(),
                ..Default::default()
            },
            ChatMsg {
                role: "tool".into(),
                content: "{}".into(),
                tool_call_id: Some("call-1".into()),
                ..Default::default()
            },
        ];
        let openai = to_openai_messages(msgs).unwrap();
        assert_eq!(openai.len(), 3, "tool turn should have been filtered");
    }

    #[test]
    fn to_openai_messages_rejects_unknown_role() {
        let msgs = vec![ChatMsg {
            role: "function".into(),
            content: "x".into(),
            ..Default::default()
        }];
        let err = to_openai_messages(msgs).unwrap_err();
        assert!(err.contains("unknown role"), "got: {err}");
    }

    #[test]
    fn chat_msg_serde_preserves_optional_fields() {
        // Backward-compatible default round-trip: existing stores
        // without `tool_calls` / `tool_call_id` should load and the
        // optional fields should round-trip when set.
        let bare = serde_json::from_str::<ChatMsg>(r#"{"role":"user","content":"hi"}"#).unwrap();
        assert_eq!(bare.role, "user");
        assert!(bare.tool_calls.is_empty());
        assert!(bare.tool_call_id.is_none());

        let full = ChatMsg {
            role: "tool".into(),
            content: "{}".into(),
            tool_call_id: Some("call-1".into()),
            tool_calls: vec![],
        };
        let json = serde_json::to_string(&full).unwrap();
        // `tool_call_id` should be serialised when present; empty
        // `tool_calls` is skipped via skip_serializing_if.
        assert!(json.contains("tool_call_id"));
        assert!(!json.contains("tool_calls"));
        let back: ChatMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tool_call_id.as_deref(), Some("call-1"));
    }
}

#[cfg(test)]
mod poison_tests {
    use super::*;

    #[test]
    fn a_poisoned_lock_is_recovered_rather_than_panicking() {
        // Previously `.lock().unwrap()`: one panic anywhere while a
        // guard was held bricked local inference for the rest of the
        // process. Poison the mutex the way a real panic would, then
        // confirm the state is still reachable.
        let m: StdMutex<Option<u32>> = StdMutex::new(Some(7));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = m.lock().unwrap();
            panic!("poison it");
        }));
        assert!(m.is_poisoned(), "test setup did not poison the mutex");

        assert_eq!(*lock_recover(&m), Some(7));
        *lock_recover(&m) = None;
        assert_eq!(*lock_recover(&m), None);
    }

    #[test]
    fn shutdown_clears_state_through_a_poisoned_lock() {
        // The shutdown path used `if let Ok(..)`, which skipped the
        // teardown entirely on poison — trading a recoverable lock for
        // the ggml-metal exit assert.
        let state = LlmState::default();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = state.loaded.lock().unwrap();
            panic!("poison it");
        }));
        assert!(state.loaded.is_poisoned());

        state.shutdown();
        assert!(!state.status().loaded);
    }
}

#[cfg(test)]
mod key_resolution_tests {
    use super::*;
    use crate::secrets::test_support::MapStore;

    /// Env-var names used only here, so these tests neither read nor
    /// clobber a real provider key that happens to be exported.
    const EV: &str = "REZON_TEST_KEY_VAR";

    fn clear_env() {
        std::env::remove_var(EV);
    }

    #[test]
    fn runtime_override_wins_over_everything() {
        std::env::set_var(EV, "from-env");
        let store = MapStore::with("api_key:openai", "from-keychain");
        let got = lookup_api_key("openai", EV, Some("from-flag"), &store);
        assert_eq!(got, Some(("from-flag".to_string(), KeySource::Runtime)));
        clear_env();
    }

    #[test]
    fn keychain_wins_over_environment() {
        // The load-bearing rule. A packaged GUI never runs a shell
        // profile, so the keychain has to be reachable ahead of the
        // environment or an installed build cannot be given a key.
        std::env::set_var(EV, "from-env");
        let store = MapStore::with("api_key:openai", "from-keychain");
        let got = lookup_api_key("openai", EV, None, &store);
        assert_eq!(
            got,
            Some(("from-keychain".to_string(), KeySource::Keychain))
        );
        clear_env();
    }

    #[test]
    fn environment_still_answers_when_nothing_is_stored() {
        // The dev path, which must keep working untouched: terminal
        // launches, `make dev`, CI, and anything a `.env` loaded.
        std::env::set_var(EV, "from-env");
        let got = lookup_api_key("openai", EV, None, &MapStore::empty());
        assert_eq!(got, Some(("from-env".to_string(), KeySource::Environment)));
        clear_env();
    }

    #[test]
    fn nothing_anywhere_yields_none() {
        clear_env();
        assert_eq!(lookup_api_key("openai", EV, None, &MapStore::empty()), None);
    }

    #[test]
    fn blank_and_whitespace_only_values_are_ignored_at_every_level() {
        // An empty env var or a keychain entry holding only spaces is
        // "not set", not a key. Treating it as one produces a 401 that
        // looks like a provider outage.
        std::env::set_var(EV, "   ");
        let store = MapStore::with("api_key:openai", "  ");
        assert_eq!(lookup_api_key("openai", EV, Some("  "), &store), None);
        clear_env();
    }

    #[test]
    fn values_are_trimmed() {
        // Keys pasted into a UI routinely carry a trailing newline.
        let store = MapStore::with("api_key:openai", "  sk-abc\n");
        let (k, _) = lookup_api_key("openai", EV, None, &store).unwrap();
        assert_eq!(k, "sk-abc");
    }

    #[test]
    fn a_provider_with_no_env_var_skips_the_environment_entirely() {
        // `other` has `envVar: ""`. Looking that up would read a
        // nonsense variable name.
        assert_eq!(lookup_api_key("other", "", None, &MapStore::empty()), None);
    }

    #[test]
    fn keys_are_namespaced_per_provider() {
        let store = MapStore::with("api_key:openai", "openai-key");
        assert!(lookup_api_key("openai", "", None, &store).is_some());
        assert!(lookup_api_key("anthropic", "", None, &store).is_none());
    }

    // ---- End to end through resolve_cloud_config --------------------

    #[test]
    fn a_named_provider_resolves_from_the_keychain_with_no_env_var_set() {
        // The bug this whole change exists to fix: before, this path
        // read only `OPENAI_API_KEY` and a packaged app could not be
        // given a key at all.
        std::env::remove_var("OPENAI_API_KEY");
        let opts = ChatOpts {
            provider: "openai".into(),
            model: Some("gpt-5.4-nano".into()),
            ..Default::default()
        };
        let store = MapStore::with("api_key:openai", "sk-from-keychain");
        let (key, base, model) = resolve_cloud_config_with(&opts, &store).unwrap();
        assert_eq!(key, "sk-from-keychain");
        assert_eq!(base, "https://api.openai.com/v1");
        assert_eq!(model, "gpt-5.4-nano");
    }

    #[test]
    fn a_named_provider_with_no_key_anywhere_names_both_remedies() {
        std::env::remove_var("OPENAI_API_KEY");
        let opts = ChatOpts {
            provider: "openai".into(),
            model: Some("gpt-5.4-nano".into()),
            ..Default::default()
        };
        let err = resolve_cloud_config_with(&opts, &MapStore::empty()).unwrap_err();
        // The old message named only the env var, which was also the
        // only thing that worked. Both paths exist now, so say so.
        assert!(err.contains("Settings"), "got: {err}");
        assert!(err.contains("OPENAI_API_KEY"), "got: {err}");
    }

    #[test]
    fn the_other_provider_still_falls_back_to_no_key() {
        // Local servers (Ollama, LM Studio) want no auth; an absent
        // key must not be an error there.
        let opts = ChatOpts {
            provider: "other".into(),
            model: Some("llama3".into()),
            base_url: Some("http://localhost:11434/v1".into()),
            ..Default::default()
        };
        let (key, base, _) = resolve_cloud_config_with(&opts, &MapStore::empty()).unwrap();
        assert_eq!(key, "no-key");
        assert_eq!(base, "http://localhost:11434/v1");
    }

    #[test]
    fn the_other_provider_uses_a_stored_key_when_there_is_one() {
        let opts = ChatOpts {
            provider: "other".into(),
            model: Some("m".into()),
            base_url: Some("https://gateway.example/v1".into()),
            ..Default::default()
        };
        let store = MapStore::with("api_key:other", "gateway-token");
        let (key, _, _) = resolve_cloud_config_with(&opts, &store).unwrap();
        assert_eq!(key, "gateway-token");
    }
}
