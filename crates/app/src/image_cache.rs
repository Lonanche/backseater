//! A self-evicting, disk-backed image cache for the chat log.
//!
//! Two problems this solves:
//!
//! 1. **Unbounded RAM.** gpui's *global* asset cache never evicts — every image URL
//!    ever drawn stays decoded (BGRA, all frames) in RAM for the process lifetime,
//!    which over a long multi-channel session grows unbounded (animated emotes
//!    dominate). gpui's bundled `RetainAllImageCache` also never evicts despite its
//!    name. So this [`ImageCache`] records, **inside `load`**, the instant each
//!    image was last accessed — and since gpui calls `load` for every image it
//!    actually renders each frame, "last accessed" is exactly "last on screen", with
//!    no per-image-kind bookkeeping in the UI. A periodic [`sweep`](LruImageCache::sweep)
//!    frees the decoded frames of anything not drawn within a lifetime window.
//!    On-screen images are re-stamped every
//!    frame so they're never evicted; off-screen ones re-load when scrolled back.
//!
//! 2. **Re-download on every miss.** gpui's image loader fetches over HTTP on every
//!    cache miss (no disk cache) and re-decodes — so a short eviction lifetime makes
//!    emotes that scroll off and back churn the network and load slowly, and nothing
//!    survives a restart. So we keep a persistent on-disk byte cache keyed by
//!    URL hash and read disk-first: [`load_cached`] writes each
//!    fetched image to `<cache>/backseater-cache/images/<hash>` and, on a later miss,
//!    decodes straight from those bytes — no network. So a first load this session
//!    of a previously-seen emote, and any reload after eviction, is a fast local
//!    read; only a truly first-ever sighting hits the network.
//!
//! Remote images are decoded by [`decode_frames`] (GIF/WebP-aware, mirroring gpui's
//! own loader) with one extra step: frames taller than the largest size we ever
//! render are **downscaled at decode time** ([`max_decode_height`]). Decoded BGRA
//! frames cost `w×h×4×frames` in heap *and* GPU atlas for as long as the image is
//! resident — a 500×500 46-frame Kick GIF is 44 MB decoded for a ≤64px render;
//! capped it's under 1 MB. (Freeing decoded frames after GPU upload
//! isn't expressible in gpui's public API — the atlas is keyed by
//! `RenderImage.id`, whose frames live behind the same `Arc` you must keep to
//! paint — so the decode-time cap is the memory lever.)
//!
//! The cache is **app-wide** (one instance shared by every tab via [`shared`]), so an
//! emote loaded in one tab is reused everywhere without re-fetching or re-decoding.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures::FutureExt;
use gpui::{
    hash, App, AppContext, Asset, Entity, ImageCache, ImageCacheError, ImageCacheItem,
    ImgResourceLoader, RenderImage, Resource, Window,
};

/// Max concurrent image downloads. Opening the picker renders hundreds of emotes
/// at once; without a cap every uncached one fires an HTTP GET simultaneously and
/// the CDN throttles them to a crawl (observed: a 44 KB emote taking 10 s because
/// it was queued behind a flood). Browsers cap ~6 connections per host; we cap
/// total in-flight downloads so a burst drains a few-at-a-time at full speed
/// instead of all-at-once at a trickle. Acquired in [`fetch_bytes`].
const MAX_CONCURRENT_FETCHES: usize = 32;

/// The global download-concurrency limiter (see [`MAX_CONCURRENT_FETCHES`]).
fn fetch_semaphore() -> &'static async_lock::Semaphore {
    static SEM: std::sync::OnceLock<async_lock::Semaphore> = std::sync::OnceLock::new();
    SEM.get_or_init(|| async_lock::Semaphore::new(MAX_CONCURRENT_FETCHES))
}

/// Max concurrent image *decodes*. Decoding an animated emote unpacks every frame
/// to BGRA (sync work on a background-pool thread); opening the picker makes
/// dozens of emotes visible at once, and uncapped that queues them all
/// simultaneously — saturating every core for seconds (the "open the picker" CPU
/// spike). A small cap does the same total work while leaving cores free for the
/// UI; emotes fill in progressively instead of all-at-once-after-a-freeze.
/// Pinned to 1: fast-scrolling the picker with always-animated cells decodes
/// every scrolled-past emote's full animation, and this bounds that churn to a
/// single core (~9% on a 12-thread CPU vs ~18% at 2, ~30% at 4). Posters
/// (first-frame thumbnails, no permit needed) keep the grid *looking* instant
/// while the full decodes trickle in behind.
const MAX_CONCURRENT_DECODES: usize = 1;

