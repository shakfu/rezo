// Confirmation modal for agent tool calls. Lives outside App.tsx
// because the surface (preview rendering + diff colorization) is
// independent of the chat-session state machine and ConfirmToolDialog
// is mounted once at the App root.
//
// Render contract:
//   * `pending == null` renders nothing (no Dialog open).
//   * Otherwise, opens a centered modal showing the tool name and
//     either a tinted preview (when the backend supplied one via
//     `Tool::preview`) or a pretty-printed args JSON dump.
//   * Approve/Deny buttons + close-via-backdrop both resolve via
//     `onResolve(id, approved)` — the consumer is responsible for
//     dropping `pending` and invoking the `confirm_tool_call` Tauri
//     command. This module owns presentation only.

import { Dialog, DialogBackdrop, DialogPopup } from "./Dialog";

export type PendingConfirm = {
  confirmationId: string;
  name: string;
  arguments: string;
  preview?: string | null;
};

/// Render a `Tool::preview` string with diff-style line tinting.
/// `+ ` lines glow green, `- ` lines glow red, anything else stays
/// default-fg. The first line is treated as the header (rezon's
/// tools emit `<tool>  <path>` there) and gets a slightly stronger
/// weight. Matches the TUI `colorize_diff` helper so behavior is
/// symmetric across shells.
export function DiffPreview({ text }: { text: string }) {
  const lines = text.split("\n");
  const [header, ...rest] = lines;
  return (
    <pre className="m-0 max-h-60 overflow-auto rounded-md border border-border-soft bg-bg/40 p-2 font-mono text-[12px] leading-relaxed">
      {header && (
        <div className="mb-1 font-semibold text-fg">{header}</div>
      )}
      {rest.map((line, i) => {
        if (line.startsWith("+ ")) {
          return (
            <div key={i} className="bg-success/10 text-success">
              {line}
            </div>
          );
        }
        if (line.startsWith("- ")) {
          return (
            <div key={i} className="bg-danger/10 text-danger">
              {line}
            </div>
          );
        }
        return (
          <div key={i} className="text-fg-dim">
            {line}
          </div>
        );
      })}
    </pre>
  );
}

export function ConfirmToolDialog({
  pending,
  onResolve,
  onAlwaysAllow,
}: {
  pending: PendingConfirm | null;
  onResolve: (id: string, approved: boolean) => void;
  /// Approve this call *and* record a standing grant for the tool, so
  /// it stops prompting. Routed to the `grant_tool_always` command
  /// rather than the frontend permission map: for tools that declare
  /// `requires_confirmation()`, the backend only honors a grant it
  /// recorded itself. See `TauriConfirmationGate`.
  onAlwaysAllow: (name: string) => void;
}) {
  if (!pending) return null;

  // Pretty-print fallback for when the tool didn't supply a preview.
  // Bare strings (rare; the model occasionally emits a non-object)
  // pass through unchanged.
  const pretty = (() => {
    try {
      const parsed = JSON.parse(pending.arguments || "{}");
      return JSON.stringify(parsed, null, 2);
    } catch {
      return pending.arguments;
    }
  })();

  // Closing the dialog (escape, backdrop click) counts as denial so
  // the agent loop unblocks. The backend's oneshot is consumed by
  // either button or the dialog's onOpenChange.
  return (
    <Dialog.Root
      open
      onOpenChange={(v) => {
        if (!v) onResolve(pending.confirmationId, false);
      }}
    >
      <Dialog.Portal>
        <DialogBackdrop />
        <DialogPopup className="flex w-[460px] max-w-[90vw] max-h-[80vh] flex-col">
          <div className="flex items-center justify-between border-b border-border-soft px-4 py-3">
            <Dialog.Title className="m-0 text-[1.05em] font-semibold">
              Allow tool call?
            </Dialog.Title>
            <Dialog.Close
              className="cursor-pointer border-none bg-transparent px-1 text-[22px] leading-none text-fg"
              aria-label="Deny and close"
            >
              ×
            </Dialog.Close>
          </div>
          <Dialog.Description className="sr-only">
            The assistant wants to call a tool. Approve or deny.
          </Dialog.Description>
          <div className="flex flex-col gap-3 overflow-y-auto px-4 py-3.5 text-[13px]">
            <div className="flex flex-col gap-1">
              <span className="text-[11px] uppercase tracking-wider text-fg-dim">
                Tool
              </span>
              <code className="font-mono text-[13px]">{pending.name}</code>
            </div>
            {pending.preview ? (
              <div className="flex flex-col gap-1">
                <span className="text-[11px] uppercase tracking-wider text-fg-dim">
                  Preview
                </span>
                <DiffPreview text={pending.preview} />
              </div>
            ) : (
              <div className="flex flex-col gap-1">
                <span className="text-[11px] uppercase tracking-wider text-fg-dim">
                  Arguments
                </span>
                <pre className="m-0 max-h-60 overflow-auto rounded-md border border-border-soft bg-bg/40 p-2 font-mono text-[12px]">
                  {pretty}
                </pre>
              </div>
            )}
            <div className="text-[11px] text-fg-dim">
              "Always allow" stops the prompts for{" "}
              <code className="font-mono">{pending.name}</code> until rezon
              restarts. Grants are not saved to disk.
            </div>
          </div>
          <div className="flex justify-end gap-2 border-t border-border-soft px-4 py-3">
            <button
              type="button"
              className="cursor-pointer rounded-md border border-border bg-transparent px-3 py-1.5 text-[13px] text-fg hover:bg-bg-soft"
              onClick={() => onResolve(pending.confirmationId, false)}
            >
              Deny
            </button>
            <button
              type="button"
              className="cursor-pointer rounded-md border border-border bg-transparent px-3 py-1.5 text-[13px] text-fg hover:bg-bg-soft"
              onClick={() => {
                onAlwaysAllow(pending.name);
                onResolve(pending.confirmationId, true);
              }}
            >
              Always allow
            </button>
            <button
              type="button"
              className="cursor-pointer rounded-md border-none bg-accent px-3 py-1.5 text-[13px] font-semibold text-white"
              onClick={() => onResolve(pending.confirmationId, true)}
              autoFocus
            >
              Approve
            </button>
          </div>
        </DialogPopup>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
