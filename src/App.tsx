import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { Tooltip } from "@base-ui/react/tooltip";
import { ConfirmToolDialog, type PendingConfirm } from "./ConfirmToolDialog";
import { cloudApiKeyAccount, keychainSet } from "./secrets";
import "katex/dist/katex.min.css";
import "highlight.js/styles/github-dark.css";
import "./App.css";

import {
  CloudProviderInfo,
  Conversation,
  Msg,
  MsgStats,
  ModelStatus,
  Settings,
  ToolCallEntry,
  ModelCatalog,
  ToolInfo,
  toolPermissionFor,
} from "./types";
import {
  deriveTitle,
  loadCloudBaseUrls,
  loadCloudModels,
  loadConversations,
  loadCurrentId,
  loadLastProvider,
  loadSettings,
  newConversation,
  saveCloudBaseUrls,
  saveCloudModels,
  saveConversations,
  saveCurrentId,
  saveLastProvider,
  saveSettings,
} from "./storage";
import { MessageBody, CopyButton } from "./MessageBody";
import { Sidebar } from "./Sidebar";
import { SettingsDrawer } from "./SettingsDrawer";
import { RightSidebar } from "./RightSidebar";
import { NotesView } from "./notes/NotesView";
import { loadAppMode, saveAppMode, type AppMode } from "./notes/vault";


/// Trailing-debounce window for persisting conversations. Long enough
/// that a fast token stream collapses to a handful of writes, short
/// enough that an unexpected kill loses at most this much. Every path
/// that could drop a pending write (turn end, unload, unmount)
/// flushes explicitly, so this only bounds the crash case.
const SAVE_DEBOUNCE_MS = 400;

