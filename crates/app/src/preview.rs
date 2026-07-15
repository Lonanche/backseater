//! App-side glue for link previews: the process-wide [`PreviewCache`] (with the
//! registered providers) plus the driver that runs a fetch on the tokio runtime
//! and delivers the result to the gpui side over a smol channel.
//!
//! The cache/data live in `bks-preview`; the providers live in their platform
//! crates. This module wires them together and owns the one shared instance,
//! mirroring `image_cache::LruImageCache::shared`.

use std::sync::Arc;
use std::sync::OnceLock;

use bks_preview::{LinkPreview, Lookup, PreviewCache};
use bks_twitch::TwitchClipPreviewProvider;
use bks_youtube::YoutubePreviewProvider;
use tokio::runtime::Handle;

/// The one app-wide preview cache. Providers are registered on first use.
/// **Adding a provider (e.g. Twitch clips) = one more entry here.**
static CACHE: OnceLock<Arc<PreviewCache>> = OnceLock::new();

/// The shared preview cache, initializing it (and its providers) on first call.
pub fn cache() -> &'static Arc<PreviewCache> {
    CACHE.get_or_init(|| {
        Arc::new(PreviewCache::new(vec![
            Box::new(YoutubePreviewProvider::new()),
            Box::new(TwitchClipPreviewProvider::new()),
        ]))
    })
}

/// Whether any provider can preview `url` (cheap; no fetch). Used by the hover
/// wiring to decide whether to even arm a preview.
pub fn is_supported(url: &str) -> bool {
    cache().is_supported(url)
}

/// The outcome of asking for a URL's preview, for the view to render.
pub enum PreviewState {
    /// Ready to show.
    Ready(Arc<LinkPreview>),
    /// A fetch is in flight; show nothing yet (a repaint comes when it lands).
    Loading,
    /// Not previewable, or recently failed — show nothing.
    None,
}

/// Reads `url`'s current preview state **without** starting a fetch — for the
/// render path, which must not spawn work (the hover [`lookup`] already armed any
/// needed fetch). A supported-but-not-yet-ready URL reads as `Loading`.
pub fn peek(url: &str) -> PreviewState {
    match cache().lookup_peek(url) {
        Some(preview) => PreviewState::Ready(preview),
        None if cache().is_supported(url) => PreviewState::Loading,
        None => PreviewState::None,
    }
}

/// Looks up `url`'s preview. If a fetch needs starting, spawns it on `rt` and
/// delivers the URL over `notify` when it completes (so the view can repaint and
/// re-query the now-`Ready` cache). Returns the current state to render.
pub fn lookup(url: &str, rt: &Handle, notify: smol::channel::Sender<String>) -> PreviewState {
    let result = cache().lookup(url);
    if let Some((target, provider_ix)) = result.to_fetch {
        let url = url.to_string();
        rt.spawn(async move {
            let outcome = cache().fetch(&target, provider_ix).await;
            cache().store(&url, outcome);
            // Wake the view; it re-queries the cache (now Ready/Failed).
            let _ = notify.send(url).await;
        });
    }
    match result.state {
        Lookup::Ready(preview) => PreviewState::Ready(preview),
        Lookup::Pending => PreviewState::Loading,
        Lookup::Unsupported | Lookup::Failed => PreviewState::None,
    }
}
