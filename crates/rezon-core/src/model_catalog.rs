//! Live model lists from a provider, cached on disk, layered over the
//! curated catalog in `models.json`.
//!
//! **Augments rather than replaces.** `models.json` is not merely a
//! list, it encodes judgment: its own comment says the first entry
//! should be the cheapest/default for that provider. A provider's
//! `/v1/models` cannot reproduce that — the response carries only
//! `id`, `object`, `created`, `owned_by`, with nothing marking a model
//! as chat-capable, so OpenAI's list arrives mixed with embeddings,
//! audio, image, and moderation models, and OpenRouter's runs to
//! hundreds of entries. Swapping the curated six for that would be a
//! downgrade. So the recommended list stays first and the fetched list
//! follows it, deduplicated.
//!
//! **Fetching is lazy, never at launch.** `/v1/models` is
//! authenticated on the named providers, so at first run there is no
//! key to fetch with. The trigger is "a provider is selected and a key
//! resolves", or an explicit refresh — not application startup.
//!
//! **Failure ladder:** fresh cache -> live fetch -> stale cache ->
//! recommended only. Every rung is usable; none of them block the UI,
//! and the model field is free text regardless, so a model released
//! after the last fetch can always be typed in.
//!
//! The cache is keyed by provider *and* a fingerprint of the API key:
//! org-gated preview models mean two keys can see different lists from
//! the same provider. The key itself is never stored — only a
//! truncated SHA-256 of it, which is enough to notice a change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// How long a fetched list is considered current. Model catalogs move
/// on the order of weeks; a day keeps the list fresh without making
/// every provider switch a network round trip.
pub const DEFAULT_TTL_SECS: u64 = 24 * 60 * 60;

/// Cache file name under the config dir.
pub const CACHE_FILE: &str = "model-cache.json";

/// One provider's cached list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCache {
    /// Unix seconds at fetch time.
    pub fetched_at: u64,
    /// Fingerprint of the key the list was fetched with. A different
    /// key can legitimately see a different set of models.
    pub key_fingerprint: String,
    pub models: Vec<String>,
}

/// The whole cache file: provider key -> entry. A `BTreeMap` so the
/// serialized form has a stable order and diffs cleanly.
pub type CacheFile = BTreeMap<String, ProviderCache>;

/// Non-reversible fingerprint of an API key.
///
/// Used to detect "the key changed, the cached list may be wrong". 16
/// hex characters of SHA-256 is far more than enough to distinguish
/// the handful of keys one user has, and storing the key itself would
/// undo the point of keeping it in the keychain.
pub fn key_fingerprint(api_key: &str) -> String {
    let mut h = Sha256::new();
    h.update(api_key.as_bytes());
    format!("{:x}", h.finalize())[..16].to_string()
}

/// Unix seconds now. Falls back to 0 if the clock is before the epoch,
/// which reads as "infinitely stale" and simply forces a refetch.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether a cache entry is current for this key.
///
/// A future `fetched_at` (clock moved backwards, or a file copied from
/// another machine) counts as stale rather than eternally fresh —
/// `saturating_sub` yields 0 elapsed, so this returns true... which is
/// wrong. Compare explicitly instead.
pub fn is_fresh(entry: &ProviderCache, key_fp: &str, now: u64, ttl: u64) -> bool {
    if entry.key_fingerprint != key_fp {
        return false;
    }
    if entry.fetched_at > now {
        // Clock skew. Refetch rather than trust it indefinitely.
        return false;
    }
    now - entry.fetched_at < ttl
}

/// Where the cache lives, or `None` when no config dir resolves.
pub fn cache_path() -> Option<PathBuf> {
    crate::paths::config_file(CACHE_FILE)
}

/// Read the cache. A missing or corrupt file is an empty cache, not an
/// error: this is a performance aid, and refusing to start over a bad
/// JSON file would be absurd.
pub fn load_cache(path: &Path) -> CacheFile {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Write the cache, creating the directory if needed.
pub fn save_cache(path: &Path, cache: &CacheFile) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {:?}: {e}", parent))?;
    }
    let body = serde_json::to_string_pretty(cache).map_err(|e| format!("serialize: {e}"))?;
    std::fs::write(path, body).map_err(|e| format!("write {:?}: {e}", path))
}