function ToolPill({ call }: { call: ToolCallEntry }) {
  const [expanded, setExpanded] = useState(false);
  const dot =
    call.status === "running" ? (
      <span className="dot-pulse inline-block">·</span>
    ) : call.status === "ok" ? (
      <span className="text-success">✓</span>
    ) : (
      <span className="text-danger">✗</span>
    );

  return (
    <div className="rounded-md border border-border-soft bg-bg/40 font-mono text-[11px]">
      <button
        type="button"
        className="flex cursor-pointer items-center gap-1.5 border-none bg-transparent px-2 py-0.5 text-fg"
        onClick={() => setExpanded((x) => !x)}
        title={call.id}
      >
        {dot}
        <span>{call.name}</span>
        <span className="opacity-50">{expanded ? "▾" : "▸"}</span>
      </button>
      {expanded && (
        <div className="border-t border-border-soft px-2 py-1.5 text-[11px]">
          {call.arguments && (
            <div className="mb-1">
              <div className="opacity-60">arguments</div>
              <pre className="m-0 whitespace-pre-wrap break-all">
                {call.arguments}
              </pre>
            </div>
          )}
          {call.status !== "running" && (
            <div>
              <div className="opacity-60">{call.error ? "error" : "result"}</div>
              <pre className="m-0 whitespace-pre-wrap break-all">
                {call.error ??
                  (typeof call.result === "string"
                    ? call.result
                    : JSON.stringify(call.result, null, 2))}
              </pre>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function modelName(path: string): string {
  const base = path.split(/[/\\]/).pop() ?? path;
  return base.replace(/\.gguf$/i, "");
}

function formatStats(s: MsgStats): string {
  const seconds = s.durationMs / 1000;
  const tps = seconds > 0 ? s.genTokens / seconds : 0;
  const parts: string[] = [];
  if (s.promptTokens != null) {
    if (s.cachedTokens != null && s.cachedTokens > 0) {
      parts.push(`${s.promptTokens} prompt (${s.cachedTokens} cached)`);
    } else {
      parts.push(`${s.promptTokens} prompt`);
    }
  }
  parts.push(`${s.genTokens} gen`);
  parts.push(`${tps.toFixed(1)} tok/s`);
  parts.push(`${seconds.toFixed(1)}s`);
  parts.push(s.provider);
  return parts.join(" · ");
}

function App() {
  // ---- Settings ----
  const [settings, setSettings] = useState<Settings>(() => loadSettings());
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [mode, setMode] = useState<AppMode>(() => loadAppMode());
  useEffect(() => {
    saveAppMode(mode);
  }, [mode]);

  // ---- Conversations ----
  const [conversations, setConversations] = useState<Conversation[]>(() =>
    loadConversations(),
  );
  const [currentId, setCurrentId] = useState<string | null>(() =>
    loadCurrentId(),
  );

  // Validate the current selection. If the persisted currentId no
  // longer matches an existing conversation, fall back to the most
  // recent one. We deliberately do NOT auto-create a conversation
  // when the list is empty: the chat input handles that on first
  // send, so a fresh launch can show an empty sidebar without a
  // throwaway "New chat".
  useEffect(() => {
    if (currentId && !conversations.some((c) => c.id === currentId)) {
      setCurrentId(conversations.length > 0 ? conversations[0].id : null);
    } else if (!currentId && conversations.length > 0) {
      setCurrentId(conversations[0].id);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Persisting on every `conversations` change would mean one
  // JSON.stringify over the *whole* conversation set plus one
  // synchronous localStorage write per streamed token, since the
  // `chat-token` / agent-token listeners append into `conversations`
  // token by token. That cost grows with history length and with
  // persisted tool results, so it degrades exactly as a session gets
  // long. Debounce instead: streaming pays one write per idle gap.
  //
  // `conversationsLiveRef` mirrors the latest value so the flush paths
  // below write current state rather than whatever was captured when
  // the timer was armed.
  const conversationsLiveRef = useRef(conversations);
  conversationsLiveRef.current = conversations;

  useEffect(() => {
    const t = window.setTimeout(
      () => saveConversations(conversationsLiveRef.current),
      SAVE_DEBOUNCE_MS,
    );
    return () => window.clearTimeout(t);
  }, [conversations]);

  // Flush on teardown so a quit inside a debounce window keeps the
  // tail. (The end-of-turn flush lives next to the `streaming` state
  // it depends on, further down.)
  useEffect(() => {
    const flush = () => saveConversations(conversationsLiveRef.current);
    window.addEventListener("beforeunload", flush);
    return () => {
      window.removeEventListener("beforeunload", flush);
      flush();
    };
  }, []);

  useEffect(() => {
    saveCurrentId(currentId);
  }, [currentId]);
  useEffect(() => {
    saveSettings(settings);
  }, [settings]);

  // Apply theme + font size to root.
  useEffect(() => {
    document.documentElement.dataset.theme = settings.theme;
    document.documentElement.style.fontSize = `${settings.fontSize}px`;
  }, [settings.theme, settings.fontSize]);

  const current = useMemo(
    () => conversations.find((c) => c.id === currentId) ?? null,
    [conversations, currentId],
  );

  function updateConversation(id: string, mut: (c: Conversation) => Conversation) {
    setConversations((prev) =>
      prev.map((c) => (c.id === id ? { ...mut(c), updatedAt: Date.now() } : c)),
    );
  }

  function newChat() {
    const c = newConversation(settings.defaultSystemPrompt);
    setConversations((prev) => [c, ...prev]);
    setCurrentId(c.id);
  }

  function deleteConversation(id: string) {
    setConversations((prev) => prev.filter((c) => c.id !== id));
    if (currentId === id) {
      const remaining = conversations.filter((c) => c.id !== id);
      setCurrentId(remaining.length > 0 ? remaining[0].id : null);
    }
  }

  // ---- Inference state (model + provider) ----
  const [input, setInput] = useState("");
  const [streaming, setStreaming] = useState(false);
  const [modelPath, setModelPath] = useState("");
  const [loadedPath, setLoadedPath] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);

  // End-of-turn flush for the debounced conversation persistence
  // above. A turn ending is the point a user is most likely to quit,
  // so don't leave its last tokens sitting in a pending timer.
  useEffect(() => {
    if (!streaming) saveConversations(conversationsLiveRef.current);
  }, [streaming]);

  const [provider, setProvider] = useState<string>(
    () => loadLastProvider() ?? "local",
  );
  const [cloudProviders, setCloudProviders] = useState<CloudProviderInfo[]>([]);

  const [cloudModel, setCloudModel] = useState<Record<string, string>>(() =>
    loadCloudModels(),
  );
  const [cloudBaseUrl, setCloudBaseUrl] = useState<Record<string, string>>(
    () => loadCloudBaseUrls(),
  );
  // Cloud API keys are persisted to the OS keychain (see
  // `secrets.ts`), not localStorage. Initial mount kicks off a
  // hydration `useEffect` below that asks the backend for each
  // provider's stored key; meanwhile the field renders empty so
  // the user can type one in.
  // Which providers have a key stored, not the keys themselves. The
  // backend resolves the actual value per request; the webview never
  // holds one. Seeded from `cloud_providers()`'s `apiKeySet`, which
  // already reflects the full chain (keychain, then environment).
  const [keySaved, setKeySaved] = useState<Record<string, boolean>>({});

  // Per-provider model catalogs. Fetched lazily — `/v1/models` is
  // authenticated on the named providers, so there is nothing to fetch
  // with at launch. The trigger is "this provider is selected and a
  // key resolves", which is also when the answer first matters.
  const [modelCatalogs, setModelCatalogs] = useState<
    Record<string, ModelCatalog>
  >({});

  async function loadModelCatalog(providerKey: string, refresh = false) {
    try {
      const c = await invoke<ModelCatalog>("provider_models", {
        provider: providerKey,
        refresh,
      });
      setModelCatalogs((prev) => ({ ...prev, [providerKey]: c }));
    } catch {
      // The curated list is already rendering; a failure here changes
      // nothing the user can see and needs no dialog.
    }
  }

  // Look up the catalog when the selected provider changes, and again
  // if a key appears for it later (saving one is the moment the fetch
  // becomes possible). Cached results make the repeat call cheap; the
  // backend only hits the network past its TTL.
  useEffect(() => {
    if (provider === "local") return;
    const p = cloudProviders.find((c) => c.key === provider);
    if (!p || p.userConfigurable) return;
    if (!(keySaved[provider] ?? p.apiKeySet)) return;
    void loadModelCatalog(provider);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [provider, cloudProviders, keySaved]);
  // Tool catalog from the backend; powers the Settings > Tools tab and
  // the toolPermissions map passed to agent_chat.
  const [tools, setTools] = useState<ToolInfo[]>([]);
  // Pending tool-confirmation prompt, if any. The agent loop is paused
  // on a backend oneshot until the user clicks Approve or Deny.
  const [pendingConfirm, setPendingConfirm] = useState<PendingConfirm | null>(null);
  // Latest unresolved-wikilink warning, displayed inline under the
  // most recent user message and auto-cleared on the next prompt.
  const [chatWarning, setChatWarning] = useState<string | null>(null);

  // Persist provider + per-provider model and base URL on change.
  useEffect(() => {
    saveLastProvider(provider);
  }, [provider]);
  useEffect(() => {
    saveCloudModels(cloudModel);
  }, [cloudModel]);
  useEffect(() => {
    saveCloudBaseUrls(cloudBaseUrl);
  }, [cloudBaseUrl]);
  const streamingRef = useRef(false);
  const streamingConvIdRef = useRef<string | null>(null);
  // True once a tool dispatched in the current turn has finished and we
  // are waiting for the next turn's first token/tool_call. The next
  // streaming event creates a fresh assistant Msg.
  //
  // Why this and not "last msg has stats": OpenAI emits the per-turn
  // usage chunk *before* the loop dispatches tools, so stats arrives
  // mid-bubble and is not a reliable turn boundary. The actual
  // boundary is "tool-end -> next content/tool-start".
  const needsNewBubbleRef = useRef(false);
  const scrollRef = useRef<HTMLDivElement>(null);

  // ---- Tauri event wiring ----
  useEffect(() => {
    let cancelled = false;
    const unlistens: (UnlistenFn | undefined)[] = [];

    (async () => {
      unlistens.push(
        await listen<string>("chat-token", (e) => {
          if (!streamingRef.current) return;
          const cid = streamingConvIdRef.current;
          if (!cid) return;
          setConversations((prev) =>
            prev.map((c) => {
              if (c.id !== cid) return c;
              const msgs = [...c.messages];
              const last = msgs[msgs.length - 1];
              if (last && last.role === "assistant") {
                msgs[msgs.length - 1] = {
                  ...last,
                  content: last.content + e.payload,
                };
              }
              return { ...c, messages: msgs };
            }),
          );
        }),
      );

      unlistens.push(
        await listen<MsgStats>("chat-stats", (e) => {
          const cid = streamingConvIdRef.current;
          if (!cid) return;
          setConversations((prev) =>
            prev.map((c) => {
              if (c.id !== cid) return c;
              const msgs = [...c.messages];
              const last = msgs[msgs.length - 1];
              if (last && last.role === "assistant") {
                msgs[msgs.length - 1] = { ...last, stats: e.payload };
              }
              return { ...c, messages: msgs };
            }),
          );
        }),
      );

      unlistens.push(
        await listen("chat-done", () => {
          streamingRef.current = false;
          setStreaming(false);
        }),
      );

      // ---- Agent loop events (phase 3+). Same conversation slot as the
      // chat path: the loop owns the streaming assistant Msg and any
      // follow-up assistant Msgs once tool dispatch has happened.
      //
      // Turn-boundary detection: a new bubble starts when a tool from
      // the current turn has dispatched and a fresh content/tool-start
      // arrives. `needsNewBubbleRef` is set on tool-end and consumed by
      // ensureStreamingMsgSlot; that way multiple tool-ends in the
      // same turn don't multiply bubbles, and an early per-turn stats
      // event doesn't split mid-turn.
      const ensureStreamingMsgSlot = (cid: string) => {
        const needsNew = needsNewBubbleRef.current;
        if (needsNew) needsNewBubbleRef.current = false;
        setConversations((prev) =>
          prev.map((c) => {
            if (c.id !== cid) return c;
            const msgs = [...c.messages];
            const last = msgs[msgs.length - 1];
            if (!last || last.role !== "assistant" || needsNew) {
              msgs.push({ role: "assistant", content: "" });
            }
            return { ...c, messages: msgs };
          }),
        );
      };

      const updateLastAssistant = (
        cid: string,
        mut: (m: Msg) => Msg,
      ) => {
        setConversations((prev) =>
          prev.map((c) => {
            if (c.id !== cid) return c;
            const msgs = [...c.messages];
            for (let i = msgs.length - 1; i >= 0; i--) {
              if (msgs[i].role === "assistant") {
                msgs[i] = mut(msgs[i]);
                break;
              }
            }
            return { ...c, messages: msgs };
          }),
        );
      };

      unlistens.push(
        await listen<string>("agent-token", (e) => {
          if (!streamingRef.current) return;
          const cid = streamingConvIdRef.current;
          if (!cid) return;
          ensureStreamingMsgSlot(cid);
          updateLastAssistant(cid, (m) => ({
            ...m,
            content: m.content + e.payload,
          }));
        }),
      );

      unlistens.push(
        await listen<string>("agent-thinking", (_e) => {
          // Per design decision #4: thinking content is hidden by
          // default; we just want the indicator to be active. The
          // streaming flag already drives the cursor in the UI.
        }),
      );

      unlistens.push(
        await listen<{ id: string; name: string }>("agent-tool-start", (e) => {
          const cid = streamingConvIdRef.current;
          if (!cid) return;
          ensureStreamingMsgSlot(cid);
          updateLastAssistant(cid, (m) => {
            const entry: ToolCallEntry = {
              id: e.payload.id,
              name: e.payload.name,
              status: "running",
            };
            return {
              ...m,
              toolCalls: [...(m.toolCalls ?? []), entry],
            };
          });
        }),
      );

      unlistens.push(
        await listen<{
          id: string;
          ok: boolean;
          result: unknown | null;
          error: string | null;
        }>("agent-tool-end", (e) => {
          const cid = streamingConvIdRef.current;
          if (!cid) return;
          updateLastAssistant(cid, (m) => ({
            ...m,
            toolCalls: (m.toolCalls ?? []).map((tc) =>
              tc.id === e.payload.id
                ? {
                    ...tc,
                    status: e.payload.ok ? "ok" : "error",
                    result: e.payload.result ?? undefined,
                    error: e.payload.error ?? undefined,
                  }
                : tc,
            ),
          }));
          // The next agent-token / agent-tool-start belongs to a new
          // turn -> open a fresh assistant bubble for it.
          needsNewBubbleRef.current = true;
        }),
      );

      unlistens.push(
        await listen<MsgStats>("agent-stats", (e) => {
          const cid = streamingConvIdRef.current;
          if (!cid) return;
          updateLastAssistant(cid, (m) => ({ ...m, stats: e.payload }));
        }),
      );

      unlistens.push(
        await listen<string>("agent-done", () => {
          streamingRef.current = false;
          setStreaming(false);
        }),
      );

      unlistens.push(
        await listen("agent-cancelled", () => {
          streamingRef.current = false;
          setStreaming(false);
        }),
      );

      unlistens.push(
        await listen<PendingConfirm>("agent-tool-confirm", (e) => {
          setPendingConfirm(e.payload);
        }),
      );

      // Wikilink expansion may surface unresolvable `[[Target]]`
      // markers. Display the latest warning inline; it clears on
      // the next user message (see send path below).
      unlistens.push(
        await listen<string>("chat-warning", (e) => {
          setChatWarning(e.payload);
        }),
      );

      unlistens.push(
        await listen<string>("agent-error", (e) => {
          streamingRef.current = false;
          setStreaming(false);
          const cid = streamingConvIdRef.current;
          if (!cid) return;
          updateLastAssistant(cid, (m) => ({
            ...m,
            content: m.content || e.payload,
            isError: true,
          }));
        }),
      );

      unlistens.push(
        await listen<string>("model-loading", (e) => {
          setModelPath(e.payload);
          setLoading(true);
          setLoadError(null);
        }),
      );
      unlistens.push(
        await listen<ModelStatus>("model-loaded", (e) => {
          setLoading(false);
          setLoadedPath(e.payload.path ?? null);
          if (e.payload.path) setModelPath(e.payload.path);
        }),
      );
      unlistens.push(
        await listen<string>("model-load-error", (e) => {
          setLoading(false);
          setLoadError(e.payload);
        }),
      );
      unlistens.push(
        await listen("open-settings", () => setSettingsOpen(true)),
      );

      if (cancelled) {
        unlistens.forEach((u) => u && u());
        return;
      }

      try {
        const status = await invoke<ModelStatus>("model_status");
        if (status.loaded && status.path) {
          setLoadedPath(status.path);
          setModelPath(status.path);
        }
      } catch {
        /* ignore */
      }
      try {
        const list = await invoke<CloudProviderInfo[]>("cloud_providers");
        setCloudProviders(list);
        setCloudModel((prev) => {
          const next = { ...prev };
          for (const p of list) {
            if (!next[p.key] && p.defaultModel) next[p.key] = p.defaultModel;
          }
          return next;
        });
        // Record which providers already resolve a key. `apiKeySet`
        // covers keychain *and* environment, so this is accurate for
        // both a saved key and an exported one.
        setKeySaved(
          Object.fromEntries(list.map((p) => [p.key, p.apiKeySet])),
        );
      } catch {
        /* ignore */
      }
      try {
        const list = await invoke<ToolInfo[]>("tools_catalog");
        setTools(list);
      } catch {
        /* ignore */
      }
    })();

    return () => {
      cancelled = true;
      unlistens.forEach((u) => u && u());
    };
  }, []);

  // Auto-scroll on new content for the current conversation.
  useEffect(() => {
    scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight });
  }, [current?.messages]);

  async function browse(kind: "file" | "directory") {
    const selected = await openDialog({
      multiple: false,
      directory: kind === "directory",
      filters:
        kind === "file"
          ? [{ name: "GGUF model", extensions: ["gguf"] }]
          : undefined,
    });
    if (typeof selected === "string") setModelPath(selected);
  }

  async function loadModel() {
    const path = modelPath.trim();
    if (!path || loading) return;
    setLoading(true);
    setLoadError(null);
    try {
      const status = await invoke<ModelStatus>("load_model", { path });
      setLoadedPath(status.path ?? null);
    } catch (err) {
      setLoadError(String(err));
    } finally {
      setLoading(false);
    }
  }

  // ---- Provider state derivations ----
  const activeCloud = cloudProviders.find((p) => p.key === provider);
  const activeCloudModel = activeCloud
    ? (cloudModel[activeCloud.key] ?? "").trim()
    : "";
  const activeBaseUrl = activeCloud
    ? (cloudBaseUrl[activeCloud.key] ?? "").trim()
    : "";
  const cloudReady = activeCloud
    ? !!activeCloudModel &&
      (!activeCloud.userConfigurable || !!activeBaseUrl)
    : false;

  async function send() {
    const text = input.trim();
    if (!text || streaming) return;
    if (provider === "local" && !loadedPath) return;
    if (activeCloud && !cloudReady) return;

    // Clear any prior wikilink warning — it was about the previous
    // turn. The backend re-emits per-turn if the new message has
    // unresolved markers.
    setChatWarning(null);

    const userMsg: Msg = { role: "user", content: text };
    const newAssistant: Msg = { role: "assistant", content: "" };

    // Resolve (or create) the conversation this send targets. Two
    // paths so the new-chat case doesn't hit `updateConversation`,
    // which would map over a state that doesn't yet contain the
    // freshly-created entry.
    let convId: string;
    let systemPrompt: string;
    let history: Msg[];
    if (current) {
      convId = current.id;
      systemPrompt = current.systemPrompt;
      history = [...current.messages, userMsg];
      const titled = current.messages.length === 0;
      updateConversation(current.id, (c) => ({
        ...c,
        title: titled ? deriveTitle(text) : c.title,
        messages: [...history, newAssistant],
      }));
    } else {
      const created = {
        ...newConversation(settings.defaultSystemPrompt),
        title: deriveTitle(text),
        messages: [userMsg, newAssistant] as Msg[],
      };
      convId = created.id;
      systemPrompt = created.systemPrompt;
      history = [userMsg];
      setConversations((prev) => [created, ...prev]);
      setCurrentId(created.id);
    }
    setInput("");
    streamingRef.current = true;
    streamingConvIdRef.current = convId;
    needsNewBubbleRef.current = false;
    setStreaming(true);

    const payload: Msg[] = [
      { role: "system", content: systemPrompt },
      ...history,
    ];

    // Sampler params apply only to cloud paths today (the local
    // llama.cpp sampler chain is hard-coded — TODO.md tracks
    // exposing it). Local provider sends are unaffected.
    const sampler = activeCloud
      ? {
          temperature: settings.sampler.temperature ?? undefined,
          topP: settings.sampler.topP ?? undefined,
          maxTokens: settings.sampler.maxTokens ?? undefined,
        }
      : {};

    // No `apiKey` field: the backend resolves it (runtime override ->
    // keychain -> environment). Putting it here would mean the webview
    // holds a plaintext key and ships it across IPC on every turn, for
    // no benefit — nothing in the UI needs the value.
    const opts = activeCloud
      ? activeCloud.userConfigurable
        ? {
            provider: activeCloud.key,
            model: activeCloudModel,
            baseUrl: activeBaseUrl,
            ...sampler,
          }
        : { provider: activeCloud.key, model: activeCloudModel, ...sampler }
      : { provider: "local" };

    // Tool calling lives on the agent loop. Both cloud and local
    // (phase 4+) are supported; the local path requires a tool-aware
    // chat template baked into the loaded GGUF (Qwen 3, Llama 3.1+,
    // Mistral Nemo, etc.) - the worker will emit an error delta if
    // not.
    const useAgent = settings.toolsEnabled;

    try {
      if (useAgent) {
        // Resolve every registered tool to its effective permission
        // ("ask" | "always" | "disable"). Backend filters disable,
        // and the confirmation gate uses the rest to decide whether
        // to prompt or auto-approve per call.
        const toolPermissions = tools.reduce<Record<string, string>>(
          (acc, t) => {
            acc[t.name] = toolPermissionFor(settings.toolPermissions, t);
            return acc;
          },
          {},
        );
        await invoke<string>("agent_chat", {
          messages: payload,
          opts: { ...opts, toolPermissions },
        });
      } else {
        await invoke<string>("chat", { messages: payload, opts });
      }
    } catch (err) {
      streamingRef.current = false;
      setStreaming(false);
      const errText = String(err);
      setConversations((prev) =>
        prev.map((c) => {
          if (c.id !== convId) return c;
          const msgs = [...c.messages];
          const last = msgs[msgs.length - 1];
          if (last && last.role === "assistant" && last.content === "") {
            msgs[msgs.length - 1] = {
              role: "assistant",
              content: errText,
              isError: true,
            };
          } else {
            msgs.push({ role: "assistant", content: errText, isError: true });
          }
          return { ...c, messages: msgs };
        }),
      );
    }
  }

  // No `!current` gate: typing into an empty sidebar auto-creates a
  // new conversation on send.
  const sendDisabled =
    streaming ||
    !input.trim() ||
    (provider === "local" && !loadedPath) ||
    (!!activeCloud && !cloudReady);

  const inputDisabled =
    streaming ||
    (provider === "local" && !loadedPath) ||
    (!!activeCloud && !cloudReady);

  const placeholder = streaming
    ? "Generating..."
    : provider === "local" && !loadedPath
      ? "Load a model first..."
      : activeCloud && !activeCloudModel
        ? `Set a ${activeCloud.label} model name...`
        : activeCloud && activeCloud.userConfigurable && !activeBaseUrl
          ? "Set a base URL..."
          : "Send a message (Enter to send, Shift+Enter for newline)";

  const headerLabel = activeCloud
    ? `${activeCloud.key}: ${activeCloudModel || "?"}`
    : loadedPath
      ? modelName(loadedPath)
      : "no model loaded";

  const modeToggle = (
    <div className="flex shrink-0 items-center gap-0.5 rounded-md border border-border-soft p-0.5 text-[11px]">
      <button
        type="button"
        className={`cursor-pointer rounded border-none px-2 py-0.5 text-inherit ${
          mode === "chat" ? "bg-accent text-white" : "bg-transparent"
        }`}
        onClick={() => setMode("chat")}
      >
        Chat
      </button>
      <button
        type="button"
        className={`cursor-pointer rounded border-none px-2 py-0.5 text-inherit ${
          mode === "notes" ? "bg-accent text-white" : "bg-transparent"
        }`}
        onClick={() => setMode("notes")}
      >
        Notes
      </button>
    </div>
  );

  if (mode === "notes") {
    return (
      <Tooltip.Provider delay={300} timeout={150}>
        <div className="flex h-screen flex-col">
          <header className="flex items-center justify-between border-b border-border-soft px-3 py-2">
            <span className="font-semibold">Rezon · notes</span>
            {modeToggle}
          </header>
          <div className="flex min-h-0 flex-1">
            <NotesView />
          </div>
          <SettingsDrawer
            open={settingsOpen}
            settings={settings}
            onChange={setSettings}
            onClose={() => setSettingsOpen(false)}
            tools={tools}
          />
        </div>
      </Tooltip.Provider>
    );
  }

  return (
    <Tooltip.Provider delay={300} timeout={150}>
      <div className="flex h-screen">
        <Sidebar
        conversations={conversations}
        currentId={currentId}
        collapsed={settings.leftSidebarCollapsed}
        onToggle={() =>
          setSettings({
            ...settings,
            leftSidebarCollapsed: !settings.leftSidebarCollapsed,
          })
        }
        onSelect={setCurrentId}
        onNew={newChat}
        onRename={(id, title) => updateConversation(id, (c) => ({ ...c, title }))}
        onDelete={deleteConversation}
      />
      <main className="flex h-screen min-w-0 flex-1 flex-col">
        <header className="flex items-baseline justify-between gap-3 border-b border-border-soft px-4 py-3 font-semibold">
          <span
            className="flex-1 overflow-hidden text-ellipsis whitespace-nowrap"
            title={current?.title}
          >
            {current?.title ?? "Rezon"}
          </span>
          <span
            className="overflow-hidden text-ellipsis whitespace-nowrap text-[11px] font-normal opacity-60"
            title={loadedPath ?? undefined}
          >
            {headerLabel}
          </span>
          {modeToggle}
        </header>

        {loadError && (
          <div
            className="flex items-start gap-2 whitespace-pre-wrap bg-danger-soft px-3 py-2 text-[12px] text-danger"
            role="alert"
          >
            <span className="flex-1">{loadError}</span>
            <button
              className="cursor-pointer border-none bg-transparent px-1 text-base leading-none text-inherit"
              onClick={() => setLoadError(null)}
              aria-label="Dismiss"
            >
              ×
            </button>
          </div>
        )}

        <div
          className="flex flex-1 flex-col gap-3.5 overflow-y-auto p-4"
          ref={scrollRef}
        >
          {current && current.messages.length === 0 && (
            <div className="mt-10 text-center opacity-60">
              {activeCloud
                ? activeCloudModel
                  ? "Send a message to begin."
                  : `Enter a ${activeCloud.label} model name to begin.`
                : loadedPath
                  ? "Send a message to begin."
                  : "Load a model to begin. Point at a .gguf file."}
            </div>
          )}
          {current?.messages.map((m, i) => {
            const isUser = m.role === "user";
            const isError = !!m.isError;
            return (
              <div key={i} className="group flex flex-col gap-1">
                <div className="flex items-center justify-between text-[11px] uppercase tracking-wider opacity-55">
                  <span>{m.role}</span>
                  {m.content && (
                    <CopyButton
                      className="opacity-0 transition-opacity group-hover:opacity-100 normal-case tracking-normal"
                      getText={() => m.content}
                      label="Copy"
                    />
                  )}
                </div>
                <div
                  className={`overflow-x-auto rounded-lg px-3 py-2.5 leading-snug ${
                    isError
                      ? "bg-danger-soft text-danger"
                      : isUser
                        ? "whitespace-pre-wrap bg-accent-soft"
                        : "bg-bg-soft"
                  }`}
                >
                  {m.content ? (
                    isError ? (
                      <pre className="m-0 whitespace-pre-wrap font-mono text-[0.9em]">
                        {m.content}
                      </pre>
                    ) : (
                      <MessageBody content={m.content} />
                    )
                  ) : streaming && i === current.messages.length - 1 ? (
                    <span className="dot-pulse inline-block">…</span>
                  ) : (
                    ""
                  )}
                  {m.toolCalls && m.toolCalls.length > 0 && (
                    <div className="mt-2 flex flex-wrap gap-1.5">
                      {m.toolCalls.map((tc) => (
                        <ToolPill key={tc.id} call={tc} />
                      ))}
                    </div>
                  )}
                </div>
                {m.stats && (
                  <div className="font-mono text-[11px] text-fg-dim">
                    {formatStats(m.stats)}
                  </div>
                )}
              </div>
            );
          })}
          {chatWarning && (
            <div
              className="mt-1 rounded-md border border-warning/40 bg-warning/10 px-3 py-2 text-[12px] text-warning"
              role="status"
            >
              <span className="font-semibold">heads up: </span>
              {chatWarning}
              <button
                type="button"
                className="float-right cursor-pointer border-none bg-transparent px-1 leading-none text-warning opacity-70 hover:opacity-100"
                onClick={() => setChatWarning(null)}
                aria-label="Dismiss warning"
              >
                ×
              </button>
            </div>
          )}
        </div>

        <form
          className="flex gap-2 border-t border-border-soft p-3"
          onSubmit={(e) => {
            e.preventDefault();
            send();
          }}
        >
          <textarea
            className="flex-1 resize-none rounded-md border border-border bg-transparent px-2.5 py-2 font-[inherit] text-fg outline-none focus-visible:ring-2 focus-visible:ring-accent disabled:opacity-50"
            value={input}
            onChange={(e) => setInput(e.currentTarget.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                send();
              }
            }}
            placeholder={placeholder}
            rows={2}
            disabled={inputDisabled}
          />
          {streaming ? (
            <button
              type="button"
              className="cursor-pointer rounded-md border-none bg-danger px-4.5 font-semibold text-white"
              onClick={() => {
                // Fire both: the chat path (and agent path) each
                // listen on their own command, and cancel is a
                // no-op when no run is active.
                invoke("cancel_chat").catch(() => {});
                invoke("cancel_agent").catch(() => {});
              }}
            >
              Stop
            </button>
          ) : (
            <button
              type="submit"
              className="cursor-pointer rounded-md border-none bg-accent px-4.5 font-semibold text-white disabled:cursor-not-allowed disabled:opacity-50"
              disabled={sendDisabled}
            >
              Send
            </button>
          )}
        </form>
      </main>
      <RightSidebar
        collapsed={settings.rightSidebarCollapsed}
        onToggle={() =>
          setSettings({
            ...settings,
            rightSidebarCollapsed: !settings.rightSidebarCollapsed,
          })
        }
        provider={provider}
        setProvider={setProvider}
        cloudProviders={cloudProviders}
        cloudModel={cloudModel}
        setCloudModel={setCloudModel}
        cloudBaseUrl={cloudBaseUrl}
        setCloudBaseUrl={setCloudBaseUrl}
        modelCatalog={modelCatalogs[provider]}
        onRefreshModels={() => void loadModelCatalog(provider, true)}
        keySaved={keySaved}
        setKeySaved={setKeySaved}
        onCloudApiKeyCommit={(key, value) => {
          // Fire-and-forget: keychain failures are non-fatal (a hostile
          // Linux policy can deny access without us being able to do
          // anything about it). Nothing is cached locally — the backend
          // re-reads the store on the next request.
          keychainSet(cloudApiKeyAccount(key), value).catch(() => {});
        }}
        modelPath={modelPath}
        setModelPath={setModelPath}
        loadedPath={loadedPath}
        loading={loading}
        onBrowseFile={() => browse("file")}
        onLoadModel={loadModel}
        current={current}
        onSystemPromptChange={(value) => {
          if (current) {
            updateConversation(current.id, (c) => ({
              ...c,
              systemPrompt: value,
            }));
          }
        }}
      />
        <SettingsDrawer
          open={settingsOpen}
          settings={settings}
          onChange={setSettings}
          onClose={() => setSettingsOpen(false)}
          tools={tools}
        />
        <ConfirmToolDialog
          pending={pendingConfirm}
          onResolve={(id, approved) => {
            setPendingConfirm(null);
            invoke("confirm_tool_call", {
              confirmationId: id,
              approved,
            }).catch(() => {});
          }}
          onAlwaysAllow={(tool) => {
            // Backend-recorded grant, not a change to the local
            // permission map: for confirmation-required tools the
            // gate only honors a grant it stored itself.
            invoke("grant_tool_always", { tool }).catch(() => {});
          }}
        />
      </div>
    </Tooltip.Provider>
  );
}

export default App;
