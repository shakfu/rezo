// Built-in tools. Each tool lives in its own file.
//
// `register_core_tools` stacks the shell-independent tools. Tools that
// need long-lived state (e.g. `search_notes` reaching into SearchState
// + EmbedState) ship their own register helper that takes the state
// at construction time.

pub mod current_time;
pub mod file_read;
pub mod read_note;
pub mod search_notes;
pub mod shell_exec;
pub mod web_fetch;
pub mod write_note;

use std::sync::Arc;

use crate::agent::tool::ToolRegistry;
use crate::embed::EmbedState;
use crate::search::SearchState;

/// Register the shell-independent tools onto an existing registry.
pub fn register_core_tools(reg: &mut ToolRegistry) {
    reg.register(Arc::new(current_time::CurrentTime));
    reg.register(Arc::new(file_read::FileRead));
    reg.register(Arc::new(web_fetch::WebFetch));
    reg.register(Arc::new(shell_exec::ShellExec::default()));
}

/// Register `search_notes`, which needs the shared SearchState +
/// EmbedState. Shells decide when (or whether) to register it.
pub fn register_search_notes(
    reg: &mut ToolRegistry,
    search: Arc<SearchState>,
    embed: Arc<EmbedState>,
) {
    reg.register(Arc::new(search_notes::SearchNotes::new(
        search.clone(),
        embed,
    )));
    // `read_note` ships alongside `search_notes` — both are
    // read-only and both need a live `SearchState::active_vault`.
    // Registering them as a pair keeps the agent's read story
    // coherent: search to find, read to fetch.
    reg.register(Arc::new(read_note::ReadNote::new(search)));
}

/// Register the vault-write tools (`write_note`, `append_note`,
/// `edit_note`). All three need an open vault and all three gate on
/// user confirmation. The function keeps its historical name so the
/// shell call sites stay terse and symmetrical with
/// `register_search_notes`.
pub fn register_write_note(reg: &mut ToolRegistry, search: Arc<SearchState>) {
    reg.register(Arc::new(write_note::WriteNote::new(search.clone())));
    reg.register(Arc::new(write_note::AppendNote::new(search.clone())));
    reg.register(Arc::new(write_note::EditNote::new(search.clone())));
    reg.register(Arc::new(write_note::UndoNote::new(search)));
}
