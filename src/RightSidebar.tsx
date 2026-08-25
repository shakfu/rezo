import { useState } from "react";
import { Combobox } from "@base-ui/react/combobox";
import { Tooltip } from "./Tooltip";
import { BaseSelect } from "./Select";
import { CloudProviderInfo, Conversation, ModelCatalog } from "./types";

type Props = {
  collapsed: boolean;
  onToggle: () => void;
  // Provider state (app-global)
  provider: string;
  setProvider: (p: string) => void;
  cloudProviders: CloudProviderInfo[];
  cloudModel: Record<string, string>;
  setCloudModel: (
    fn: (prev: Record<string, string>) => Record<string, string>,
  ) => void;
  cloudBaseUrl: Record<string, string>;
  setCloudBaseUrl: (
    fn: (prev: Record<string, string>) => Record<string, string>,
  ) => void;
  /// Live model catalog for the selected provider, when one has been
  /// resolved. `undefined` means "not looked up yet".
  modelCatalog?: ModelCatalog;
  onRefreshModels: () => void;
  /// Which providers currently resolve a key — from `apiKeySet`, so it
  /// reflects the keychain *and* the environment. Not the keys
  /// themselves; the frontend never holds one.
  keySaved: Record<string, boolean>;
  setKeySaved: (
    fn: (prev: Record<string, boolean>) => Record<string, boolean>,
  ) => void;
  /// Called when an API-key input loses focus, or when Clear is
  /// pressed (with an empty value, which deletes). Persists to the OS
  /// keychain via the parent; doing it on blur rather than onChange
  /// keeps every keystroke from hammering the keychain.
  onCloudApiKeyCommit: (providerKey: string, value: string) => void;
  // Local model state
  modelPath: string;
  setModelPath: (s: string) => void;
  loadedPath: string | null;
  loading: boolean;
  onBrowseFile: () => void;
  onLoadModel: () => void;
  // Per-conversation
  current: Conversation | null;
  onSystemPromptChange: (value: string) => void;
};

const INPUT =
  "w-full box-border rounded-md border border-border bg-transparent px-2 py-1.5 text-[13px] text-fg font-[inherit] outline-none focus-visible:ring-2 focus-visible:ring-accent";

const BTN =
  "rounded-md border border-border bg-transparent px-2.5 py-1.5 text-[13px] text-fg cursor-pointer hover:bg-bg-soft disabled:opacity-50 disabled:cursor-not-allowed";

const SIDEBAR_BTN =
  "w-7 h-7 flex items-center justify-center rounded-md border border-border bg-transparent text-fg-dim hover:bg-bg-soft hover:text-fg cursor-pointer text-sm leading-none";

function ModelCombobox({
  items,
  fetched = [],
  value,
  onChange,
  placeholder,
}: {
  /// Curated entries from `models.json`, in their catalog order —
  /// which is the point of them: cheapest/default first.
  items: string[];
  /// Everything the provider itself reported, minus the curated ones.
  /// Listed second because the provider's `/v1/models` carries no
  /// ranking and no chat-capability marker, so it cannot be presented
  /// as a considered shortlist.
  fetched?: string[];
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
}) {
  // Base UI matches against a flat item list; the groups below are a
  // presentation layer over the same values.
  const all = fetched.length ? [...items, ...fetched] : items;
  return (
    <Combobox.Root
      items={all}
      inputValue={value}
      onInputValueChange={onChange}
    >
      <div className="relative">
        <Combobox.Input className={`${INPUT} pr-8`} placeholder={placeholder} />
        <Combobox.Trigger
          className="absolute right-1 top-1/2 -translate-y-1/2 flex h-6 w-6 cursor-pointer items-center justify-center rounded text-fg-dim hover:bg-bg-soft hover:text-fg"
          aria-label="Show recommended models"
        >
          ▾
        </Combobox.Trigger>
      </div>
      <Combobox.Portal>
        <Combobox.Positioner sideOffset={4} className="z-[150]">
          <Combobox.Popup className="max-h-64 min-w-[var(--anchor-width)] overflow-y-auto rounded-md border border-border bg-bg-elev py-1 text-[13px] text-fg shadow-[0_10px_30px_rgba(0,0,0,0.3)]">
            <Combobox.Empty className="px-2.5 py-1.5 text-[12px] italic text-fg-dim">
              No matches — press Enter to use as-is.
            </Combobox.Empty>
            <Combobox.List>
              {(item: string) => (
                <Combobox.Item
                  key={item}
                  value={item}
                  className={`cursor-pointer px-2.5 py-1.5 hover:bg-bg-soft data-[highlighted]:bg-accent-soft ${
                    fetched.includes(item) ? "text-fg-dim" : ""
                  }`}
                >
                  {item}
                </Combobox.Item>
              )}
            </Combobox.List>
          </Combobox.Popup>
        </Combobox.Positioner>
      </Combobox.Portal>
    </Combobox.Root>
  );
}

