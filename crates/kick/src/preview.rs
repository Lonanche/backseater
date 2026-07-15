//! Kick clip link-preview provider: resolves a Kick clip link to its title /
//! channel / views / thumbnail via the Cloudflare-fronted clips endpoint (in
//! process, through the shared emulated `wreq` client like the other Kick reads).
//!
//! Registered alongside the YouTube + Twitch providers in the app's preview list;
//! adding it needs no other change (the [`LinkPreviewProvider`] seam).

use async_trait::async_trait;
use bks_preview::{LinkPreview, LinkPreviewProvider, PreviewKind, PreviewTarget};
use serde::Deserialize;

use crate::api::kick_get;

/// The Kick clip preview provider.
#[derive(Default)]
pub struct KickClipPreviewProvider;

impl KickClipPreviewProvider {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl LinkPreviewProvider for KickClipPreviewProvider {
    fn name(&self) -> &'static str {
        "kick-clip"
    }

    fn match_url(&self, url: &str) -> Option<PreviewTarget> {
        clip_id(url).map(|id| PreviewTarget {
            id,
            kind: PreviewKind::Clip,
        })
    }

    async fn fetch(&self, target: &PreviewTarget) -> anyhow::Result<LinkPreview> {
        // The Cloudflare-fronted clip endpoint (via the emulated client, like the
        // rest of Kick's reads). Parsed tolerantly — Kick's field names vary.
        let resp = kick_get(format!(
            "https://kick.com/api/v2/clips/{}",
            bks_core::encode_url_component(&target.id)
        ))
        .await
        .map_err(|e| anyhow::anyhow!("requesting kick clip {}: {e}", target.id))?;
        if !resp.status().is_success() {
            anyhow::bail!("kick clip {} returned {}", target.id, resp.status());
        }
        let body: RawResponse = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("parsing kick clip {}: {e}", target.id))?;
        let clip = body.clip.ok_or_else(|| anyhow::anyhow!("clip not found"))?;

        // Compute the borrowing bits before moving `title` out of `clip`.
        let author = clip
            .channel
            .as_ref()
            .map(|c| c.best_name())
            .filter(|s| !s.is_empty())
            .unwrap_or_default();
        let stats = clip
            .view_count()
            .map(|n| format!("{} views", bks_core::format_count_compact(n)));
        let thumbnail_url = clip.thumbnail();
        let byline = clip
            .creator
            .as_ref()
            .map(|c| c.best_name())
            .filter(|s| !s.is_empty())
            .map(|name| format!("Clipped by {name}"));
        let title = clip.title.unwrap_or_default();
        if title.is_empty() {
            anyhow::bail!("kick clip has no title");
        }

        Ok(LinkPreview {
            kind: PreviewKind::Clip,
            title,
            author,
            thumbnail_url,
            stats,
            byline,
        })
    }
}

/// Extracts a Kick clip id from the clip URL forms, or `None` if not one. Handles
/// `kick.com/<channel>/clips/<id>` and `kick.com/<channel>?clip=<id>` (the two
/// forms the site produces). A Kick clip id looks like `clip_XXXXXXXX`.
fn clip_id(url: &str) -> Option<String> {
    let lower = url.to_ascii_lowercase();
    if !lower.contains("kick.com/") {
        return None;
    }
    // `.../clips/<id>` path form.
    if let Some(rest) = split_after(&lower, url, "/clips/") {
        if let Some(id) = first_segment(rest) {
            return Some(id);
        }
    }
    // `?clip=<id>` / `&clip=<id>` query form.
    if let Some(rest) = split_after(&lower, url, "clip=") {
        if let Some(id) = first_segment(rest) {
            return Some(id);
        }
    }
    None
}

/// Locates `marker` (case-insensitively via `lower`) in `url` and returns the
/// original-cased remainder after it.
fn split_after<'a>(lower: &str, url: &'a str, marker: &str) -> Option<&'a str> {
    lower.find(marker).map(|i| &url[i + marker.len()..])
}

/// The first path/query segment of `rest` (up to `/`, `?`, `&`, or `#`), if
/// non-empty.
fn first_segment(rest: &str) -> Option<String> {
    let seg = rest.split(['/', '?', '&', '#']).next()?;
    (!seg.is_empty()).then(|| seg.to_string())
}

// ---- Raw Kick clip JSON (tolerant; only the fields we render) ---------------

#[derive(Deserialize)]
struct RawResponse {
    clip: Option<RawClip>,
}

#[derive(Deserialize)]
struct RawClip {
    #[serde(default)]
    title: Option<String>,
    // Kick's clip body carries *both* `views` and `view_count` (same value), so
    // they must be separate fields — a serde `alias` treats them as one and errors
    // on the duplicate. We read whichever is present ([`RawClip::view_count`]).
    #[serde(default)]
    views: Option<u64>,
    #[serde(default)]
    view_count: Option<u64>,
    /// Thumbnail as a bare URL string (the live shape).
    #[serde(default)]
    thumbnail_url: Option<String>,
    /// Thumbnail as an object (`{src|url}`) — an alternate shape Kick has used;
    /// a distinct field (not an alias) so a body carrying both can't panic.
    #[serde(default, deserialize_with = "thumb_obj")]
    thumbnail: Option<String>,
    #[serde(default)]
    channel: Option<RawChannel>,
    /// Who made the clip (shown as "Clipped by X"); same shape as `channel`.
    #[serde(default)]
    creator: Option<RawChannel>,
}

