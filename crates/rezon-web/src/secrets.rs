// Tauri command wrappers over `rezon_core::secrets`.
//
// The store itself lives in core so `rezon-tui` shares it and so key
// *resolution* (runtime -> keychain -> environment) is one policy in
// one place. This file is only the IPC surface.
//
// Note what is deliberately absent: there is no command that returns a
// stored key. The frontend can set one and ask whether one exists, but
// never reads the value back. A key the webview never holds is a key
// that cannot leak through devtools, a content-injection bug, or an
// IPC payload — and nothing in the UI needs the plaintext.

use rezon_core::secrets;

/// Write (or, with an empty value, delete) a secret.
#[tauri::command]
pub fn keychain_set(account: String, value: String) -> Result<(), String> {
    secrets::keyring_set(&account, &value)
}

/// Explicit delete. Idempotent on a missing entry.
#[tauri::command]
pub fn keychain_delete(account: String) -> Result<(), String> {
    secrets::keyring_delete(&account)
}

/// Whether a secret exists, without revealing it. This replaced a
/// `keychain_get` that handed the plaintext to the webview.
#[tauri::command]
pub fn keychain_has(account: String) -> Result<bool, String> {
    secrets::keyring_get(&account).map(|v| v.is_some())
}