/// One dim line under the model field saying where the list came from,
/// plus a refresh. Deliberately quiet: the curated list works offline
/// and the field is free text, so a failed fetch is a non-event and
/// must not read like a broken app.
function ModelCatalogNote({
  catalog,
  onRefresh,
}: {
  catalog?: ModelCatalog;
  onRefresh: () => void;
}) {
  const extra = catalog?.fetched.length ?? 0;
  let text: string;
  if (!catalog) text = "recommended models";
  else if (catalog.source === "recommendedOnly")
    text = catalog.error ? "recommended only (provider list unavailable)" : "recommended models";
  else if (catalog.source === "staleCache")
    text = `+${extra} from provider (cached, refresh failed)`;
  else text = `+${extra} from provider`;

  return (
    <div className="flex items-center gap-2 text-[11px] text-fg-dim">
      <span title={catalog?.error ?? undefined}>{text}</span>
      <button
        type="button"
        className="cursor-pointer border-none bg-transparent p-0 text-fg-dim underline"
        onClick={onRefresh}
      >
        Refresh
      </button>
    </div>
  );
}

/// Write-only API-key input.
///
/// There is no read path: the backend resolves keys itself and the
/// webview never receives one. So the field starts empty and shows
/// whether a key is *stored* rather than what it is. Typing replaces
/// it; clearing and blurring deletes it.
function ApiKeyField({
  providerKey,
  envVar,
  saved,
  onCommit,
  setKeySaved,
  optional = false,
}: {
  providerKey: string;
  envVar?: string;
  saved: boolean;
  onCommit: (key: string, value: string) => void;
  setKeySaved: (
    fn: (prev: Record<string, boolean>) => Record<string, boolean>,
  ) => void;
  optional?: boolean;
}) {
  const [draft, setDraft] = useState("");

  function commit() {
    const v = draft.trim();
    // An untouched field must not wipe a stored key — only an explicit
    // clear does, and that is what the Clear button is for.
    if (!v) return;
    onCommit(providerKey, v);
    setKeySaved((prev) => ({ ...prev, [providerKey]: true }));
    setDraft("");
  }

  return (
    <div className="flex flex-col gap-1">
      <input
        className={INPUT}
        type="password"
        autoComplete="off"
        value={draft}
        onChange={(e) => setDraft(e.currentTarget.value)}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.currentTarget.blur();
          }
        }}
        placeholder={saved ? "API key saved — type to replace" : "API key"}
      />
      <div className="flex items-center gap-2 text-[11px]">
        {saved ? (
          <>
            <span className="text-success">key configured</span>
            <button
              type="button"
              className="cursor-pointer border-none bg-transparent p-0 text-fg-dim underline"
              onClick={() => {
                onCommit(providerKey, "");
                setKeySaved((prev) => ({ ...prev, [providerKey]: false }));
                setDraft("");
              }}
            >
              Clear
            </button>
          </>
        ) : (
          <span className={optional ? "text-fg-dim" : "text-danger"}>
            {optional
              ? "optional for local servers"
              : envVar
                ? `no key — save one here or export ${envVar}`
                : "no key"}
          </span>
        )}
      </div>
    </div>
  );
}

