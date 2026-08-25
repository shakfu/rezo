pub mod agent;
mod embed;
mod llm;
mod search;
mod secrets;
mod vault;

use tauri::menu::{
    AboutMetadataBuilder, MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder,
};
use tauri::{Emitter, Manager};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Load `.env` before anything reads a key. Keeps the development
    // path working: a repo `.env` for `make dev`, and one under the app
    // config dir for packaged builds, which have no shell profile to
    // inherit from. Already-exported variables win over both.
    let loaded = rezon_core::dotenv::load();
    for p in &loaded.paths {
        eprintln!("loaded env from {}", p.display());
    }

    // sqlite-vec is registered as a SQLite auto-extension *before* any
    // Connection is opened. Every connection opened thereafter
    // (rusqlite or otherwise) gets `vec_*` and `vec0` available.
    search::register_sqlite_vec();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(std::sync::Arc::new(llm::LlmState::default()))
        .manage(agent::commands::AgentState::default())
        .manage(std::sync::Arc::new(embed::EmbedState::default()))
        .invoke_handler(tauri::generate_handler![
            llm::load_model,
            llm::model_status,
            llm::chat,
            llm::cancel_chat,
            llm::cloud_providers,
            llm::provider_models,
            agent::commands::agent_chat,
            agent::commands::cancel_agent,
            agent::commands::confirm_tool_call,
            agent::commands::tools_catalog,
            agent::commands::grant_tool_always,
            agent::commands::revoke_tool_always,
            agent::commands::tool_always_grants,
            vault::vault_list_tree,
            vault::vault_read,
            vault::vault_write,
            vault::vault_create,
            vault::vault_mkdir,
            vault::vault_delete,
            vault::vault_rename,
            vault::vault_resolve_wikilink,
            vault::vault_undo,
            vault::vault_redo,
            vault::vault_journal_recent,
            secrets::keychain_has,
            secrets::keychain_set,
            secrets::keychain_delete,
            search::vault_search,
            search::vault_index_open,
            search::vault_index_touch,
            search::vault_related,
            embed::embed_load_model,
            embed::embed_status,
            embed::vault_search_semantic,
        ])
        .setup(|app| {
            // ---- Native menu (macOS app menu + Edit) ----
            let settings_item = MenuItemBuilder::with_id("settings", "Settings...")
                .accelerator("CmdOrCtrl+,")
                .build(app)?;

            let about_metadata = AboutMetadataBuilder::new().name(Some("Rezon")).build();

            let app_menu = SubmenuBuilder::new(app, "Rezon")
                .item(&PredefinedMenuItem::about(
                    app,
                    Some("About Rezon"),
                    Some(about_metadata),
                )?)
                .separator()
                .item(&settings_item)
                .separator()
                .item(&PredefinedMenuItem::services(app, None)?)
                .separator()
                .item(&PredefinedMenuItem::hide(app, None)?)
                .item(&PredefinedMenuItem::hide_others(app, None)?)
                .item(&PredefinedMenuItem::show_all(app, None)?)
                .separator()
                .item(&PredefinedMenuItem::quit(app, None)?)
                .build()?;

            // Edit menu — required on macOS for Cut/Copy/Paste shortcuts to
            // work in text inputs once a custom menu is installed.
            let edit_menu = SubmenuBuilder::new(app, "Edit")
                .item(&PredefinedMenuItem::undo(app, None)?)
                .item(&PredefinedMenuItem::redo(app, None)?)
                .separator()
                .item(&PredefinedMenuItem::cut(app, None)?)
                .item(&PredefinedMenuItem::copy(app, None)?)
                .item(&PredefinedMenuItem::paste(app, None)?)
                .item(&PredefinedMenuItem::select_all(app, None)?)
                .build()?;

            let menu = MenuBuilder::new(app)
                .item(&app_menu)
                .item(&edit_menu)
                .build()?;
            app.set_menu(menu)?;

            app.on_menu_event(|app, event| {
                if event.id() == "settings" {
                    let _ = app.emit("open-settings", ());
                }
            });

            // ---- Search state (data_dir resolved here, not at builder time) ----
            let data_dir = app
                .path()
                .app_data_dir()
                .map_err(|e| format!("app_data_dir: {e}"))?;
            app.manage(std::sync::Arc::new(search::SearchState::new(data_dir)));

            // ---- Auto-load last models ----
            let handle = app.handle().clone();
            if let Some(path) = llm::read_last_model(&handle) {
                let h = handle.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = llm::do_load(&h, path).await {
                        eprintln!("auto-load failed: {e}");
                        let _ = h.emit("model-load-error", e);
                    }
                });
            }
            if let Some(path) = embed::read_last_embed_model(&handle) {
                let h = handle.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = embed::do_load_embed(&h, path).await {
                        eprintln!("embed auto-load failed: {e}");
                        let _ = h.emit("embed-load-error", e);
                    }
                });
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|handle, event| {
        if let tauri::RunEvent::Exit = event {
            // Cancel any in-flight agent run before tearing down the
            // local model state, so the agent loop's stream consumer
            // exits promptly.
            handle.state::<agent::commands::AgentState>().shutdown();
            handle.state::<std::sync::Arc<llm::LlmState>>().shutdown();
            handle
                .state::<std::sync::Arc<embed::EmbedState>>()
                .shutdown();
            handle
                .state::<std::sync::Arc<search::SearchState>>()
                .shutdown();
        }
    });
}
