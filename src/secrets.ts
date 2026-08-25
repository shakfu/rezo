// OS-native credential storage wrapper. Backed by `rezon_core::secrets`
// via the Tauri commands in `crates/rezon-web/src/secrets.rs`.
//
// Note there is no `keychainGet`. Keys are write-only from the
// frontend's side: the backend resolves them itself at request time
// (runtime override -> keychain -> environment), so the webview never
// holds a plaintext key and never puts one on an IPC payload. Use
// `keychainHas` when the UI needs to know whether one is configured.
//
// Account naming convention: `<purpose>:<scope>`. For cloud API keys,
// `api_key:<provider_key>` — e.g. `api_key:openai`. Must match
// `rezon_core::secrets::cloud_api_key_account`; renaming either side
// orphans whatever was saved under the old name.

import { invoke } from "@tauri-apps/api/core";

export async function keychainSet(
  account: string,
  value: string,
): Promise<void> {
  return invoke<void>("keychain_set", { account, value });
}

export async function keychainDelete(account: string): Promise<void> {
  return invoke<void>("keychain_delete", { account });
}

/// Whether a secret is stored, without retrieving it.
export async function keychainHas(account: string): Promise<boolean> {
  return invoke<boolean>("keychain_has", { account });
}

/// Convention helper: account name for a cloud-provider API key.
/// Use this rather than constructing `"api_key:"+key` inline so
/// renames stay centralized.
export function cloudApiKeyAccount(providerKey: string): string {
  return `api_key:${providerKey}`;
}
