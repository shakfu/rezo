import { Tabs } from "@base-ui/react/tabs";
import { Dialog, DialogBackdrop, DialogPopup } from "./Dialog";
import { BaseSelect } from "./Select";
import {
  Settings,
  Theme,
  ToolInfo,
  ToolPermission,
  toolPermissionFor,
} from "./types";

type Props = {
  open: boolean;
  settings: Settings;
  onChange: (s: Settings) => void;
  onClose: () => void;
  tools: ToolInfo[];
};

const INPUT =
  "rounded-md border border-border bg-transparent px-2 py-1 text-[13px] text-fg font-[inherit] outline-none focus-visible:ring-2 focus-visible:ring-accent";

const TAB_LIST =
  "flex gap-1 border-b border-border-soft px-4 pt-2";

// Base UI's Tabs.Tab exposes `data-active` (not data-selected) when
// the tab is the active one. Bind selected styling to that attribute.
const TAB =
  "relative cursor-pointer border-none bg-transparent px-3 py-2 text-[13px] text-fg-dim hover:text-fg data-[active]:text-fg data-[active]:after:absolute data-[active]:after:inset-x-0 data-[active]:after:-bottom-px data-[active]:after:h-0.5 data-[active]:after:bg-accent";

const PANEL = "flex flex-col gap-3.5 overflow-y-auto px-4 py-3.5";

