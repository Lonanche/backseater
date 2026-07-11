//! Resolving a tab's YouTube source string into a currently-live video id.
//!
//! The source can be many things a user might paste: an `@handle` (with or
//! without the `@`), a channel URL
//! (`/channel/UC…`, `/c/name`, `/user/name`, `/@handle`), or a direct video
//! reference (`watch?v=…`, `youtu.be/…`, `/live/…`, `/shorts/…`, `/embed/…`, or a
//! bare 11-char id). Direct video refs resolve instantly ([`extract_video_id`]).
//! For a channel we resolve its `UC…` id (from the channel page HTML) and then
//! probe the *current* live video via the embed live-stream endpoint
//! ([`probe_live_video_id`]) — so a tab set to a channel auto-follows whatever
//! it's streaming right now, and yields `None` when the channel is offline.

use once_cell::sync::Lazy;
use regex::Regex;

use crate::api::client;

/// An 11-char YouTube video id (`[A-Za-z0-9_-]{11}`).
static VIDEO_ID_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[A-Za-z0-9_-]{11}$").unwrap());
/// A `UC…` channel id (24 chars).
static CHANNEL_ID_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^UC[A-Za-z0-9_-]{22}$").unwrap());

/// Patterns that pull a canonical watch video id out of channel/embed page HTML,
/// tried in order (first match wins).
static EMBED_LIVE_RES: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r#"<link rel="canonical" href="https://www\.youtube\.com/watch\?v=([A-Za-z0-9_-]{11})""#,
        r#"<meta property="og:url" content="https://www\.youtube\.com/watch\?v=([A-Za-z0-9_-]{11})""#,
        r#""canonicalBaseUrl":"\\?/watch\\?\?v=([A-Za-z0-9_-]{11})""#,
        r#"https://www\.youtube\.com/embed/([A-Za-z0-9_-]{11})"#,
        r#"\\?/embed\\?/([A-Za-z0-9_-]{11})"#,
    ]
    .iter()
    .map(|p| Regex::new(p).unwrap())
    .collect()
});

/// Patterns that pull the owner channel id (`UC…`) out of a channel page's HTML.
static CHANNEL_ID_HTML_RES: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r#""channelId":"(UC[A-Za-z0-9_-]{22})""#,
        r#""externalId":"(UC[A-Za-z0-9_-]{22})""#,
        r#"<meta itemprop="identifier" content="(UC[A-Za-z0-9_-]{22})""#,
    ]
    .iter()
    .map(|p| Regex::new(p).unwrap())
    .collect()
});

/// Whether `s` looks like a bare video id.
pub fn is_video_id(s: &str) -> bool {
    VIDEO_ID_RE.is_match(s)
}

/// Whether `s` looks like a `UC…` channel id.
pub fn is_channel_id(s: &str) -> bool {
    CHANNEL_ID_RE.is_match(s)
}

/// Pulls a video id out of a direct reference (bare id or one of the URL forms).
/// Returns `None` for a channel/handle source (which needs a live probe instead).
pub fn extract_video_id(source: &str) -> Option<String> {
    let s = source.trim();
    if is_video_id(s) {
        return Some(s.to_string());
    }

    // Normalize to something url::Url-free parsing can slice. We avoid a URL crate
    // dep and just look for the well-known markers.
    let lower = s.to_ascii_lowercase();

    // watch?v=<id> (anywhere in the string).
    if let Some(id) = capture_after(s, "v=") {
        return Some(id);
    }
    // youtu.be/<id>
    if let Some(rest) = split_after(&lower, s, "youtu.be/") {
        return first_id_segment(rest);
    }
    // /live/<id>, /shorts/<id>, /embed/<id>
    for marker in ["/live/", "/shorts/", "/embed/"] {
        if let Some(rest) = split_after(&lower, s, marker) {
            if let Some(id) = first_id_segment(rest) {
                return Some(id);
            }
        }
    }
    None
}