/// The global decode-concurrency limiter (see [`MAX_CONCURRENT_DECODES`]).
fn decode_semaphore() -> &'static async_lock::Semaphore {
    static SEM: std::sync::OnceLock<async_lock::Semaphore> = std::sync::OnceLock::new();
    SEM.get_or_init(|| async_lock::Semaphore::new(MAX_CONCURRENT_DECODES))
}

/// How long to wait before retrying an image whose load failed, so a permanently
/// broken image (undecodable format, dead CDN) doesn't re-fetch every single frame
/// while still on screen, but a transient failure recovers on its own shortly.
const FAILED_RETRY_COOLDOWN: Duration = Duration::from_secs(10);

/// One cache slot for a URL: either a (loading/loaded) gpui image, or a failure
/// awaiting retry after a cooldown.
enum Slot {
    /// A gpui image cache item (loading, or loaded OK).
    Image(ImageCacheItem),
    /// The last load failed at this instant; retried once the cooldown elapses.
    Failed(Instant),
}

/// One cached slot plus when it was last accessed (drawn).
struct Entry {
    slot: Slot,
    last_used: Instant,
}

/// An [`ImageCache`] that evicts images not drawn within a lifetime window and
/// backs its fetches with a persistent on-disk byte cache.
pub struct LruImageCache {
    entries: HashMap<u64, Entry>,
    /// Images are dropped (decoded frames + GPU textures) this long after they
    /// were last drawn.
    lifetime: Duration,
}

/// Holds the single app-wide image cache as a gpui global, so every tab (and the
/// picker, usercards, ...) share one cache — an emote loaded anywhere is reused
/// everywhere without re-fetching or re-decoding.
struct GlobalImageCache(Entity<LruImageCache>);

impl gpui::Global for GlobalImageCache {}

impl LruImageCache {
    /// The app-wide shared cache, created on first use. All callers get the same
    /// entity, so its eviction/disk cache is shared across the whole app. The first
    /// call starts the single periodic eviction sweep (every `sweep_interval`).
    pub fn shared(lifetime: Duration, sweep_interval: Duration, cx: &mut App) -> Entity<Self> {
        if !cx.has_global::<GlobalImageCache>() {
            let cache = Self::new(lifetime, cx);
            cx.set_global(GlobalImageCache(cache.clone()));
            // One startup prune of the on-disk byte cache (age-based GC).
            crate::bridge::runtime().spawn_blocking(prune_disk_cache);
            let weak = cache.downgrade();
            cx.spawn(async move |cx| loop {
                cx.background_executor().timer(sweep_interval).await;
                let alive = cx.update(|cx| match weak.upgrade() {
                    Some(cache) => {
                        cache.update(cx, |c, cx| c.sweep(cx));
                        true
                    }
                    None => false,
                });
                if !alive {
                    break; // cache gone (app shutting down)
                }
            })
            .detach();
        }
        cx.global::<GlobalImageCache>().0.clone()
    }

    /// Returns the app-wide cache if [`shared`](Self::shared) has created it (it
    /// has, by the time anything renders). The [`AnimatedImage`](crate::animated_img)
    /// element loads through this.
    pub fn try_shared(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalImageCache>().map(|g| g.0.clone())
    }

    /// Creates the cache as an entity (the form gpui's `image_cache(..)` element
    /// wants). Frees all decoded frames when released. Prefer [`shared`](Self::shared).
    fn new(lifetime: Duration, cx: &mut App) -> Entity<Self> {
        let e = cx.new(|_cx| LruImageCache {
            entries: HashMap::new(),
            lifetime,
        });
        cx.observe_release(&e, |cache, cx| {
            for (_, entry) in std::mem::take(&mut cache.entries) {
                drop_slot(entry.slot, cx);
            }
        })
        .detach();
        e
    }

