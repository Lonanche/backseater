//! Link previews: given a chat link, resolve a small render-agnostic card of
//! metadata (title / author / view count / thumbnail) for it.
//!
//! This is the *expandability seam* for link previews, mirroring the emote
//! provider seam. A [`LinkPreviewProvider`] answers "is this my kind of link,
//! and what's its metadata?" for one source (YouTube videos today; Twitch clips,
//! Kick clips, … later — each is one more provider, nothing else changes). The
//! result ([`LinkPreview`]) is *render-agnostic*: the same struct feeds a hover
//! tooltip today and (designed-for, not built) an inline in-chat card later.
//!
//! **No GUI, no runtime here.** The crate defines the trait, the data, and a
//! process-wide [`PreviewCache`] that dedupes fetches by provider and target (a link posted five
//! times, hovered twice, and shown inline all share one fetch). The *driving* of
//! the async fetch — spawning it on a tokio runtime and storing the result — is
//! the app's job (it owns the runtime, like the image cache), so this crate
//! stays dependency-light and testable.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;

/// Parses a preview link before a provider checks its exact host and path.
pub fn web_url(input: &str) -> Option<url::Url> {
    if input.len() > 4096 {
        return None;
    }
    let url = url::Url::parse(input)
        .or_else(|error| match error {
            url::ParseError::RelativeUrlWithoutBase => url::Url::parse(&format!("https://{input}")),
            error => Err(error),
        })
        .ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none())
    .then_some(url)
}

/// What kind of thing a link points at — lets the UI label/style the card and
/// grows as providers are added.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PreviewKind {
    /// A video (YouTube watch/live/short, …).
    Video,
    /// A clip (Twitch clip, …) — reserved for the future clip provider.
    Clip,
}

/// The resolved metadata for a link — the only contract between a provider and
/// whatever renders the preview. Render-agnostic on purpose: a tooltip and an
/// inline card both just read these fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkPreview {
    pub kind: PreviewKind,
    /// The video/clip title.
    pub title: String,
    /// The channel / uploader / streamer name.
    pub author: String,
    /// A thumbnail image URL, if the source has one.
    pub thumbnail_url: Option<String>,
    /// A short human stats line ("1.2M views", "45K views"), if available.
    pub stats: Option<String>,
    /// An extra attribution line under the stats, if any — e.g. a clip's
    /// "Clipped by X" (the [`author`](Self::author) is the streamer; this is who
    /// made the clip). `None` for sources without one (e.g. YouTube videos).
    pub byline: Option<String>,
}

/// A link a provider claimed, carrying the provider-specific id it extracted so
/// [`LinkPreviewProvider::fetch`] doesn't re-parse the URL.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PreviewTarget {
    /// The extracted id (e.g. a YouTube video id).
    pub id: String,
    pub kind: PreviewKind,
}

/// One source of link previews. Implementors match a URL to their kind and fetch
/// its metadata. Mirrors `EmoteProvider`: adding a source = implement this and
/// push it into the registered provider list.
#[async_trait]
pub trait LinkPreviewProvider: Send + Sync {
    /// A short name for logs (e.g. "youtube").
    fn name(&self) -> &'static str;

    /// Whether `url` is this provider's kind of link, and if so the target to
    /// fetch. `None` = not mine.
    fn match_url(&self, url: &str) -> Option<PreviewTarget>;

    /// Fetches the preview for a target this provider claimed.
    async fn fetch(&self, target: &PreviewTarget) -> anyhow::Result<LinkPreview>;
}

/// How long a resolved preview stays fresh before it's re-fetched.
const TTL: Duration = Duration::from_secs(30 * 60);
/// A failed fetch is cached (negative) only briefly so a transient error can
/// retry soon, but a burst of the same bad link doesn't hammer the network.
const NEGATIVE_TTL: Duration = Duration::from_secs(60);

const MAX_ENTRIES: usize = 512;
const MAX_PENDING: usize = 64;
const PENDING_TTL: Duration = Duration::from_secs(60);

type CacheKey = (usize, PreviewTarget);

enum EntryState {
    Pending(u64),
    Ready(Arc<LinkPreview>),
    Failed,
}

struct Entry {
    state: EntryState,
    created: Instant,
    last_used: Instant,
}

impl Entry {
    fn fresh(&self, now: Instant) -> bool {
        let ttl = match self.state {
            EntryState::Pending(_) => PENDING_TTL,
            EntryState::Ready(_) => TTL,
            EntryState::Failed => NEGATIVE_TTL,
        };
        now.duration_since(self.created) < ttl
    }

    fn lookup(&self) -> Lookup {
        match &self.state {
            EntryState::Pending(_) => Lookup::Pending,
            EntryState::Ready(p) => Lookup::Ready(p.clone()),
            EntryState::Failed => Lookup::Failed,
        }
    }
}

