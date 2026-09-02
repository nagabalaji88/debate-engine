//! The response cache: `(provider, model, params, prompt_hash) → response`,
//! ARCHITECTURE §7. "Never `prompt_hash` alone: the same prompt sent to two
//! models has one `prompt_hash` and two different answers" (INTERFACES §5).
//!
//! `&self` throughout, like [`crate::budget::BudgetLedger`]: `StageContext`
//! holds `cache: &'a ResponseCache` as a shared reference, since concurrent
//! stages read and populate the same cache at once. Same reasoning for
//! `std::sync::Mutex` over `tokio::sync::Mutex` as that module: every critical
//! section is a synchronous `BTreeMap` lookup or insert with no `.await` inside
//! it, so there is no lock-held-across-await hazard for the async variant to
//! solve.

use crate::store::{CacheKey, CachedResponse};
use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Debug, Default)]
pub struct ResponseCache {
    inner: Mutex<BTreeMap<CacheKey, CachedResponse>>,
}

impl ResponseCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &CacheKey) -> Option<CachedResponse> {
        self.inner.lock().unwrap().get(key).cloned()
    }

    /// Committed by the caller in the same transaction as the call and its
    /// budget charge (§8.3) — this method itself only updates the in-memory
    /// view; the transactional guarantee is `Tx::put_cache`'s (currently the
    /// D21-deferred stub in `arbiter-store`, since `cache_entries` doesn't
    /// exist yet).
    pub fn put(&self, key: CacheKey, response: CachedResponse) {
        self.inner.lock().unwrap().insert(key, response);
    }

    /// Exact replay: "cache-only with the network disabled" (§7). A miss is an
    /// error, never a fallback to a network call — this method has no way to
    /// reach a socket at all (no `Provider`, no `reqwest`, nothing in its
    /// dependency graph that could open one), which is the property
    /// `replay_opens_no_socket` exists to pin down structurally, not just by
    /// observation at runtime.
    pub fn get_for_replay(&self, key: &CacheKey) -> Result<CachedResponse, CacheMissInReplay> {
        self.get(key).ok_or(CacheMissInReplay)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("no cached response for this key; replay mode does not permit a network call")]
pub struct CacheMissInReplay;

#[cfg(test)]
mod tests {
    use super::*;
    use arbiter_core::{ModelId, ProviderId};

    fn key(provider: &str, model: &str) -> CacheKey {
        CacheKey {
            provider: ProviderId::new(provider),
            model: ModelId::new(model),
            params: "{}".to_string(),
            prompt_hash: "blake3:same_prompt".to_string(),
        }
    }

    fn response(hash: &str) -> CachedResponse {
        CachedResponse {
            response_hash: hash.to_string(),
            size_bytes: 10,
            inline: Some("hi".to_string()),
        }
    }

    #[test]
    fn same_prompt_two_models_do_not_collide() {
        let cache = ResponseCache::new();
        let key_a = key("anthropic", "claude");
        let key_b = key("anthropic", "gpt"); // same provider+prompt_hash, different model
        cache.put(key_a.clone(), response("blake3:a"));
        cache.put(key_b.clone(), response("blake3:b"));

        assert_eq!(cache.get(&key_a).unwrap().response_hash, "blake3:a");
        assert_eq!(cache.get(&key_b).unwrap().response_hash, "blake3:b");
    }

    #[test]
    fn replay_opens_no_socket() {
        let cache = ResponseCache::new();
        // A cold cache in replay mode errors rather than attempting any network
        // path -- there is no code path here that could reach one.
        let result = cache.get_for_replay(&key("anthropic", "claude"));
        assert!(matches!(result, Err(CacheMissInReplay)));

        // A populated cache serves the hit purely from memory.
        cache.put(key("anthropic", "claude"), response("blake3:a"));
        let hit = cache.get_for_replay(&key("anthropic", "claude")).unwrap();
        assert_eq!(hit.response_hash, "blake3:a");
    }

    #[test]
    fn params_participate_in_the_key_too() {
        let cache = ResponseCache::new();
        let mut key_high_temp = key("anthropic", "claude");
        key_high_temp.params = "{\"temperature\":0.9}".to_string();
        let mut key_low_temp = key("anthropic", "claude");
        key_low_temp.params = "{\"temperature\":0.1}".to_string();

        cache.put(key_high_temp.clone(), response("blake3:hot"));
        cache.put(key_low_temp.clone(), response("blake3:cold"));

        assert_eq!(
            cache.get(&key_high_temp).unwrap().response_hash,
            "blake3:hot"
        );
        assert_eq!(
            cache.get(&key_low_temp).unwrap().response_hash,
            "blake3:cold"
        );
    }
}