    /// Frees the decoded frames (and GPU textures, from every window) of each image
    /// not drawn within [`self.lifetime`]. Driven by the timer in [`shared`](Self::shared).
    /// Images on screen were accessed within the last frame, so they're never swept.
    fn sweep(&mut self, cx: &mut App) {
        let now = Instant::now();
        let stale: Vec<u64> = self
            .entries
            .iter()
            .filter(|(_, e)| now.duration_since(e.last_used) > self.lifetime)
            .map(|(&hash, _)| hash)
            .collect();
        for hash in stale {
            if let Some(entry) = self.entries.remove(&hash) {
                drop_slot(entry.slot, cx);
            }
        }
        // At `debug` level, summarize the resident decoded footprint (w×h×4 per
        // frame, the BGRA heap cost) so a regression — e.g. an oversized image
        // slipping past the decode downscale — is visible without per-frame cost
        // in normal runs.
        if tracing::enabled!(tracing::Level::DEBUG) {
            let (mut loaded, mut decoded_bytes) = (0u64, 0u64);
            for entry in self.entries.values_mut() {
                if let Slot::Image(item) = &mut entry.slot {
                    if let Some(Ok(img)) = item.get() {
                        loaded += 1;
                        for frame in 0..img.frame_count() {
                            let size = img.size(frame);
                            decoded_bytes += size.width.0 as u64 * size.height.0 as u64 * 4;
                        }
                    }
                }
            }
            tracing::debug!(
                "image cache: {} entries, {loaded} loaded, decoded heap {} MB",
                self.entries.len(),
                decoded_bytes / (1024 * 1024)
            );
        }
    }
}

/// Frees a slot's decoded image (from every window), if it holds one that finished
/// loading. A failed slot has nothing to free.
fn drop_slot(slot: Slot, cx: &mut App) {
    if let Slot::Image(mut item) = slot {
        if let Some(Ok(image)) = item.get() {
            cx.drop_image(image, None);
        }
    }
}

/// The on-disk cache file for `url` (`<cache>/backseater-cache/images/<hex hash>`), or
/// `None` if the cache dir can't be created.
fn cache_file(url: &str) -> Option<PathBuf> {
    let dir = bks_auth::store::image_cache_dir().ok()?;
    Some(dir.join(format!("{:016x}", hash(&url))))
}

/// Pseudo-scheme marking a **poster** load: fetch/disk-cache the real URL's bytes
/// exactly like a normal load (same disk file — the compressed bytes are shared),
/// but decode **only the first frame**. Decoding an animated emote's full frame
/// set is what dominated CPU while fast-scrolling the picker (the thumbnails are
/// static, yet every scrolled-past emote paid the whole-animation decode); a
/// poster is ~1/frame-count of that work. The picker renders static cells from
/// `poster://<url>` and switches to the real URL (a separate cache slot, full
/// decode) only for the hovered, animating cell.
pub(crate) const POSTER_PREFIX: &str = "poster://";

/// Frames taller than this (device px) are downscaled at decode time, preserving
/// aspect ratio. The cap is the largest size we ever render an image from this
/// cache — the emote popup / tooltip preview (≤72 logical px) — times the display
/// scale. 7TV/BTTV/FFZ emotes are already fetched at 1x/2x (≤64px tall) and pass
/// through untouched; this exists for sources with no small variant (Kick's
/// `fullsize`-only emote CDN, legacy 4x files in the disk cache), whose decoded
/// BGRA frames (`w×h×4×frames`, held in heap + GPU atlas while resident) would
/// otherwise be tens of MB *each*.
fn max_decode_height() -> u32 {
    80 * bks_core::preferred_scale() as u32
}

/// Downscales `frame` to [`max_decode_height`] if taller (keeping aspect), then
/// swizzles RGBA→BGRA like gpui's own loader. Every decoded frame passes through
/// here exactly once.
fn finish_frame(frame: image::Frame) -> image::Frame {
    let cap = max_decode_height();
    let delay = frame.delay();
    let mut buf = frame.into_buffer();
    let (w, h) = buf.dimensions();
    if h > cap {
        let new_w = ((w as u64 * cap as u64) / h as u64).max(1) as u32;
        buf = image::imageops::resize(&buf, new_w, cap, image::imageops::FilterType::Triangle);
    }
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2); // RGBA -> BGRA
    }
    image::Frame::from_parts(buf, 0, 0, delay)
}

/// Shorthand for wrapping an [`image::ImageError`] the way gpui's loader does.
fn image_err(err: image::ImageError) -> ImageCacheError {
    ImageCacheError::Image(Arc::new(err))
}