pub enum Lookup {
    Ready(Arc<LinkPreview>),
    Unsupported,
    Pending,
    Failed,
}

#[derive(Default)]
struct CacheState {
    entries: HashMap<CacheKey, Entry>,
    generation: u64,
}

/// Bounded cache keyed by the provider's canonical target, not tracking URLs.
pub struct PreviewCache {
    providers: Vec<Box<dyn LinkPreviewProvider>>,
    state: Mutex<CacheState>,
}

/// Identifies one fetch so a late completion cannot overwrite a newer attempt.
pub struct PreviewRequest {
    key: CacheKey,
    generation: u64,
}

pub struct LookupResult {
    pub state: Lookup,
    pub to_fetch: Option<PreviewRequest>,
}

impl PreviewCache {
    pub fn new(providers: Vec<Box<dyn LinkPreviewProvider>>) -> Self {
        Self {
            providers,
            state: Mutex::new(CacheState::default()),
        }
    }

    fn match_provider(&self, url: &str) -> Option<CacheKey> {
        self.providers
            .iter()
            .enumerate()
            .find_map(|(i, p)| p.match_url(url).map(|target| (i, target)))
    }

    pub fn lookup(&self, url: &str) -> LookupResult {
        let Some(key) = self.match_provider(url) else {
            return LookupResult {
                state: Lookup::Unsupported,
                to_fetch: None,
            };
        };
        let now = Instant::now();
        let mut cache = self.state.lock().unwrap();
        cache.entries.retain(|_, entry| entry.fresh(now));
        if let Some(entry) = cache.entries.get_mut(&key) {
            entry.last_used = now;
            return LookupResult {
                state: entry.lookup(),
                to_fetch: None,
            };
        }
        let pending = cache
            .entries
            .values()
            .filter(|entry| matches!(entry.state, EntryState::Pending(_)))
            .count();
        if pending >= MAX_PENDING {
            return LookupResult {
                state: Lookup::Failed,
                to_fetch: None,
            };
        }
        if cache.entries.len() >= MAX_ENTRIES {
            let oldest = cache
                .entries
                .iter()
                .filter(|(_, entry)| !matches!(entry.state, EntryState::Pending(_)))
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone());
            if let Some(key) = oldest {
                cache.entries.remove(&key);
            }
        }
        cache.generation += 1;
        let generation = cache.generation;
        cache.entries.insert(
            key.clone(),
            Entry {
                state: EntryState::Pending(generation),
                created: now,
                last_used: now,
            },
        );
        LookupResult {
            state: Lookup::Pending,
            to_fetch: Some(PreviewRequest { key, generation }),
        }
    }

    pub fn lookup_peek(&self, url: &str) -> Lookup {
        let Some(key) = self.match_provider(url) else {
            return Lookup::Unsupported;
        };
        let cache = self.state.lock().unwrap();
        match cache.entries.get(&key) {
            Some(entry) if entry.fresh(Instant::now()) => entry.lookup(),
            _ => Lookup::Failed,
        }
    }

    pub async fn fetch(&self, request: &PreviewRequest) -> anyhow::Result<LinkPreview> {
        self.providers[request.key.0].fetch(&request.key.1).await
    }

    pub fn store(&self, request: PreviewRequest, result: anyhow::Result<LinkPreview>) {
        let mut cache = self.state.lock().unwrap();
        let Some(entry) = cache.entries.get_mut(&request.key) else {
            return;
        };
        if !matches!(entry.state, EntryState::Pending(g) if g == request.generation) {
            return;
        }
        entry.state = match result {
            Ok(preview) => EntryState::Ready(Arc::new(preview)),
            Err(err) => {
                tracing::debug!("link preview fetch failed: {err:#}");
                EntryState::Failed
            }
        };
        entry.created = Instant::now();
        entry.last_used = entry.created;
    }

    pub fn is_supported(&self, url: &str) -> bool {
        self.match_provider(url).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeProvider;

    #[async_trait]
    impl LinkPreviewProvider for FakeProvider {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn match_url(&self, url: &str) -> Option<PreviewTarget> {
            url.strip_prefix("fake://").map(|id| PreviewTarget {
                id: id.split('?').next().unwrap().to_string(),
                kind: PreviewKind::Video,
            })
        }
        async fn fetch(&self, target: &PreviewTarget) -> anyhow::Result<LinkPreview> {
            Ok(LinkPreview {
                kind: PreviewKind::Video,
                title: format!("title {}", target.id),
                author: "chan".into(),
                thumbnail_url: None,
                stats: None,
                byline: None,
            })
        }
    }

    fn cache() -> PreviewCache {
        PreviewCache::new(vec![Box::new(FakeProvider)])
    }

    #[test]
    fn unsupported_url_is_unsupported() {
        let c = cache();
        assert!(matches!(
            c.lookup("https://x.com").state,
            Lookup::Unsupported
        ));
        assert!(!c.is_supported("https://x.com"));
        assert!(c.is_supported("fake://abc"));
    }

    #[test]
    fn first_lookup_starts_fetch_second_is_pending() {
        let c = cache();
        let first = c.lookup("fake://abc");
        assert!(matches!(first.state, Lookup::Pending));
        assert!(
            first.to_fetch.is_some(),
            "first lookup should start a fetch"
        );

        // A second lookup while in flight must NOT start another fetch (dedupe).
        let second = c.lookup("fake://abc");
        assert!(matches!(second.state, Lookup::Pending));
        assert!(
            second.to_fetch.is_none(),
            "in-flight fetch must not restart"
        );
    }

    #[test]
    fn store_then_lookup_is_ready() {
        let c = cache();
        let request = c.lookup("fake://abc").to_fetch.unwrap();
        c.store(
            request,
            Ok(LinkPreview {
                kind: PreviewKind::Video,
                title: "hello".into(),
                author: "chan".into(),
                thumbnail_url: None,
                stats: Some("5 views".into()),
                byline: None,
            }),
        );
        match c.lookup("fake://abc").state {
            Lookup::Ready(p) => {
                assert_eq!(p.title, "hello");
                assert_eq!(p.stats.as_deref(), Some("5 views"));
            }
            _ => panic!("expected Ready after store"),
        }
    }

    #[test]
    fn failed_fetch_is_negative_cached() {
        let c = cache();
        let request = c.lookup("fake://abc").to_fetch.unwrap();
        c.store(request, Err(anyhow::anyhow!("boom")));
        let after = c.lookup("fake://abc");
        assert!(matches!(after.state, Lookup::Failed));
        assert!(
            after.to_fetch.is_none(),
            "negative cache must not immediately retry"
        );
    }
    #[test]
    fn canonical_targets_share_pending_and_ready_entries() {
        let c = cache();
        let request = c.lookup("fake://clip?tracking=one").to_fetch.unwrap();
        assert!(c.lookup("fake://clip?tracking=two").to_fetch.is_none());
        c.store(request, Err(anyhow::anyhow!("missing")));
        assert!(matches!(
            c.lookup("fake://clip?tracking=three").state,
            Lookup::Failed
        ));
        assert_eq!(c.state.lock().unwrap().entries.len(), 1);
    }

    #[test]
    fn expires_unvisited_entries_and_rejects_late_completions() {
        let c = cache();
        let old = c.lookup("fake://old").to_fetch.unwrap();
        c.state
            .lock()
            .unwrap()
            .entries
            .get_mut(&old.key)
            .unwrap()
            .created = Instant::now() - PENDING_TTL;
        let new = c.lookup("fake://old").to_fetch.unwrap();
        c.store(old, Err(anyhow::anyhow!("late completion")));
        assert!(matches!(c.lookup_peek("fake://old"), Lookup::Pending));
        c.store(new, Err(anyhow::anyhow!("new completion")));
        c.state
            .lock()
            .unwrap()
            .entries
            .values_mut()
            .next()
            .unwrap()
            .created = Instant::now() - NEGATIVE_TTL;
        c.lookup("fake://another");
        assert_eq!(c.state.lock().unwrap().entries.len(), 1);
    }

    #[test]
    fn bounds_completed_entries_and_pending_work() {
        let c = cache();
        for i in 0..MAX_ENTRIES + 20 {
            let request = c.lookup(&format!("fake://{i}")).to_fetch.unwrap();
            c.store(request, Err(anyhow::anyhow!("missing")));
        }
        assert_eq!(c.state.lock().unwrap().entries.len(), MAX_ENTRIES);
        for i in 0..MAX_PENDING {
            assert!(c.lookup(&format!("fake://pending{i}")).to_fetch.is_some());
        }
        assert!(c.lookup("fake://overflow").to_fetch.is_none());
        assert_eq!(c.state.lock().unwrap().entries.len(), MAX_ENTRIES);
    }

    #[test]
    fn capacity_eviction_keeps_recently_used_entries() {
        let c = cache();
        for i in 0..MAX_ENTRIES {
            let request = c.lookup(&format!("fake://{i}")).to_fetch.unwrap();
            c.store(request, Err(anyhow::anyhow!("missing")));
        }
        c.lookup("fake://0");
        c.lookup("fake://new");
        let cache = c.state.lock().unwrap();
        assert!(cache
            .entries
            .contains_key(&c.match_provider("fake://0").unwrap()));
        assert!(!cache
            .entries
            .contains_key(&c.match_provider("fake://1").unwrap()));
    }
}
