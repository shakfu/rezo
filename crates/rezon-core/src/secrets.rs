//! OS-native credential storage, and the abstraction key resolution
//! reads through.
//!
//! Lives in core rather than in the Tauri shell because *where a key
//! comes from* is backend policy, not a GUI concern: `rezon-tui` wants
//! the same store and the same precedence rules. The Tauri commands in
//! `rezon-web::secrets` are thin wrappers over this.
//!
//! Account naming convention is `<purpose>:<scope>` — for cloud API
//! keys, `api_key:<provider_key>` (e.g. `api_key:openai`). Stable
//! across versions; renaming an account orphans whatever was saved
//! under the old name. Use `cloud_api_key_account` rather than
//! formatting the string at call sites.

use keyring::Entry;

/// Service identifier passed to `keyring::Entry::new`. Stable across
/// versions — changing it would orphan every previously-saved secret.
const SERVICE: &str = "rezon-tui";

/// Account name for a cloud provider's API key.
pub fn cloud_api_key_account(provider_key: &str) -> String {
    format!("api_key:{provider_key}")
}

/// Read-only view of stored secrets, as key resolution needs it.
///
/// A trait rather than a direct `keyring` call so `resolve_cloud_config`
/// stays testable: the real store depends on the developer's login
/// keychain, which a test must not read, write, or depend on the state
/// of.
pub trait SecretStore: Send + Sync {
    /// Fetch a secret, or `None` when absent. Implementations swallow
    /// backend errors and report them as `None` — a locked or
    /// unavailable keyring should degrade to "no stored key" and let
    /// the next source in the chain answer, not abort the request.
    fn get(&self, account: &str) -> Option<String>;
}

/// The real OS-backed store.
pub struct KeyringStore;

impl SecretStore for KeyringStore {
    fn get(&self, account: &str) -> Option<String> {
        keyring_get(account).ok().flatten()
    }
}

/// A store that never has anything. Used where a keyring is
/// deliberately not consulted.
pub struct NullStore;

impl SecretStore for NullStore {
    fn get(&self, _account: &str) -> Option<String> {
        None
    }
}

/// Read a secret. `Ok(None)` when no entry exists; `Err` for unexpected
/// backend failures (corrupt store, denied access) so a UI can tell the
/// user something is wrong with the keychain itself.
pub fn keyring_get(account: &str) -> Result<Option<String>, String> {
    let entry = Entry::new(SERVICE, account).map_err(|e| format!("keyring: {e}"))?;
    match entry.get_password() {
        Ok(s) => Ok(Some(s)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("keyring get {account}: {e}")),
    }
}

/// Write (or overwrite) a secret. An empty value deletes, so a UI that
/// clears its field does the obvious thing without a separate command.
pub fn keyring_set(account: &str, value: &str) -> Result<(), String> {
    let entry = Entry::new(SERVICE, account).map_err(|e| format!("keyring: {e}"))?;
    if value.is_empty() {
        return match entry.delete_credential() {
            Ok(_) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!("keyring delete {account}: {e}")),
        };
    }
    entry
        .set_password(value)
        .map_err(|e| format!("keyring set {account}: {e}"))
}

/// Explicit delete. Idempotent on a missing entry.
pub fn keyring_delete(account: &str) -> Result<(), String> {
    let entry = Entry::new(SERVICE, account).map_err(|e| format!("keyring: {e}"))?;
    match entry.delete_credential() {
        Ok(_) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("keyring delete {account}: {e}")),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::SecretStore;
    use std::collections::HashMap;

    /// In-memory store for tests. Never touches the real keychain.
    pub struct MapStore(pub HashMap<String, String>);

    impl MapStore {
        pub fn empty() -> Self {
            MapStore(HashMap::new())
        }

        pub fn with(account: &str, value: &str) -> Self {
            let mut m = HashMap::new();
            m.insert(account.to_string(), value.to_string());
            MapStore(m)
        }
    }

    impl SecretStore for MapStore {
        fn get(&self, account: &str) -> Option<String> {
            self.0.get(account).cloned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_naming_is_stable() {
        // Renaming this orphans every key a user has already saved.
        assert_eq!(cloud_api_key_account("openai"), "api_key:openai");
        assert_eq!(cloud_api_key_account("other"), "api_key:other");
    }

    #[test]
    fn null_store_never_answers() {
        assert_eq!(NullStore.get("api_key:openai"), None);
    }
}
