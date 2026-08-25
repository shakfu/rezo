// Confirmation gate: gates each tool dispatch on user approval.
//
// The agent loop calls `gate.ask(call)` before dispatching every tool.
// The gate decides whether to prompt the user, auto-approve, or
// auto-deny.
//
// The *decision* lives here, in `decide`, as a pure function; the gate
// implementations own only the I/O of asking. That split is deliberate:
// the decision is a security control, and it was previously written out
// twice — once in `TauriConfirmationGate` and once, differently and
// wrongly, in `TuiConfirmationGate` (which prompted for every tool,
// including ones that declare themselves read-only). One table, tested
// exhaustively, is harder to get wrong than two hand-maintained
// branches, and it needs neither an `AppHandle` nor a channel to test.
//
// Implementations:
//   - `AutoApproveGate`: always Approved. Examples and tests.
//   - `TauriConfirmationGate` (`tauri_gate.rs`): emits an event to the
//     frontend and awaits a oneshot resolved by `confirm_tool_call`.
//   - `TuiConfirmationGate` (rezon-tui `sink.rs`): sends a `Confirm`
//     event to the REPL and awaits a y/n answer.

use async_trait::async_trait;

use crate::agent::tool::ToolCall;

/// What the frontend/user configuration says about a tool, before the
/// tool's own requirement is taken into account.
///
/// This is advisory input, not a verdict: see `decide`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPermission {
    /// Prompt before every call. The default for anything unrecognized.
    Ask,
    /// The user asked not to be prompted. Honored only for tools that
    /// do not declare `requires_confirmation()`; see `decide`.
    Always,
    /// Never dispatch. Always honored — it only removes capability.
    Disable,
}

impl ToolPermission {
    /// Parse the wire form used by the frontend's permission map.
    /// Unknown strings fall back to `Ask`, which is the safe direction:
    /// a typo or a schema change should cost a prompt, never a silent
    /// auto-approval.
    pub fn parse(s: &str) -> Self {
        match s {
            "always" => ToolPermission::Always,
            "disable" => ToolPermission::Disable,
            _ => ToolPermission::Ask,
        }
    }
}

/// The gate's verdict for one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    /// Dispatch without asking.
    Approve,
    /// Refuse without asking.
    Deny,
    /// Ask the user.
    Prompt,
}