export function RightSidebar(props: Props) {
  const {
    collapsed,
    onToggle,
    provider,
    setProvider,
    cloudProviders,
    cloudModel,
    setCloudModel,
    cloudBaseUrl,
    setCloudBaseUrl,
    modelCatalog,
    onRefreshModels,
    keySaved,
    setKeySaved,
    onCloudApiKeyCommit,
    modelPath,
    setModelPath,
    loading,
    onBrowseFile,
    onLoadModel,
    current,
    onSystemPromptChange,
  } = props;

  const activeCloud = cloudProviders.find((p) => p.key === provider);

  if (collapsed) {
    return (
      <aside className="flex w-10 flex-col items-center border-l border-border-soft bg-bg-elev py-2">
        <Tooltip
          side="left"
          label="Expand sidebar"
          className={SIDEBAR_BTN}
          onClick={onToggle}
        >
          «
        </Tooltip>
      </aside>
    );
  }

  return (
    <aside className="flex w-72 flex-col overflow-y-auto border-l border-border-soft bg-bg-elev">
      <div className="flex justify-start border-b border-border-soft px-2.5 py-2">
        <Tooltip
          side="left"
          label="Collapse sidebar"
          className={SIDEBAR_BTN}
          onClick={onToggle}
        >
          »
        </Tooltip>
      </div>

      <Section title="Provider">
        <BaseSelect
          value={provider}
          onValueChange={setProvider}
          items={[
            { value: "local", label: "Local" },
            ...cloudProviders.map((p) => ({ value: p.key, label: p.label })),
          ]}
        />
        {activeCloud && !activeCloud.userConfigurable && (
          // Named providers get a real input, not just a notice. This
          // used to say "<ENV_VAR> not set" with nothing to do about
          // it — which was a dead end for anyone running a packaged
          // build, since a GUI launched from the dock never sees a
          // shell export.
          <ApiKeyField
            providerKey={activeCloud.key}
            envVar={activeCloud.envVar}
            saved={keySaved[activeCloud.key] ?? activeCloud.apiKeySet}
            onCommit={onCloudApiKeyCommit}
            setKeySaved={setKeySaved}
          />
        )}
      </Section>

      <Section title="Model">
        {provider === "local" ? (
          <div className="flex flex-col gap-1.5">
            <input
              className={INPUT}
              value={modelPath}
              onChange={(e) => setModelPath(e.currentTarget.value)}
              placeholder="/path/to/model.gguf"
              disabled={loading}
            />
            <div className="flex gap-1.5">
              <button
                type="button"
                className={BTN}
                onClick={onBrowseFile}
                disabled={loading}
              >
                Browse...
              </button>
              <button
                className={`${BTN} flex-1`}
                onClick={onLoadModel}
                disabled={loading || !modelPath.trim()}
              >
                {loading ? "Loading..." : "Load"}
              </button>
            </div>
          </div>
        ) : activeCloud ? (
          activeCloud.userConfigurable ? (
            <div className="flex flex-col gap-1.5">
              <input
                className={INPUT}
                value={cloudModel[activeCloud.key] ?? ""}
                onChange={(e) =>
                  setCloudModel((prev) => ({
                    ...prev,
                    [activeCloud.key]: e.currentTarget.value,
                  }))
                }
                placeholder="model (e.g. llama3.2)"
              />
              <input
                className={INPUT}
                value={cloudBaseUrl[activeCloud.key] ?? ""}
                onChange={(e) =>
                  setCloudBaseUrl((prev) => ({
                    ...prev,
                    [activeCloud.key]: e.currentTarget.value,
                  }))
                }
                placeholder="base URL (e.g. http://localhost:11434/v1)"
              />
              <ApiKeyField
                providerKey={activeCloud.key}
                saved={keySaved[activeCloud.key] ?? false}
                onCommit={onCloudApiKeyCommit}
                setKeySaved={setKeySaved}
                optional
              />
            </div>
          ) : (
            // `key` forces a fresh Combobox per provider. Without it,
            // the underlying Base UI component keeps stale internal
            // filter / popup state across provider switches and the
            // dropdown needs two chevron clicks to surface the new
            // provider's model list.
            <div className="flex flex-col gap-1">
              <ModelCombobox
                key={activeCloud.key}
                items={activeCloud.recommendedModels}
                fetched={modelCatalog?.fetched ?? []}
                value={cloudModel[activeCloud.key] ?? ""}
                onChange={(v) =>
                  setCloudModel((prev) => ({ ...prev, [activeCloud.key]: v }))
                }
                placeholder={activeCloud.defaultModel}
              />
              <ModelCatalogNote
                catalog={modelCatalog}
                onRefresh={onRefreshModels}
              />
            </div>
          )
        ) : null}
      </Section>

      <section className="flex flex-1 flex-col gap-2 px-3.5 py-3">
        <h3 className="m-0 text-[11px] font-semibold uppercase tracking-wider text-fg-dim">
          System prompt
        </h3>
        {current ? (
          <textarea
            className={`${INPUT} flex-1 min-h-[140px] resize-y`}
            value={current.systemPrompt}
            onChange={(e) => onSystemPromptChange(e.currentTarget.value)}
            placeholder="Instructions for the assistant for this conversation."
          />
        ) : (
          <div className="text-[12px] italic text-fg-dim">
            No conversation selected.
          </div>
        )}
      </section>
    </aside>
  );
}

function Section({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
  return (
    <section className="flex flex-col gap-2 border-b border-border-soft px-3.5 py-3">
      <h3 className="m-0 text-[11px] font-semibold uppercase tracking-wider text-fg-dim">
        {title}
      </h3>
      {children}
    </section>
  );
}