export function SettingsDrawer({
  open,
  settings,
  onChange,
  onClose,
  tools,
}: Props) {
  return (
    <Dialog.Root open={open} onOpenChange={(v) => !v && onClose()}>
      <Dialog.Portal>
        <DialogBackdrop />
        <DialogPopup className="flex w-[460px] max-w-[90vw] max-h-[80vh] flex-col">
          <div className="flex items-center justify-between border-b border-border-soft px-4 py-3">
            <Dialog.Title className="m-0 text-[1.05em] font-semibold">
              Settings
            </Dialog.Title>
            <Dialog.Close
              className="cursor-pointer border-none bg-transparent px-1 text-[22px] leading-none text-fg"
              aria-label="Close settings"
            >
              ×
            </Dialog.Close>
          </div>
          <Dialog.Description className="sr-only">
            Application settings: appearance, behavior, and per-tool
            permissions.
          </Dialog.Description>

          <Tabs.Root defaultValue="general" className="flex min-h-0 flex-col">
            <Tabs.List className={TAB_LIST}>
              <Tabs.Tab value="general" className={TAB}>
                General
              </Tabs.Tab>
              <Tabs.Tab value="tools" className={TAB}>
                Tools
              </Tabs.Tab>
            </Tabs.List>

            <Tabs.Panel value="general" className={PANEL}>
              <GeneralPanel settings={settings} onChange={onChange} />
            </Tabs.Panel>

            <Tabs.Panel value="tools" className={PANEL}>
              <ToolsPanel
                settings={settings}
                onChange={onChange}
                tools={tools}
              />
            </Tabs.Panel>
          </Tabs.Root>
        </DialogPopup>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

function GeneralPanel({
  settings,
  onChange,
}: {
  settings: Settings;
  onChange: (s: Settings) => void;
}) {
  return (
    <>
      <div className="flex items-center justify-between gap-3 text-[13px]">
        <span>Theme</span>
        <BaseSelect
          className="w-[140px]"
          value={settings.theme}
          onValueChange={(v) => onChange({ ...settings, theme: v as Theme })}
          items={[
            { value: "system", label: "System" },
            { value: "light", label: "Light" },
            { value: "dark", label: "Dark" },
          ]}
        />
      </div>
      <label className="flex items-center justify-between gap-3 text-[13px]">
        <span>Font size ({settings.fontSize}px)</span>
        <input
          type="range"
          className="w-40"
          min={12}
          max={20}
          step={1}
          value={settings.fontSize}
          onChange={(e) =>
            onChange({
              ...settings,
              fontSize: Number(e.currentTarget.value),
            })
          }
        />
      </label>
      <label className="flex flex-col gap-1.5 text-[13px]">
        <span>Default system prompt</span>
        <textarea
          rows={4}
          className={`${INPUT} resize-y`}
          value={settings.defaultSystemPrompt}
          onChange={(e) =>
            onChange({
              ...settings,
              defaultSystemPrompt: e.currentTarget.value,
            })
          }
        />
        <small className="text-[11px] text-fg-dim">
          Used when starting a new conversation. Existing conversations keep
          their own prompt.
        </small>
      </label>
      <label className="flex flex-col gap-1.5 text-[13px]">
        <span className="flex items-center justify-between gap-3">
          <span>Slide context window on overflow</span>
          <input
            type="checkbox"
            checked={settings.contextOverflow === "slide"}
            onChange={(e) =>
              onChange({
                ...settings,
                contextOverflow: e.currentTarget.checked ? "slide" : "error",
              })
            }
          />
        </span>
        <small className="text-[11px] text-fg-dim">
          When the conversation exceeds the model's context length, drop the
          oldest non-system messages to keep going. Off by default: long
          conversations error instead, so you notice.
        </small>
      </label>
      <SamplerControls
        settings={settings.sampler}
        onChange={(s) => onChange({ ...settings, sampler: s })}
      />
    </>
  );
}

function SamplerControls({
  settings,
  onChange,
}: {
  settings: import("./types").SamplerSettings;
  onChange: (s: import("./types").SamplerSettings) => void;
}) {
  return (
    <div className="flex flex-col gap-2 rounded-md border border-border-soft p-2.5">
      <div className="text-[11px] uppercase tracking-wider text-fg-dim">
        Cloud sampler (applies to cloud providers only)
      </div>
      <SamplerNumberField
        label="Temperature"
        hint="0–2. Lower is more deterministic; default (≈1) when blank."
        min={0}
        max={2}
        step={0.05}
        value={settings.temperature}
        onChange={(v) => onChange({ ...settings, temperature: v })}
      />
      <SamplerNumberField
        label="Top-p"
        hint="0–1 nucleus cutoff. Default when blank."
        min={0}
        max={1}
        step={0.05}
        value={settings.topP}
        onChange={(v) => onChange({ ...settings, topP: v })}
      />
      <SamplerNumberField
        label="Max tokens"
        hint="Upper bound on response length. Default when blank."
        min={1}
        max={1_000_000}
        step={1}
        value={settings.maxTokens}
        onChange={(v) => onChange({ ...settings, maxTokens: v })}
      />
    </div>
  );
}

function SamplerNumberField({
  label,
  hint,
  min,
  max,
  step,
  value,
  onChange,
}: {
  label: string;
  hint: string;
  min: number;
  max: number;
  step: number;
  value: number | null;
  onChange: (v: number | null) => void;
}) {
  return (
    <label className="flex flex-col gap-1 text-[12px]">
      <div className="flex items-center justify-between gap-2">
        <span>{label}</span>
        <input
          type="number"
          className="w-24 rounded-md border border-border bg-transparent px-2 py-1 text-right font-mono text-[12px] outline-none focus-visible:ring-2 focus-visible:ring-accent"
          min={min}
          max={max}
          step={step}
          value={value ?? ""}
          // Empty string → null (provider default). Anything else
          // parses as a number; falls back to null on NaN so users
          // can't accidentally save an invalid value.
          onChange={(e) => {
            const raw = e.currentTarget.value;
            if (raw === "") {
              onChange(null);
              return;
            }
            const n = Number(raw);
            onChange(Number.isFinite(n) ? n : null);
          }}
          placeholder="default"
        />
      </div>
      <small className="text-[11px] text-fg-dim">{hint}</small>
    </label>
  );
}

function ToolsPanel({
  settings,
  onChange,
  tools,
}: {
  settings: Settings;
  onChange: (s: Settings) => void;
  tools: ToolInfo[];
}) {
  const setPermission = (name: string, perm: ToolPermission) => {
    onChange({
      ...settings,
      toolPermissions: {
        ...settings.toolPermissions,
        [name]: perm,
      },
    });
  };

  return (
    <>
      <label className="flex flex-col gap-1.5 text-[13px]">
        <span className="flex items-center justify-between gap-3">
          <span>Enable tool calling</span>
          <input
            type="checkbox"
            checked={settings.toolsEnabled}
            onChange={(e) =>
              onChange({
                ...settings,
                toolsEnabled: e.currentTarget.checked,
              })
            }
          />
        </span>
        <small className="text-[11px] text-fg-dim">
          Route turns through the agent loop so the assistant can invoke
          registered tools. Works with cloud providers and with local GGUF
          models that have a tool-aware chat template (Qwen 3, Llama 3.1+,
          Mistral Nemo, etc.).
        </small>
      </label>

      <div className="flex flex-col gap-1.5">
        <div className="text-[11px] uppercase tracking-wider text-fg-dim">
          Per-tool permissions
        </div>
        {tools.length === 0 ? (
          <div className="text-[12px] italic text-fg-dim">
            No tools registered.
          </div>
        ) : (
          <div
            className={`flex flex-col rounded-md border border-border-soft ${
              !settings.toolsEnabled ? "opacity-60" : ""
            }`}
          >
            {tools.map((t, i) => {
              const current = toolPermissionFor(settings.toolPermissions, t);
              return (
                <div
                  key={t.name}
                  className={`flex items-center gap-3 px-3 py-2 ${
                    i > 0 ? "border-t border-border-soft" : ""
                  }`}
                >
                  <div className="flex min-w-0 flex-1 flex-col gap-0.5">
                    <code className="font-mono text-[12px] leading-tight">
                      {t.name}
                    </code>
                    <span className="text-[11px] leading-tight text-fg-dim">
                      {t.description}
                    </span>
                  </div>
                  <PermissionToggle
                    value={current}
                    onChange={(v) => setPermission(t.name, v)}
                    disabled={!settings.toolsEnabled}
                    // A tool that declares `requires_confirmation()`
                    // cannot be auto-approved from this map — the
                    // backend ignores "always" for it and prompts
                    // anyway. Grey the option out instead of offering
                    // a setting that does nothing.
                    unavailable={t.requiresConfirmation ? ["always"] : []}
                    unavailableTitle={
                      "This tool has side effects, so it always prompts. " +
                      "Use \u201cAlways allow\u201d in the confirmation dialog " +
                      "to grant it for this session."
                    }
                  />
                </div>
              );
            })}
          </div>
        )}
        <small className="text-[11px] text-fg-dim">
          <strong>Ask</strong> prompts before each call.{" "}
          <strong>Always</strong> dispatches without prompting.{" "}
          <strong>Disable</strong> hides the tool from the model entirely.
          <br />
          Tools with side effects (shell, file and note writes, network
          fetches) always prompt regardless of this setting. To stop being
          asked for one, use &ldquo;Always allow&rdquo; in the confirmation
          dialog; that grant lasts until rezon restarts.
        </small>
      </div>
    </>
  );
}

const PERMISSION_OPTIONS: { v: ToolPermission; label: string }[] = [
  { v: "ask", label: "Ask" },
  { v: "always", label: "Always" },
  { v: "disable", label: "Disable" },
];

/// Three-button segmented control for picking a tool permission.
/// Replaces the dropdown for compactness — one click instead of two,
/// and the active state is visible at a glance.
function PermissionToggle({
  value,
  onChange,
  disabled,
  unavailable,
  unavailableTitle,
}: {
  value: ToolPermission;
  onChange: (v: ToolPermission) => void;
  disabled?: boolean;
  /// Options the backend will not honor from this map, greyed out
  /// rather than silently ignored.
  unavailable?: ToolPermission[];
  unavailableTitle?: string;
}) {
  return (
    <div className="inline-flex shrink-0 overflow-hidden rounded-md border border-border-soft">
      {PERMISSION_OPTIONS.map((o, i) => {
        const active = value === o.v;
        const blocked = unavailable?.includes(o.v) ?? false;
        return (
          <button
            key={o.v}
            type="button"
            disabled={disabled || blocked}
            title={blocked ? unavailableTitle : undefined}
            onClick={() => onChange(o.v)}
            className={[
              "cursor-pointer border-none px-2 py-1 text-[11px]",
              i > 0 ? "border-l border-border-soft" : "",
              active
                ? "bg-accent text-white"
                : "bg-transparent text-fg-dim hover:bg-bg-soft hover:text-fg",
              disabled || blocked ? "cursor-not-allowed opacity-60" : "",
            ].join(" ")}
          >
            {o.label}
          </button>
        );
      })}
    </div>
  );
}