/// Decodes `bytes` into a [`RenderImage`] — animated GIF/WebP yield all frames,
/// everything else one. Mirrors gpui's `ImageAssetLoader` (skip individually
/// corrupt animation frames, error only when *all* fail), plus the
/// [`finish_frame`] downscale. `label` is only for log context.
fn decode_frames(bytes: &[u8], label: &str) -> Result<Arc<RenderImage>, ImageCacheError> {
    use image::codecs::gif::GifDecoder;
    use image::codecs::webp::WebPDecoder;
    use image::{AnimationDecoder, ImageFormat};
    use std::io::Cursor;

    let format = image::guess_format(bytes).map_err(image_err)?;
    let frames: Vec<image::Frame> = match format {
        ImageFormat::Gif => {
            let decoder = GifDecoder::new(Cursor::new(bytes)).map_err(image_err)?;
            collect_animation_frames(decoder.into_frames(), label)?
        }
        ImageFormat::WebP => {
            let mut decoder = WebPDecoder::new(Cursor::new(bytes)).map_err(image_err)?;
            if decoder.has_animation() {
                let _ = decoder.set_background_color(image::Rgba([0, 0, 0, 0]));
                collect_animation_frames(decoder.into_frames(), label)?
            } else {
                let rgba = image::DynamicImage::from_decoder(decoder)
                    .map_err(image_err)?
                    .into_rgba8();
                vec![finish_frame(image::Frame::new(rgba))]
            }
        }
        _ => {
            let rgba = image::load_from_memory_with_format(bytes, format)
                .map_err(image_err)?
                .into_rgba8();
            vec![finish_frame(image::Frame::new(rgba))]
        }
    };
    Ok(Arc::new(RenderImage::new(frames)))
}

/// Collects an animation's frames, skipping individually corrupt ones (some CDN
/// GIFs have a bad trailing frame); errors only if none decode.
fn collect_animation_frames(
    frames: image::Frames<'_>,
    label: &str,
) -> Result<Vec<image::Frame>, ImageCacheError> {
    let mut out = Vec::new();
    for frame in frames {
        match frame {
            Ok(frame) => out.push(finish_frame(frame)),
            Err(err) => {
                tracing::debug!("image: skipping corrupt animation frame in {label}: {err}")
            }
        }
    }
    if out.is_empty() {
        return Err(anyhow::anyhow!("animation could not be decoded: all frames failed").into());
    }
    Ok(out)
}

/// Decodes only the first frame of `bytes` into a single-frame [`RenderImage`]
/// (downscaled + swizzled like every frame). `image::load_from_memory` returns
/// frame 0 for animated GIF/WebP, which is exactly a poster.
fn decode_poster(bytes: &[u8]) -> Result<Arc<RenderImage>, image::ImageError> {
    let rgba = image::load_from_memory(bytes)?.into_rgba8();
    let frame = finish_frame(image::Frame::new(rgba));
    Ok(Arc::new(RenderImage::new(vec![frame])))
}

/// Resolves a `poster://` load: bytes from the shared disk cache (or the network,
/// written through for the full-decode path to reuse), then a first-frame-only
/// decode. No decode-semaphore permit — a poster decode is ~1ms, and queueing it
/// behind full animation decodes would defeat its purpose (instant scroll fill).
/// A poster-decode failure falls back to the full decode of the same bytes.
async fn load_poster(
    url: String,
    cache: Option<PathBuf>,
) -> Result<Arc<RenderImage>, ImageCacheError> {
    // Disk bytes when present + readable; a read failure falls back to the
    // network like a plain miss (an unreadable file must not pin the poster
    // to a failure loop).
    let disk_bytes = match &cache {
        Some(path) if path.exists() => {
            touch(path);
            std::fs::read(path)
                .inspect_err(|err| {
                    tracing::warn!("image: poster disk read failed for {url} ({err:#})")
                })
                .ok()
        }
        _ => None,
    };
    let bytes = match disk_bytes {
        Some(bytes) => bytes,
        None => {
            let bytes = fetch_bytes(&url).await?;
            if let Some(path) = &cache {
                if let Err(err) = std::fs::write(path, &bytes) {
                    tracing::warn!("image: disk write failed for {url} → {path:?} ({err:#})");
                }
            }
            bytes
        }
    };
    match decode_poster(&bytes) {
        Ok(image) => Ok(image),
        Err(err) => {
            tracing::warn!("image: poster decode failed for {url} ({err}); using full decode");
            decode_frames(&bytes, &url)
        }
    }
}