/// Merge the curated list with a fetched one.
///
/// Recommended entries keep their order and come first — that order is
/// the judgment `models.json` exists to encode. Fetched entries follow,
/// with anything already recommended removed so nothing appears twice.
pub fn merge(recommended: &[String], fetched: &[String]) -> Vec<String> {
    let mut out: Vec<String> = recommended.to_vec();
    for m in fetched {
        if !out.iter().any(|r| r == m) {
            out.push(m.clone());
        }
    }
    out
}

/// Where a returned list came from, so a UI can say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CatalogSource {
    /// Served from a cache entry still inside its TTL.
    Cache,
    /// Fetched from the provider during this call.
    Fetched,
    /// Fetch failed; a cache entry past its TTL was used instead.
    StaleCache,
    /// No fetched list available at all.
    RecommendedOnly,
}

/// A provider's model list, plus how it was obtained.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    /// Curated entries, in `models.json` order.
    pub recommended: Vec<String>,
    /// Everything the provider reported that is not already
    /// recommended. Empty when no fetch has ever succeeded.
    pub fetched: Vec<String>,
    pub source: CatalogSource,
    /// Unix seconds of the fetch behind `fetched`, when there is one.
    pub fetched_at: Option<u64>,
    /// Why a fetch failed, when one was attempted and did not work.
    /// Advisory — the catalog is still usable.
    pub error: Option<String>,
}