/// The base channel-page URL a non-video source denotes (e.g.
/// `https://www.youtube.com/@handle` or `…/channel/UC…`). Resolution appends
/// `/live` to it to find the current live video, and scrapes it for the `UC…` id
/// as an embed-probe fallback; [`crate::streams`] resolves it to a channel id
/// for the last-stream browse.
pub(crate) fn channel_page_url(source: &str) -> Option<String> {
    let s = source.trim();
    if s.is_empty() {
        return None;
    }
    // A bare @handle → the canonical handle page (the `@`-less form falls
    // through to the handle fallback at the bottom).
    if let Some(handle) = s.strip_prefix('@') {
        return Some(format!("https://www.youtube.com/@{handle}"));
    }
    // A bare `UC…` channel id → the channel page.
    if is_channel_id(s) {
        return Some(format!("https://www.youtube.com/channel/{s}"));
    }
    // Any youtube.com URL (/@handle, /channel/UC…, /c/name, /user/name) → use it as
    // the base, stripped of a trailing `/live`, `/streams`, or query so we can
    // append `/live` cleanly.
    let lower = s.to_ascii_lowercase();
    if lower.contains("youtube.com/") {
        let mut url = if lower.starts_with("http") {
            s.to_string()
        } else {
            format!("https://{s}")
        };
        // Drop a query string and any trailing /live|/streams|/featured segment.
        if let Some(q) = url.find('?') {
            url.truncate(q);
        }
        let url = url.trim_end_matches('/');
        for suffix in ["/live", "/streams", "/featured", "/videos"] {
            if let Some(base) = url.strip_suffix(suffix) {
                return Some(base.to_string());
            }
        }
        return Some(url.to_string());
    }
    // A bare channel name without the `@` (e.g. `TheBurntPeanut`) — treat it as
    // a handle. Anything URL-shaped was handled above; a stray full URL to some
    // other site (has a `/`) is rejected rather than mangled into a handle.
    if !s.contains('/') && !s.contains(char::is_whitespace) {
        return Some(format!("https://www.youtube.com/@{s}"));
    }
    None
}

/// Resolves `source` to a currently-live video id, or `None` when it's not a
/// live stream (channel offline, or a fixed video that isn't live). Network
/// errors also surface as `None` (the caller retries).
pub async fn resolve_live_video_id(source: &str) -> Option<String> {
    if let Some(id) = extract_video_id(source) {
        return Some(id);
    }
    let base = channel_page_url(source)?;

    // Primary path: the channel's `/live` page redirects to (and carries a
    // canonical link for) the current live broadcast when one is running. This is
    // far more reliable than the embed endpoint (which often omits the id).
    let live_url = format!("{base}/live");
    if let Some(html) = get_text(&live_url).await {
        // The `/live` page redirects to the channel's most recent live-type video
        // even when nothing is live *now* — an ended broadcast or a scheduled
        // upcoming one. Its `liveBroadcastDetails.isLiveNow`/`isLiveNow` flag is
        // `false` in those cases (and the page carries `LIVE_STREAM_OFFLINE`), so
        // only accept the scraped id when the page says it's live right now.
        // Otherwise the tab would report a channel online off its last VOD.
        if is_live_now(&html) {
            if let Some(id) = first_match(&html, &EMBED_LIVE_RES).filter(|id| is_video_id(id)) {
                return Some(id);
            }
        }
        // Fallback within the same fetch: scrape the `UC…` id and probe the embed
        // endpoint (covers channels whose /live page didn't carry the canonical).
        if let Some(channel_id) =
            first_match(&html, &CHANNEL_ID_HTML_RES).filter(|id| is_channel_id(id))
        {
            if let Some(id) = probe_live_video_id(&channel_id).await {
                return Some(id);
            }
        }
    }
    None
}

/// Whether a `/live` (or embed) page describes a broadcast that is live *right
/// now*. YouTube serves the same page for a live, ended, or upcoming broadcast,
/// distinguished by `isLiveNow` (the `isLive`/`isLiveContent` flags are `true`
/// for *any* live-type video, so they can't be used). We require an affirmative
/// `isLiveNow`; its absence or a `LIVE_STREAM_OFFLINE` status means not live.
fn is_live_now(html: &str) -> bool {
    if html.contains(r#""isLiveNow":true"#) {
        return true;
    }
    // No affirmative live-now flag → treat an explicit offline/false as not live.
    // (An embed-probe page may omit the flag entirely; `probe_live_video_id`'s
    // canonical scrape covers that, but the primary `/live` page always carries it.)
    false
}

/// Probes the channel's current live video via the embed live-stream endpoint.
/// A secondary path — the `/live` canonical is preferred. `None` if not live.
async fn probe_live_video_id(channel_id: &str) -> Option<String> {
    let url = format!("https://www.youtube.com/embed/live_stream?channel={channel_id}");
    let html = get_text(&url).await?;
    first_match(&html, &EMBED_LIVE_RES).filter(|id| is_video_id(id))
}

/// GETs a URL as text with a channel referer, following redirects. `None` on error.
async fn get_text(url: &str) -> Option<String> {
    match client()
        .get(url)
        .header(reqwest::header::REFERER, "https://www.youtube.com/")
        .send()
        .await
    {
        Ok(resp) => resp.text().await.ok(),
        Err(err) => {
            tracing::debug!("youtube GET {url} failed: {err:#}");
            None
        }
    }
}

/// The first capture group of the first matching regex in `res`.
fn first_match(text: &str, res: &[Regex]) -> Option<String> {
    res.iter().find_map(|re| {
        re.captures(text)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
    })
}

/// The value after a `key=` marker (e.g. `v=`) up to the next `&`/`/`, if it's a
/// valid video id. Case-sensitive on the value (ids are), marker matched literally.
fn capture_after(s: &str, key: &str) -> Option<String> {
    let idx = s.find(key)? + key.len();
    let rest = &s[idx..];
    let seg: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    is_video_id(&seg).then_some(seg)
}

