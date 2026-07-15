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
//! process-wide [`PreviewCache`] that dedupes fetches by URL (a link posted five
//! times, hovered twice, and shown inline all share one fetch). The *driving* of
//! the async fetch — spawning it on a tokio runtime and storing the result — is
//! the app's job (it owns the runtime, like the image cache), so this crate
//! stays dependency-light and testable.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;

/// What kind of thing a link points at — lets the UI label/style the card and
/// grows as providers are added.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
}

/// A link a provider claimed, carrying the provider-specific id it extracted so
/// [`LinkPreviewProvider::fetch`] doesn't re-parse the URL.
#[derive(Clone, Debug, PartialEq, Eq)]
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

/// The cache state for one URL.
#[derive(Clone)]
enum Entry {
    /// A fetch is running; don't start another.
    Pending,
    /// Resolved successfully at this time.
    Ready(Arc<LinkPreview>, Instant),
    /// Failed at this time (negative cache).
    Failed(Instant),
}

/// The outcome of asking the cache for a URL's preview.
pub enum Lookup {
    /// Ready to render.
    Ready(Arc<LinkPreview>),
    /// No provider claims this URL — never previewable.
    Unsupported,
    /// A fetch is in flight (or was just started by this call — see the returned
    /// flag); nothing to show yet.
    Pending,
    /// Recently failed and still within the negative-cache window; nothing to show.
    Failed,
}

/// A process-wide, URL-keyed preview cache with in-flight dedupe and a short
/// negative cache. Providers are registered once; the app drives fetches.
///
/// Flow the app follows on hover: call [`PreviewCache::lookup`]; if it returns
/// `Pending` *and* `started` is true, spawn the returned target's fetch on the
/// runtime and call [`PreviewCache::store`] with the result, then repaint.
pub struct PreviewCache {
    providers: Vec<Box<dyn LinkPreviewProvider>>,
    entries: Mutex<HashMap<String, Entry>>,
}

/// What [`PreviewCache::lookup`] hands back — the current state plus, when a
/// fetch needs starting, the target to fetch and which provider owns it.
pub struct LookupResult {
    pub state: Lookup,
    /// `Some` only when the caller should *start* a fetch (state is `Pending`
    /// and no fetch was already running): the target + the provider index.
    pub to_fetch: Option<(PreviewTarget, usize)>,
}

impl PreviewCache {
    pub fn new(providers: Vec<Box<dyn LinkPreviewProvider>>) -> Self {
        Self {
            providers,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Which provider (if any) claims `url`, and the target to fetch.
    fn match_provider(&self, url: &str) -> Option<(PreviewTarget, usize)> {
        self.providers
            .iter()
            .enumerate()
            .find_map(|(i, p)| p.match_url(url).map(|t| (t, i)))
    }

    /// Looks up `url`'s preview state, marking it in-flight (and telling the
    /// caller to start the fetch) on a fresh, uncached, supported URL.
    pub fn lookup(&self, url: &str) -> LookupResult {
        let Some((target, provider_ix)) = self.match_provider(url) else {
            return LookupResult {
                state: Lookup::Unsupported,
                to_fetch: None,
            };
        };

        let mut entries = self.entries.lock().unwrap();
        match entries.get(url) {
            Some(Entry::Ready(preview, at)) if at.elapsed() < TTL => LookupResult {
                state: Lookup::Ready(preview.clone()),
                to_fetch: None,
            },
            Some(Entry::Failed(at)) if at.elapsed() < NEGATIVE_TTL => LookupResult {
                state: Lookup::Failed,
                to_fetch: None,
            },
            Some(Entry::Pending) => LookupResult {
                state: Lookup::Pending,
                to_fetch: None,
            },
            // Absent, or a stale Ready/Failed → (re)start the fetch.
            _ => {
                entries.insert(url.to_string(), Entry::Pending);
                LookupResult {
                    state: Lookup::Pending,
                    to_fetch: Some((target, provider_ix)),
                }
            }
        }
    }

    /// Reads a fresh, resolved preview for `url` **without** starting or marking
    /// a fetch — for the render path, which must not mutate cache state. Returns
    /// `None` for an absent/pending/failed/stale entry (the caller distinguishes
    /// loading vs unsupported via [`is_supported`](Self::is_supported)).
    pub fn lookup_peek(&self, url: &str) -> Option<Arc<LinkPreview>> {
        let entries = self.entries.lock().unwrap();
        match entries.get(url) {
            Some(Entry::Ready(preview, at)) if at.elapsed() < TTL => Some(preview.clone()),
            _ => None,
        }
    }

    /// Runs a claimed target's fetch through its provider. The app awaits this on
    /// its runtime, then calls [`store`](Self::store) with the outcome.
    pub async fn fetch(&self, target: &PreviewTarget, provider_ix: usize) -> anyhow::Result<LinkPreview> {
        self.providers[provider_ix].fetch(target).await
    }

    /// Records a fetch outcome for `url` (resolved or failed).
    pub fn store(&self, url: &str, result: anyhow::Result<LinkPreview>) {
        let mut entries = self.entries.lock().unwrap();
        match result {
            Ok(preview) => {
                entries.insert(url.to_string(), Entry::Ready(Arc::new(preview), Instant::now()));
            }
            Err(err) => {
                tracing::debug!("link preview fetch for {url} failed: {err:#}");
                entries.insert(url.to_string(), Entry::Failed(Instant::now()));
            }
        }
    }

    /// Whether any registered provider claims `url` (cheap; no fetch). Lets the
    /// UI decide *whether to even arm* a hover preview without touching the cache.
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
                id: id.to_string(),
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
            })
        }
    }

    fn cache() -> PreviewCache {
        PreviewCache::new(vec![Box::new(FakeProvider)])
    }

    #[test]
    fn unsupported_url_is_unsupported() {
        let c = cache();
        assert!(matches!(c.lookup("https://x.com").state, Lookup::Unsupported));
        assert!(!c.is_supported("https://x.com"));
        assert!(c.is_supported("fake://abc"));
    }

    #[test]
    fn first_lookup_starts_fetch_second_is_pending() {
        let c = cache();
        let first = c.lookup("fake://abc");
        assert!(matches!(first.state, Lookup::Pending));
        assert!(first.to_fetch.is_some(), "first lookup should start a fetch");

        // A second lookup while in flight must NOT start another fetch (dedupe).
        let second = c.lookup("fake://abc");
        assert!(matches!(second.state, Lookup::Pending));
        assert!(second.to_fetch.is_none(), "in-flight fetch must not restart");
    }

    #[test]
    fn store_then_lookup_is_ready() {
        let c = cache();
        c.lookup("fake://abc"); // mark pending
        c.store(
            "fake://abc",
            Ok(LinkPreview {
                kind: PreviewKind::Video,
                title: "hello".into(),
                author: "chan".into(),
                thumbnail_url: None,
                stats: Some("5 views".into()),
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
        c.lookup("fake://abc");
        c.store("fake://abc", Err(anyhow::anyhow!("boom")));
        let after = c.lookup("fake://abc");
        assert!(matches!(after.state, Lookup::Failed));
        assert!(after.to_fetch.is_none(), "negative cache must not immediately retry");
    }
}