/// List a provider's models over its OpenAI-compatible endpoint.
///
/// Ids are sorted for a stable dropdown; no filtering is attempted,
/// because the response carries nothing that reliably distinguishes a
/// chat model from an embedding or audio one, and guessing from id
/// prefixes is exactly the heuristic that breaks when a provider
/// renames a family.
pub async fn fetch_models(base_url: &str, api_key: &str) -> Result<Vec<String>, String> {
    use async_openai::config::OpenAIConfig;
    use async_openai::Client;

    let cfg = OpenAIConfig::new()
        .with_api_key(api_key)
        .with_api_base(base_url);
    let client = Client::with_config(cfg);
    let resp = client
        .models()
        .list()
        .await
        .map_err(|e| format!("list models: {e}"))?;
    let mut ids: Vec<String> = resp.data.into_iter().map(|m| m.id).collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// Assemble a provider's catalog, applying the failure ladder.
///
/// `refresh` forces a fetch even when the cache is current — the
/// explicit "I just got access to a new model" case.
///
/// Never returns an error: the recommended list is always a usable
/// answer, so a fetch failure downgrades the result instead of failing
/// the call. Any error is reported on `Catalog::error` for the UI to
/// mention quietly.
pub async fn resolve_catalog(
    provider_key: &str,
    base_url: &str,
    api_key: Option<&str>,
    recommended: &[String],
    cache_file: &Path,
    refresh: bool,
    ttl: u64,
) -> Catalog {
    let mut cache = load_cache(cache_file);
    let now = now_secs();

    // With no key there is nothing to fetch with and nothing to key a
    // cache entry on: the named providers authenticate this endpoint.
    let Some(api_key) = api_key.map(str::trim).filter(|s| !s.is_empty()) else {
        return Catalog {
            recommended: recommended.to_vec(),
            fetched: Vec::new(),
            source: CatalogSource::RecommendedOnly,
            fetched_at: None,
            error: None,
        };
    };
    let fp = key_fingerprint(api_key);

    if !refresh {
        if let Some(hit) = cache
            .get(provider_key)
            .filter(|e| is_fresh(e, &fp, now, ttl))
        {
            return Catalog {
                recommended: recommended.to_vec(),
                fetched: without(&hit.models, recommended),
                source: CatalogSource::Cache,
                fetched_at: Some(hit.fetched_at),
                error: None,
            };
        }
    }

    match fetch_models(base_url, api_key).await {
        Ok(models) => {
            let entry = ProviderCache {
                fetched_at: now,
                key_fingerprint: fp,
                models: models.clone(),
            };
            cache.insert(provider_key.to_string(), entry);
            // A cache that cannot be written is not worth failing over;
            // the list is already in hand for this session.
            let _ = save_cache(cache_file, &cache);
            Catalog {
                recommended: recommended.to_vec(),
                fetched: without(&models, recommended),
                source: CatalogSource::Fetched,
                fetched_at: Some(now),
                error: None,
            }
        }
        Err(e) => {
            // Offline, rate-limited, or a provider without the
            // endpoint. A stale list beats no list.
            if let Some(stale) = cache.get(provider_key).filter(|c| c.key_fingerprint == fp) {
                return Catalog {
                    recommended: recommended.to_vec(),
                    fetched: without(&stale.models, recommended),
                    source: CatalogSource::StaleCache,
                    fetched_at: Some(stale.fetched_at),
                    error: Some(e),
                };
            }
            Catalog {
                recommended: recommended.to_vec(),
                fetched: Vec::new(),
                source: CatalogSource::RecommendedOnly,
                fetched_at: None,
                error: Some(e),
            }
        }
    }
}

/// `all` minus anything already in `exclude`, order preserved.
fn without(all: &[String], exclude: &[String]) -> Vec<String> {
    all.iter()
        .filter(|m| !exclude.iter().any(|r| &r == m))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn entry(fetched_at: u64, fp: &str, models: &[&str]) -> ProviderCache {
        ProviderCache {
            fetched_at,
            key_fingerprint: fp.to_string(),
            models: models.iter().map(|s| s.to_string()).collect(),
        }
    }

    // ---- Fingerprinting ---------------------------------------------

    #[test]
    fn fingerprint_is_stable_and_distinguishes_keys() {
        assert_eq!(key_fingerprint("sk-a"), key_fingerprint("sk-a"));
        assert_ne!(key_fingerprint("sk-a"), key_fingerprint("sk-b"));
    }

    #[test]
    fn fingerprint_does_not_contain_the_key() {
        // The whole point of keeping keys in the keychain is undone if
        // the cache file quietly holds one.
        let fp = key_fingerprint("sk-supersecret-value");
        assert!(!fp.contains("supersecret"));
        assert_eq!(fp.len(), 16);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ---- Freshness ---------------------------------------------------

    #[test]
    fn a_recent_entry_for_the_same_key_is_fresh() {
        let e = entry(1_000, "fp1", &["a"]);
        assert!(is_fresh(&e, "fp1", 1_500, DEFAULT_TTL_SECS));
    }

    #[test]
    fn an_entry_past_its_ttl_is_stale() {
        let e = entry(1_000, "fp1", &["a"]);
        assert!(!is_fresh(
            &e,
            "fp1",
            1_000 + DEFAULT_TTL_SECS,
            DEFAULT_TTL_SECS
        ));
    }

    #[test]
    fn a_different_key_invalidates_regardless_of_age() {
        // Org-gated preview models mean two keys legitimately see
        // different lists from the same provider.
        let e = entry(1_000, "fp1", &["a"]);
        assert!(!is_fresh(&e, "fp2", 1_001, DEFAULT_TTL_SECS));
    }

    #[test]
    fn a_future_timestamp_counts_as_stale() {
        // Clock skew, or a config dir copied between machines. Naive
        // `now - fetched_at` would underflow or read as freshly
        // fetched forever.
        let e = entry(9_999, "fp1", &["a"]);
        assert!(!is_fresh(&e, "fp1", 1_000, DEFAULT_TTL_SECS));
    }

    // ---- Merge -------------------------------------------------------

    #[test]
    fn merge_keeps_recommended_order_first() {
        // That order is the curation: cheapest/default leads.
        let rec = vec!["cheap".to_string(), "mid".to_string()];
        let fetched = vec!["aaa".to_string(), "zzz".to_string()];
        assert_eq!(merge(&rec, &fetched), vec!["cheap", "mid", "aaa", "zzz"]);
    }

    #[test]
    fn merge_drops_fetched_duplicates() {
        let rec = vec!["gpt-x".to_string()];
        let fetched = vec!["gpt-x".to_string(), "gpt-y".to_string()];
        assert_eq!(merge(&rec, &fetched), vec!["gpt-x", "gpt-y"]);
    }

    #[test]
    fn merge_with_nothing_fetched_is_just_the_recommended_list() {
        let rec = vec!["a".to_string(), "b".to_string()];
        assert_eq!(merge(&rec, &[]), rec);
    }

    // ---- Cache file --------------------------------------------------

    #[test]
    fn cache_round_trips() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("sub").join(CACHE_FILE);
        let mut c = CacheFile::new();
        c.insert("openai".to_string(), entry(42, "fp1", &["a", "b"]));

        // Parent directory does not exist yet.
        save_cache(&p, &c).unwrap();
        assert_eq!(load_cache(&p), c);
    }

    #[test]
    fn a_missing_cache_file_reads_as_empty() {
        let dir = TempDir::new().unwrap();
        assert!(load_cache(&dir.path().join("nope.json")).is_empty());
    }

    #[test]
    fn a_corrupt_cache_file_reads_as_empty_rather_than_failing() {
        // This is a performance aid. Refusing to work because of a bad
        // JSON file would be the wrong trade.
        let dir = TempDir::new().unwrap();
        let p = dir.path().join(CACHE_FILE);
        std::fs::write(&p, "{ not json").unwrap();
        assert!(load_cache(&p).is_empty());
    }

    // ---- fetch_models over a real socket -----------------------------

    async fn serve(body: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = body.to_string();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
        });
        format!("http://{addr}/v1")
    }

    #[tokio::test]
    async fn fetch_returns_sorted_deduplicated_ids() {
        let base = serve(
            r#"{"object":"list","data":[
                {"id":"zeta","object":"model","created":1,"owned_by":"o"},
                {"id":"alpha","object":"model","created":2,"owned_by":"o"},
                {"id":"alpha","object":"model","created":3,"owned_by":"o"}
            ]}"#,
        )
        .await;
        let got = fetch_models(&base, "sk-test").await.unwrap();
        assert_eq!(got, vec!["alpha", "zeta"]);
    }

    #[tokio::test]
    async fn fetch_handles_an_empty_catalog() {
        let base = serve(r#"{"object":"list","data":[]}"#).await;
        assert!(fetch_models(&base, "sk-test").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn fetch_reports_an_unreachable_endpoint_as_an_error() {
        // Port 1 is not listening. The caller falls back to cache or
        // to the recommended list; it must not panic or hang.
        let err = fetch_models("http://127.0.0.1:1/v1", "sk-test")
            .await
            .unwrap_err();
        assert!(err.contains("list models"), "got: {err}");
    }

    // ---- resolve_catalog: the failure ladder -------------------------

    fn rec() -> Vec<String> {
        vec!["cheap-model".to_string(), "big-model".to_string()]
    }

    #[tokio::test]
    async fn no_key_yields_recommended_only_without_touching_the_network() {
        // First run: nothing to authenticate with. Must not error, and
        // must not attempt a fetch (the base URL here would refuse).
        let dir = TempDir::new().unwrap();
        let c = resolve_catalog(
            "openai",
            "http://127.0.0.1:1/v1",
            None,
            &rec(),
            &dir.path().join(CACHE_FILE),
            false,
            DEFAULT_TTL_SECS,
        )
        .await;
        assert_eq!(c.source, CatalogSource::RecommendedOnly);
        assert_eq!(c.recommended, rec());
        assert!(c.fetched.is_empty());
        assert!(c.error.is_none(), "absence of a key is not an error");
    }

    #[tokio::test]
    async fn a_blank_key_is_treated_as_no_key() {
        let dir = TempDir::new().unwrap();
        let c = resolve_catalog(
            "openai",
            "http://127.0.0.1:1/v1",
            Some("   "),
            &rec(),
            &dir.path().join(CACHE_FILE),
            false,
            DEFAULT_TTL_SECS,
        )
        .await;
        assert_eq!(c.source, CatalogSource::RecommendedOnly);
        assert!(c.error.is_none());
    }

    #[tokio::test]
    async fn a_successful_fetch_is_merged_and_written_to_cache() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(CACHE_FILE);
        let base = serve(
            r#"{"object":"list","data":[
                {"id":"cheap-model","object":"model","created":1,"owned_by":"o"},
                {"id":"exotic","object":"model","created":2,"owned_by":"o"}
            ]}"#,
        )
        .await;

        let c = resolve_catalog(
            "openai",
            &base,
            Some("sk-1"),
            &rec(),
            &path,
            false,
            DEFAULT_TTL_SECS,
        )
        .await;
        assert_eq!(c.source, CatalogSource::Fetched);
        // Already recommended, so not repeated in `fetched`.
        assert_eq!(c.fetched, vec!["exotic"]);
        assert_eq!(
            merge(&c.recommended, &c.fetched),
            vec!["cheap-model", "big-model", "exotic"]
        );

        let cached = load_cache(&path);
        assert_eq!(cached["openai"].models, vec!["cheap-model", "exotic"]);
        assert_eq!(cached["openai"].key_fingerprint, key_fingerprint("sk-1"));
    }

    #[tokio::test]
    async fn a_fresh_cache_entry_is_served_without_a_fetch() {
        // The one-shot server is already consumed, so an attempted
        // fetch would fail — reaching CatalogSource::Cache proves none
        // was made.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(CACHE_FILE);
        let mut c = CacheFile::new();
        c.insert(
            "openai".to_string(),
            entry(now_secs(), &key_fingerprint("sk-1"), &["cached-only"]),
        );
        save_cache(&path, &c).unwrap();

        let got = resolve_catalog(
            "openai",
            "http://127.0.0.1:1/v1",
            Some("sk-1"),
            &rec(),
            &path,
            false,
            DEFAULT_TTL_SECS,
        )
        .await;
        assert_eq!(got.source, CatalogSource::Cache);
        assert_eq!(got.fetched, vec!["cached-only"]);
        assert!(got.error.is_none());
    }

    #[tokio::test]
    async fn refresh_bypasses_a_fresh_cache() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(CACHE_FILE);
        let mut c = CacheFile::new();
        c.insert(
            "openai".to_string(),
            entry(now_secs(), &key_fingerprint("sk-1"), &["stale-entry"]),
        );
        save_cache(&path, &c).unwrap();

        let base = serve(
            r#"{"object":"list","data":[{"id":"brand-new","object":"model","created":1,"owned_by":"o"}]}"#,
        )
        .await;
        let got = resolve_catalog(
            "openai",
            &base,
            Some("sk-1"),
            &rec(),
            &path,
            true,
            DEFAULT_TTL_SECS,
        )
        .await;
        assert_eq!(got.source, CatalogSource::Fetched);
        assert_eq!(got.fetched, vec!["brand-new"]);
    }

    #[tokio::test]
    async fn a_failed_fetch_falls_back_to_a_stale_entry() {
        // Offline, rate-limited, or a provider whose compat layer has
        // no /v1/models. A stale list beats no list.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(CACHE_FILE);
        let mut c = CacheFile::new();
        c.insert(
            "openai".to_string(),
            entry(1_000, &key_fingerprint("sk-1"), &["old-model"]),
        );
        save_cache(&path, &c).unwrap();

        let got = resolve_catalog(
            "openai",
            "http://127.0.0.1:1/v1",
            Some("sk-1"),
            &rec(),
            &path,
            false,
            DEFAULT_TTL_SECS,
        )
        .await;
        assert_eq!(got.source, CatalogSource::StaleCache);
        assert_eq!(got.fetched, vec!["old-model"]);
        assert!(got.error.is_some(), "the failure should still be reported");
        assert_eq!(got.fetched_at, Some(1_000));
    }

    #[tokio::test]
    async fn a_failed_fetch_with_no_cache_still_returns_the_recommended_list() {
        let dir = TempDir::new().unwrap();
        let got = resolve_catalog(
            "openai",
            "http://127.0.0.1:1/v1",
            Some("sk-1"),
            &rec(),
            &dir.path().join(CACHE_FILE),
            false,
            DEFAULT_TTL_SECS,
        )
        .await;
        assert_eq!(got.source, CatalogSource::RecommendedOnly);
        assert_eq!(got.recommended, rec());
        assert!(got.error.is_some());
    }

    #[tokio::test]
    async fn a_stale_entry_from_a_different_key_is_not_used() {
        // Falling back to another key's list would show models this
        // key cannot actually call.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(CACHE_FILE);
        let mut c = CacheFile::new();
        c.insert(
            "openai".to_string(),
            entry(1_000, &key_fingerprint("sk-OTHER"), &["other-key-model"]),
        );
        save_cache(&path, &c).unwrap();

        let got = resolve_catalog(
            "openai",
            "http://127.0.0.1:1/v1",
            Some("sk-1"),
            &rec(),
            &path,
            false,
            DEFAULT_TTL_SECS,
        )
        .await;
        assert_eq!(got.source, CatalogSource::RecommendedOnly);
        assert!(got.fetched.is_empty());
    }

    #[tokio::test]
    async fn one_provider_s_cache_does_not_answer_for_another() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(CACHE_FILE);
        let mut c = CacheFile::new();
        c.insert(
            "openai".to_string(),
            entry(now_secs(), &key_fingerprint("sk-1"), &["openai-model"]),
        );
        save_cache(&path, &c).unwrap();

        let got = resolve_catalog(
            "anthropic",
            "http://127.0.0.1:1/v1",
            Some("sk-1"),
            &rec(),
            &path,
            false,
            DEFAULT_TTL_SECS,
        )
        .await;
        assert_eq!(got.source, CatalogSource::RecommendedOnly);
    }
}