/// Locates `marker` (matched case-insensitively via `lower`) in the original `s`
/// and returns the original-cased remainder after it.
fn split_after<'a>(lower: &str, s: &'a str, marker: &str) -> Option<&'a str> {
    lower.find(marker).map(|i| &s[i + marker.len()..])
}

/// The first path segment of `rest` (up to `/` or `?`), if it's a valid video id.
fn first_id_segment(rest: &str) -> Option<String> {
    let seg = rest.split(['/', '?', '&']).next()?;
    is_video_id(seg).then(|| seg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_video_id() {
        assert_eq!(
            extract_video_id("dQw4w9WgXcQ").as_deref(),
            Some("dQw4w9WgXcQ")
        );
    }

    #[test]
    fn watch_url() {
        assert_eq!(
            extract_video_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=5").as_deref(),
            Some("dQw4w9WgXcQ")
        );
    }

    #[test]
    fn youtu_be_short_url() {
        assert_eq!(
            extract_video_id("https://youtu.be/dQw4w9WgXcQ?si=abc").as_deref(),
            Some("dQw4w9WgXcQ")
        );
    }

    #[test]
    fn live_and_embed_paths() {
        assert_eq!(
            extract_video_id("youtube.com/live/dQw4w9WgXcQ").as_deref(),
            Some("dQw4w9WgXcQ")
        );
        assert_eq!(
            extract_video_id("https://www.youtube.com/embed/dQw4w9WgXcQ").as_deref(),
            Some("dQw4w9WgXcQ")
        );
    }

    #[test]
    fn handle_is_not_a_video_id() {
        assert_eq!(extract_video_id("@somechannel"), None);
        assert_eq!(
            extract_video_id("https://www.youtube.com/@somechannel"),
            None
        );
    }

    #[test]
    fn channel_page_url_normalizes() {
        // Bare handle and channel id → canonical channel pages.
        assert_eq!(
            channel_page_url("@handle").as_deref(),
            Some("https://www.youtube.com/@handle")
        );
        assert_eq!(
            channel_page_url("UCuAXFkgsw1L7xaCfnd5JJOw").as_deref(),
            Some("https://www.youtube.com/channel/UCuAXFkgsw1L7xaCfnd5JJOw")
        );
        // A URL already pointing at /live is stripped back to the base (so we can
        // re-append /live cleanly) — not doubled.
        assert_eq!(
            channel_page_url("https://www.youtube.com/@TheBurntPeanut/live").as_deref(),
            Some("https://www.youtube.com/@TheBurntPeanut")
        );
        assert_eq!(
            channel_page_url("https://www.youtube.com/channel/UCuAXFkgsw1L7xaCfnd5JJOw/streams")
                .as_deref(),
            Some("https://www.youtube.com/channel/UCuAXFkgsw1L7xaCfnd5JJOw")
        );
        // A query string is dropped.
        assert_eq!(
            channel_page_url("https://www.youtube.com/c/SomeName?foo=bar").as_deref(),
            Some("https://www.youtube.com/c/SomeName")
        );
        // A scheme-less youtube.com URL gets https:// (kept as typed, no www added).
        assert_eq!(
            channel_page_url("youtube.com/@handle").as_deref(),
            Some("https://youtube.com/@handle")
        );
        // A bare channel name without the `@` is treated as a handle.
        assert_eq!(
            channel_page_url("TheBurntPeanut").as_deref(),
            Some("https://www.youtube.com/@TheBurntPeanut")
        );
        // But path-ish strings that aren't YouTube URLs are rejected, not mangled.
        assert_eq!(channel_page_url("example.com/foo"), None);
    }

    #[test]
    fn is_live_now_gate() {
        // A live page: affirmative isLiveNow.
        assert!(is_live_now(r#"…"liveBroadcastDetails":{"isLiveNow":true}…"#));
        // An ended broadcast the /live page redirects to.
        assert!(!is_live_now(
            r#"…"liveBroadcastDetails":{"isLiveNow":false}…"status":"LIVE_STREAM_OFFLINE"…"#
        ));
        // An upcoming/scheduled stream (Posty's actual case) — isLive/isLiveContent
        // are true but isLiveNow is false, so it must not count as live.
        assert!(!is_live_now(
            r#"…"isLive":true…"isLiveNow":false…"isLiveContent":true…"isUpcoming":true…"#
        ));
        // No live-now flag at all → not live.
        assert!(!is_live_now(r#"…some unrelated page…"#));
    }

    #[test]
    fn channel_id_validation() {
        assert!(is_channel_id("UCuAXFkgsw1L7xaCfnd5JJOw"));
        assert!(!is_channel_id("UCtoo_short"));
        assert!(!is_video_id("UCuAXFkgsw1L7xaCfnd5JJOw"));
        assert!(is_video_id("dQw4w9WgXcQ"));
    }
}