/// How long an unused file survives in the on-disk cache. Without a prune the
/// directory grows forever (every emote ever seen, across all sessions); mtime
/// is refreshed on each disk hit ([`touch`]), so this is time since last *use*.
const DISK_CACHE_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// Best-effort mtime bump so the prune sees this file as recently used.
fn touch(path: &std::path::Path) {
    if let Ok(f) = std::fs::OpenOptions::new().append(true).open(path) {
        let _ = f.set_modified(std::time::SystemTime::now());
    }
}

/// Deletes cache files not used (mtime) within [`DISK_CACHE_MAX_AGE`]. Run once
/// at startup on the tokio blocking pool — it's pure filesystem work.
fn prune_disk_cache() {
    let Ok(dir) = bks_auth::store::image_cache_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    let mut removed = 0u32;
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|mtime| now.duration_since(mtime).ok())
            .is_some_and(|age| age > DISK_CACHE_MAX_AGE);
        if stale && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        tracing::info!("image cache: pruned {removed} unused disk files");
    }
}

/// Resolves a URL into a [`RenderImage`], reading from the on-disk byte cache when
/// present and only hitting the network on a disk miss (writing the bytes through
/// for next time). Decoding is ours ([`decode_frames`]: gpui's logic + the
/// downscale cap), runs on the background executor this future is spawned on, and
/// holds a decode-semaphore permit so a picker burst can't saturate every core.
async fn load_cached(
    url: String,
    cache: Option<PathBuf>,
) -> Result<Arc<RenderImage>, ImageCacheError> {
    if let Some(path) = &cache {
        // Disk hit: decode from the file, no network. Touch its mtime so the
        // age-based prune (see [`prune_disk_cache`]) treats it as recently used.
        if path.exists() {
            touch(path);
            match std::fs::read(path) {
                Ok(bytes) => {
                    let _permit = decode_semaphore().acquire().await;
                    let result = decode_frames(&bytes, &url);
                    if let Err(err) = &result {
                        // A decode error on an existing cache file means the file is
                        // corrupt (truncated/partial write). Delete it so the next
                        // attempt re-fetches.
                        tracing::warn!(
                            "image: disk-cache decode failed for {url} ({err:#}); deleting {path:?}"
                        );
                        let _ = std::fs::remove_file(path);
                    }
                    return result;
                }
                // An unreadable file (locked, permissions) would otherwise fail
                // every retry forever — drop it and fall through to a fresh fetch.
                Err(err) => {
                    tracing::warn!(
                        "image: disk-cache read failed for {url} ({err:#}); deleting {path:?} and re-fetching"
                    );
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
    // Disk miss: fetch, write through (best-effort), decode the fetched bytes.
    let bytes = fetch_bytes(&url)
        .await
        .inspect_err(|err| tracing::warn!("image: fetch failed for {url} ({err:#})"))?;
    if let Some(path) = &cache {
        if let Err(err) = std::fs::write(path, &bytes) {
            tracing::warn!("image: disk write failed for {url} → {path:?} ({err:#})");
        }
    }
    let _permit = decode_semaphore().acquire().await;
    decode_frames(&bytes, &url)
        .inspect_err(|err| tracing::warn!("image: decode failed for {url} ({err:#})"))
}

/// One shared `reqwest::Client` for image downloads, with connection pooling +
/// HTTP/2. Driven on [`crate::bridge::runtime`] (multi-threaded), NOT gpui's
/// `reqwest_client` — that one pins all requests to a single tokio worker thread,
/// which throttled emote downloads to ~10–50 KB/s under a picker burst.
fn image_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent("backseater/0.1")
            // Timeouts, since reqwest's default has none: a hung download would
            // otherwise hold one of the MAX_CONCURRENT_FETCHES permits forever —
            // 32 hung fetches and all image loading stops for the session.
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("building image http client")
    })
}

/// Fetches the raw bytes of `url` on our own multi-threaded runtime + pooled
/// client, holding a concurrency permit (see [`MAX_CONCURRENT_FETCHES`]). The work
/// runs on `bridge::runtime()`; the result is handed back to the gpui-executor
/// caller over a oneshot channel.
async fn fetch_bytes(url: &str) -> anyhow::Result<Vec<u8>> {
    let _permit = fetch_semaphore().acquire().await;
    let url = url.to_string();
    let (tx, rx) = futures::channel::oneshot::channel();
    crate::bridge::runtime().spawn(async move {
        let result = async {
            let resp = image_client().get(&url).send().await?;
            anyhow::ensure!(resp.status().is_success(), "http {}", resp.status());
            Ok::<_, anyhow::Error>(resp.bytes().await?.to_vec())
        }
        .await;
        let _ = tx.send(result);
    });
    rx.await.context("image download task dropped")?
}

impl ImageCache for LruImageCache {
    fn load(
        &mut self,
        resource: &Resource,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        let hash = hash(resource);
        let now = Instant::now();

        if let Some(entry) = self.entries.get_mut(&hash) {
            entry.last_used = now; // accessed this frame → on screen → keep
            match &mut entry.slot {
                Slot::Image(item) => match item.get() {
                    // A failed load must NOT stay cached: a transient network blip or
                    // a decode race would otherwise leave the image blank forever (it
                    // stays `Loaded(Err)` and every re-draw returns the same error).
                    // Mark it failed-now so we retry after a cooldown (not every frame).
                    Some(Err(err)) => {
                        tracing::warn!("image: load failed (will retry after cooldown): {err:#}");
                        entry.slot = Slot::Failed(now);
                    }
                    // Still loading, or loaded OK → return as-is.
                    other => return other,
                },
                // Previously failed: retry once the cooldown elapses, else stay blank
                // (without re-spawning a fetch every frame).
                Slot::Failed(at) => {
                    if now.duration_since(*at) < FAILED_RETRY_COOLDOWN {
                        return None;
                    }
                    self.entries.remove(&hash); // cooldown over → fall through to retry
                }
            }
        }

        // Miss: spawn the load and store a Loading entry (so concurrent draws of the
        // same image share one fetch/decode). The view is notified when it finishes.
        // Only remote URLs go through the disk cache + our decoder; embedded/path
        // resources (bundled Kick badges, the platform icon) decode via gpui's own
        // loader — they're small statics, identical either way.
        let task = match resource {
            // Poster: the picker's static thumbnails — shared bytes, first-frame
            // decode (see [`POSTER_PREFIX`]). Its own slot (the hash covers the
            // prefix), so the hovered cell's full animation is a separate entry.
            Resource::Uri(uri) if uri.starts_with(POSTER_PREFIX) => {
                let url = uri[POSTER_PREFIX.len()..].to_string();
                let file = cache_file(&url);
                let fut = load_poster(url, file);
                cx.background_executor().spawn(fut).shared()
            }
            Resource::Uri(uri) => {
                let url = uri.to_string();
                let file = cache_file(&url);
                let fut = load_cached(url, file);
                cx.background_executor().spawn(fut).shared()
            }
            other => {
                let fut = <ImgResourceLoader as Asset>::load(other.clone(), cx);
                cx.background_executor().spawn(fut).shared()
            }
        };
        self.entries.insert(
            hash,
            Entry {
                slot: Slot::Image(ImageCacheItem::Loading(task.clone())),
                last_used: now,
            },
        );

        let entity = window.current_view();
        window
            .spawn(cx, async move |cx| {
                _ = task.await;
                cx.on_next_frame(move |_, cx| cx.notify(entity));
            })
            .detach();

        None
    }
}

/// Loads `url` through the app-wide cache — the load path of the
/// [`AnimatedImage`](crate::animated_img) element, equivalent to what an `img()`
/// scoped to this cache would do (same resource parsing, same slot key, same LRU
/// access stamp). `None` while loading or during a failure cooldown.
pub fn load_image(
    url: &gpui::SharedString,
    window: &mut Window,
    cx: &mut App,
) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
    let cache = LruImageCache::try_shared(cx)?;
    // Parse the way `img()` does: http(s) URLs become `Resource::Uri` (fetched +
    // disk-cached), anything else `Resource::Embedded` (the bundled Kick badge
    // paths like "kick/badges/moderator.webp", served by `assets.rs`).
    let gpui::ImageSource::Resource(resource) = gpui::ImageSource::from(url.clone()) else {
        return None;
    };
    cache.update(cx, |cache, cx| cache.load(&resource, window, &mut *cx))
}