impl RawClip {
    /// The view count from whichever field Kick populated.
    fn view_count(&self) -> Option<u64> {
        self.views.or(self.view_count)
    }

    /// The thumbnail URL from whichever shape Kick returned (string or object).
    fn thumbnail(&self) -> Option<String> {
        self.thumbnail_url
            .clone()
            .or_else(|| self.thumbnail.clone())
            .filter(|s| !s.is_empty())
    }
}

#[derive(Deserialize)]
struct RawChannel {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    slug: Option<String>,
}

impl RawChannel {
    /// The channel's display name, preferring `username` over the slug.
    fn best_name(&self) -> String {
        self.username
            .clone()
            .or_else(|| self.slug.clone())
            .unwrap_or_default()
    }
}

/// Extracts a URL from a `thumbnail` object (`{src|url}`); tolerates a bare string
/// too. `None` for anything else. (The common `thumbnail_url` string field is read
/// directly; this only covers the object-shaped `thumbnail`.)
fn thumb_obj<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(match v {
        serde_json::Value::String(s) => Some(s),
        serde_json::Value::Object(map) => map
            .get("src")
            .or_else(|| map.get("url"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> KickClipPreviewProvider {
        KickClipPreviewProvider::new()
    }

    #[test]
    fn matches_clip_url_forms() {
        let p = provider();
        assert_eq!(
            p.match_url("https://kick.com/somestreamer/clips/clip_01ABCDEF")
                .map(|t| t.id)
                .as_deref(),
            Some("clip_01ABCDEF")
        );
        assert_eq!(
            p.match_url("https://kick.com/somestreamer?clip=clip_01ABCDEF")
                .map(|t| t.id)
                .as_deref(),
            Some("clip_01ABCDEF")
        );
    }

    #[test]
    fn does_not_match_non_clip_or_other_hosts() {
        let p = provider();
        // A plain channel page is not a clip.
        assert!(p.match_url("https://kick.com/somestreamer").is_none());
        // A `/clips/` path on another host isn't Kick's.
        assert!(p.match_url("https://example.com/clips/whatever").is_none());
        // Other platforms' links are their providers' job.
        assert!(p.match_url("https://clips.twitch.tv/SomeSlug").is_none());
    }

    #[test]
    fn parses_clip_response_string_thumbnail() {
        let json = r#"{"clip":{
            "id":"clip_01ABCDEF",
            "title":"huge play",
            "views":12500,
            "thumbnail_url":"https://clips.kick.com/x.jpg",
            "channel":{"username":"SomeStreamer","slug":"somestreamer"}
        }}"#;
        let body: RawResponse = serde_json::from_str(json).unwrap();
        let clip = body.clip.unwrap();
        assert_eq!(clip.title.as_deref(), Some("huge play"));
        assert_eq!(clip.view_count(), Some(12500));
        assert_eq!(clip.channel.as_ref().unwrap().best_name(), "SomeStreamer");
        assert_eq!(clip.thumbnail().as_deref(), Some("https://clips.kick.com/x.jpg"));
    }

    #[test]
    fn parses_real_kick_clip_body() {
        // The exact live response shape (from a real clip).
        let json = r#"{"clip":{"id":"clip_01KA5BPF1J3VBTV4Z9SM2P90HN","livestream_id":"83619243","category_id":"13","channel_id":83713955,"user_id":58669552,"title":"aja jaaaa","clip_url":"https://clips.kick.com/x/playlist.m3u8","thumbnail_url":"https://clips.kick.com/x/thumbnail.webp","privacy":"public","likes":0,"liked":false,"views":189,"duration":34,"view_count":189,"category":{"id":13,"name":"Rust"},"creator":{"id":58669552,"username":"deployval","slug":"deployval","profile_picture":null},"channel":{"id":83713955,"username":"trausi","slug":"trausi","profile_picture":"https://x/p.webp"}}}"#;
        let body: RawResponse = serde_json::from_str(json).expect("should parse real body");
        let clip = body.clip.unwrap();
        assert_eq!(clip.title.as_deref(), Some("aja jaaaa"));
        // Both `views` and `view_count` are present — must not error as a duplicate.
        assert_eq!(clip.view_count(), Some(189));
        assert_eq!(clip.channel.as_ref().unwrap().best_name(), "trausi");
        assert_eq!(
            clip.thumbnail().as_deref(),
            Some("https://clips.kick.com/x/thumbnail.webp")
        );
    }

    #[test]
    fn parses_clip_response_object_thumbnail_and_view_count_alias() {
        // The alternate shape: `thumbnail` object + `view_count`.
        let json = r#"{"clip":{
            "title":"nice",
            "view_count":900,
            "thumbnail":{"src":"https://clips.kick.com/y.jpg"},
            "channel":{"slug":"onlyslug"}
        }}"#;
        let body: RawResponse = serde_json::from_str(json).unwrap();
        let clip = body.clip.unwrap();
        // `view_count` present, `views` absent → still resolves.
        assert_eq!(clip.view_count(), Some(900));
        assert_eq!(clip.thumbnail().as_deref(), Some("https://clips.kick.com/y.jpg"));
        // Falls back to the slug when there's no username.
        assert_eq!(clip.channel.as_ref().unwrap().best_name(), "onlyslug");
    }
}