/// Decide what to do with one tool call.
///
/// The rule that matters: a tool declaring `requires_confirmation()`
/// cannot be auto-approved by `permission` alone. `permission` is
/// supplied by the frontend as a per-request argument, so a UI bug, a
/// bad state restore, or a compromised webview could set `shell_exec`
/// to `Always` and the backend would have no way to tell that apart
/// from a real user decision. Only `has_backend_grant` — recorded by
/// its own explicit command — clears that floor.
///
/// `permission` can still *raise* the bar in both directions: `Disable`
/// is always honored, and `Ask` on a read-only tool still prompts.
///
/// `requires_confirmation` should default to `true` for a tool the
/// caller does not recognize. An unrecognized name is not one to wave
/// through.
pub fn decide(
    permission: ToolPermission,
    requires_confirmation: bool,
    has_backend_grant: bool,
) -> GateDecision {
    // Checked first: it only ever removes capability, so it is the one
    // frontend verdict safe to take at face value.
    if permission == ToolPermission::Disable {
        return GateDecision::Deny;
    }
    if requires_confirmation {
        // The floor. `permission` is deliberately not consulted here.
        return if has_backend_grant {
            GateDecision::Approve
        } else {
            GateDecision::Prompt
        };
    }
    // Tool declared itself side-effect-free, so the frontend map is
    // authoritative: the worst it can do is skip a prompt for a
    // read-only operation.
    match permission {
        ToolPermission::Always => GateDecision::Approve,
        _ => GateDecision::Prompt,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmationOutcome {
    Approved,
    Denied,
}

#[async_trait]
pub trait ConfirmationGate: Send + Sync {
    /// Decide whether to dispatch `call`. `preview` is an optional
    /// human-readable rendering of what the tool will do (see
    /// `Tool::preview`); confirmation UIs should display it in
    /// place of the raw arguments JSON when present. The agent
    /// loop computes the preview once per call before invoking the
    /// gate so each implementation gets it for free.
    async fn ask(&self, call: &ToolCall, preview: Option<&str>) -> ConfirmationOutcome;
}

/// Always approves. Suitable for examples, tests, and anywhere the
/// user-confirmation UX does not exist.
pub struct AutoApproveGate;

#[async_trait]
impl ConfirmationGate for AutoApproveGate {
    async fn ask(&self, _call: &ToolCall, _preview: Option<&str>) -> ConfirmationOutcome {
        ConfirmationOutcome::Approved
    }
}

/// Generates a stable-enough confirmation_id within a single rezon
/// session: timestamp millis + a process-local counter.
pub fn next_confirmation_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("conf-{now}-{c}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use GateDecision::*;
    use ToolPermission::*;

    /// The complete decision table. Enumerated rather than sampled:
    /// this is a security control with only 12 inputs, so there is no
    /// reason to leave any of them unpinned.
    ///
    /// Rows are (permission, requires_confirmation, has_grant, expected).
    #[test]
    fn decision_table_is_exhaustive() {
        let table = [
            // --- Read-only tools: the frontend map is authoritative. ---
            (Ask, false, false, Prompt),
            (Ask, false, true, Prompt),
            (Always, false, false, Approve),
            (Always, false, true, Approve),
            (Disable, false, false, Deny),
            (Disable, false, true, Deny),
            // --- Side-effecting tools: only a backend grant clears the
            // floor. Note rows 3 and 4: `Always` from the frontend does
            // NOT approve without a grant. That is the whole point.
            (Ask, true, false, Prompt),
            (Ask, true, true, Approve),
            (Always, true, false, Prompt),
            (Always, true, true, Approve),
            (Disable, true, false, Deny),
            (Disable, true, true, Deny),
        ];
        for (perm, requires, grant, expected) in table {
            let got = decide(perm, requires, grant);
            assert_eq!(
                got, expected,
                "decide({perm:?}, requires={requires}, grant={grant}) = {got:?}, want {expected:?}"
            );
        }
    }

    #[test]
    fn frontend_always_cannot_auto_approve_a_side_effecting_tool() {
        // Regression guard for the exact bug this floor exists to stop:
        // anything that can call `agent_chat` used to be able to
        // auto-approve `shell_exec` by putting one string in a map.
        assert_eq!(decide(Always, true, false), Prompt);
    }

    #[test]
    fn disable_wins_over_a_backend_grant() {
        // Revoking capability must not be overridable by a stale grant.
        assert_eq!(decide(Disable, true, true), Deny);
        assert_eq!(decide(Disable, false, true), Deny);
    }

    #[test]
    fn a_backend_grant_does_not_leak_into_read_only_tools() {
        // A grant is per-tool; it must not turn an `Ask` on some other
        // tool into an approval.
        assert_eq!(decide(Ask, false, true), Prompt);
    }

    #[test]
    fn unknown_permission_strings_fall_back_to_ask() {
        for s in ["", "ALWAYS", "yes", "true", "allow", "always "] {
            assert_eq!(
                ToolPermission::parse(s),
                Ask,
                "{s:?} must not parse as anything permissive"
            );
        }
        assert_eq!(ToolPermission::parse("always"), Always);
        assert_eq!(ToolPermission::parse("disable"), Disable);
    }

    #[test]
    fn an_unrecognized_tool_prompts_when_callers_default_to_true() {
        // Callers pass `requires_confirmation = true` for names they
        // don't recognize; confirm that still prompts under every
        // permission that isn't an outright Disable.
        assert_eq!(decide(Ask, true, false), Prompt);
        assert_eq!(decide(Always, true, false), Prompt);
    }
}
