//! Shared HTTP client + emote cache for the REST emote providers (7TV/BTTV/FFZ).
//!
//! All three providers fetch a global set and per-channel sets from a REST API and
//! map them to [`Emote`]s. They share one `reqwest::Client` (so HTTP keep-alive /
//! connection pooling is reused) and one process-wide, URL-keyed cache, so the
//! (identical) global set isn't re-fetched once per connection and a channel's set
//! survives the reconnect storm a login flip causes (every tab re-joins). The
//! cache keys on the full request URL, which never collides across providers.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use bks_core::Emote;

/// The preferred emote scale (`1` or `2`), from the shared `bks-core` setting (set
/// once at startup from the display DPI). Providers fetch this size — a larger
/// variant than the display needs is wasted bytes + decode + memory.
pub(crate) fn preferred_scale() -> u8 {
    bks_core::preferred_scale()
}

/// How long a cached fetch stays fresh. Emote sets change rarely; this only needs
/// to outlive a session's reconnect storms while still picking up changes if the
/// app runs for hours.
const CACHE_TTL: Duration = Duration::from_secs(600);

/// A cached fetch: the emotes plus when they were stored (for TTL).
type CacheEntry = (Instant, Vec<Emote>);

/// The process-wide cache of emote fetches keyed by request URL.
fn cache() -> &'static Mutex<HashMap<String, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Returns the cached emotes for `url` if still within [`CACHE_TTL`].
pub(crate) fn cache_get(url: &str) -> Option<Vec<Emote>> {
    let map = cache().lock().unwrap();
    map.get(url)
        .and_then(|(at, emotes)| (at.elapsed() < CACHE_TTL).then(|| emotes.clone()))
}

/// Stores `emotes` for `url` with the current time as the freshness stamp, and
/// drops any entries past their TTL (they'd otherwise accumulate forever — the
/// map is only ever written, never read-through-and-removed).
pub(crate) fn cache_put(url: &str, emotes: &[Emote]) {
    let mut map = cache().lock().unwrap();
    map.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
    map.insert(url.to_string(), (Instant::now(), emotes.to_vec()));
}

/// The shared scaffold of every provider fetch (7TV/BTTV/FFZ, global and
/// per-channel): serve from the process-wide cache, else GET `url`, optionally
/// treat a 404 as an empty set (`not_found_is_empty` — a channel with no
/// account on the provider), parse the JSON body, `map` it to emotes, and
/// cache the result (an empty set caches too, so a 404 isn't retried).
pub(crate) async fn fetch_cached<Resp: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    not_found_is_empty: bool,
    map: impl FnOnce(Resp) -> Vec<Emote>,
) -> anyhow::Result<Vec<Emote>> {
    if let Some(cached) = cache_get(url) {
        return Ok(cached);
    }
    let resp = client.get(url).send().await?;
    if not_found_is_empty && resp.status() == reqwest::StatusCode::NOT_FOUND {
        cache_put(url, &[]);
        return Ok(Vec::new());
    }
    let body: Resp = resp.error_for_status()?.json().await?;
    let emotes = map(body);
    cache_put(url, &emotes);
    Ok(emotes)
}

/// One shared `reqwest::Client` for all emote fetches (and the 7TV GraphQL client
/// in `seventv_api`), reusing keep-alive / connection pooling across providers.
/// Timeouts are set here — reqwest's default has none, so a stalled connection
/// would hang a provider load forever.
pub(crate) fn shared_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(20))
                .build()
                .expect("building emotes http client")
        })
        .clone()
}

/// A **crate-wide** lock for tests that mutate the process-global [`PREFERRED_SCALE`].
/// All size-sensitive tests across every provider module acquire this so they run
/// serially relative to each other (the scale is one global; per-module locks
/// wouldn't serialize across modules). Held for the test's duration.
#[cfg(test)]
pub(crate) fn scale_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_round_trips_within_ttl() {
        let url = "https://example.test/cache-round-trip";
        assert!(cache_get(url).is_none());
        cache_put(
            url,
            &[Emote {
                id: "1".into(),
                name: "Kappa".into(),
                url: "u".into(),
                animated: false,
                tooltip: Default::default(),
            }],
        );
        let got = cache_get(url).expect("freshly cached value should be present");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "Kappa");
    }

    #[test]
    fn cache_treats_empty_set_as_a_hit() {
        // A 404 channel caches `[]`; a later load must hit (return empty), not miss
        // and re-fetch.
        let url = "https://example.test/empty-set";
        cache_put(url, &[]);
        assert!(cache_get(url).is_some());
    }
}
