//! Twitch clip link-preview provider: resolves a clip link to its title /
//! curator / view count / thumbnail via an anonymous `gql.twitch.tv` query (the
//! same public web Client-Id path the badge fetch uses — no auth, no Helix).
//!
//! Registered alongside the YouTube provider in the app's preview list; adding it
//! needs no other change (the [`LinkPreviewProvider`] seam).

use async_trait::async_trait;
use bks_preview::{LinkPreview, LinkPreviewProvider, PreviewKind, PreviewTarget};
use serde::Deserialize;

use crate::http::{client, GQL_URL, WEB_CLIENT_ID};

/// The Twitch clip preview provider.
#[derive(Default)]
pub struct TwitchClipPreviewProvider;

impl TwitchClipPreviewProvider {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl LinkPreviewProvider for TwitchClipPreviewProvider {
    fn name(&self) -> &'static str {
        "twitch-clip"
    }

    fn match_url(&self, url: &str) -> Option<PreviewTarget> {
        clip_slug(url).map(|slug| PreviewTarget {
            id: slug,
            kind: PreviewKind::Clip,
        })
    }

    async fn fetch(&self, target: &PreviewTarget) -> anyhow::Result<LinkPreview> {
        // A raw GraphQL query (not a persisted-query hash, which drifts) for the
        // public clip fields. `gql.twitch.tv` accepts these with the web Client-Id.
        let query = r#"query($slug:ID!){clip(slug:$slug){
            title viewCount
            curator{displayName}
            broadcaster{displayName}
            thumbnailURL(width:480,height:272)
        }}"#;
        let body = serde_json::json!({
            "query": query,
            "variables": { "slug": target.id },
        });

        let resp: GqlResponse = client()
            .post(GQL_URL)
            .header("Client-Id", WEB_CLIENT_ID)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let clip = resp
            .data
            .clip
            .ok_or_else(|| anyhow::anyhow!("clip not found"))?;
        let title = clip.title.unwrap_or_default();
        if title.is_empty() {
            anyhow::bail!("clip has no title");
        }
        // The clip's broadcaster is the channel; the curator is who clipped it.
        let author = clip
            .broadcaster
            .and_then(|b| b.display_name)
            .unwrap_or_default();
        let stats = clip
            .view_count
            .map(|n| format!("{} views", bks_core::format_count_compact(n)));
        let byline = clip
            .curator
            .and_then(|c| c.display_name)
            .filter(|s| !s.is_empty())
            .map(|name| format!("Clipped by {name}"));

        Ok(LinkPreview {
            kind: PreviewKind::Clip,
            title,
            author,
            thumbnail_url: clip.thumbnail_url.filter(|s| !s.is_empty()),
            stats,
            byline,
        })
    }
}

/// Extracts a clip slug from the Twitch clip URL forms, or `None` if not a clip
/// link. Handles: `clips.twitch.tv/<slug>`, `(www|m).twitch.tv/<chan>/clip/<slug>`,
/// and `(www|m).twitch.tv/clip/<slug>`. A slug is the first path segment after the
/// marker, stripped of any query string.
fn clip_slug(url: &str) -> Option<String> {
    let url = bks_preview::web_url(url)?;
    let segments: Vec<_> = url.path().trim_matches('/').split('/').collect();
    let slug = match (url.host_str()?, segments.as_slice()) {
        ("clips.twitch.tv", [slug]) => *slug,
        ("twitch.tv" | "www.twitch.tv" | "m.twitch.tv", ["clip", slug])
        | ("twitch.tv" | "www.twitch.tv" | "m.twitch.tv", [_, "clip", slug]) => *slug,
        _ => return None,
    };
    (!slug.is_empty()
        && slug
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)))
    .then(|| slug.to_string())
}

#[derive(Deserialize)]
struct GqlResponse {
    data: GqlData,
}

#[derive(Deserialize)]
struct GqlData {
    clip: Option<GqlClip>,
}

#[derive(Deserialize)]
struct GqlClip {
    title: Option<String>,
    #[serde(rename = "viewCount")]
    view_count: Option<u64>,
    broadcaster: Option<GqlChannel>,
    /// Who made the clip (shown as "Clipped by X").
    curator: Option<GqlChannel>,
    #[serde(rename = "thumbnailURL")]
    thumbnail_url: Option<String>,
}

#[derive(Deserialize)]
struct GqlChannel {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> TwitchClipPreviewProvider {
        TwitchClipPreviewProvider::new()
    }

    #[test]
    fn rejects_spoofed_hosts_and_non_web_links() {
        let provider = TwitchClipPreviewProvider::new();
        for url in [
            "https://example.org/clips.twitch.tv/RealClipSlug",
            "https://clips.twitch.tv.evil.example/RealClipSlug",
            "https://evil.example/?next=https://twitch.tv/user/clip/RealClipSlug",
            "https://clips.twitch.tv@evil.example/RealClipSlug",
            "ftp://clips.twitch.tv/RealClipSlug",
            "https://clips.twitch.tv:8443/RealClipSlug",
        ] {
            assert!(
                provider.match_url(url).is_none(),
                "unexpected preview for {url}"
            );
        }
    }

    #[test]
    fn matches_clip_url_forms() {
        let p = provider();
        assert_eq!(
            p.match_url("https://clips.twitch.tv/AwkwardHelplessSalamanderSwiftRage")
                .map(|t| t.id)
                .as_deref(),
            Some("AwkwardHelplessSalamanderSwiftRage")
        );
        assert_eq!(
            p.match_url("https://www.twitch.tv/somestreamer/clip/GoodClip-Slug_123?filter=clips")
                .map(|t| t.id)
                .as_deref(),
            Some("GoodClip-Slug_123")
        );
        assert_eq!(
            p.match_url("https://m.twitch.tv/clip/AnotherSlug")
                .map(|t| t.id)
                .as_deref(),
            Some("AnotherSlug")
        );
    }

    #[test]
    fn does_not_match_non_clip_twitch_or_other_hosts() {
        let p = provider();
        // A plain channel page is not a clip.
        assert!(p.match_url("https://www.twitch.tv/somestreamer").is_none());
        // A VOD is not a clip.
        assert!(p
            .match_url("https://www.twitch.tv/videos/123456789")
            .is_none());
        // A `/clip/` path on some other host must not be claimed.
        assert!(p.match_url("https://example.com/clip/whatever").is_none());
        // A YouTube link is the other provider's job.
        assert!(p.match_url("https://youtu.be/dQw4w9WgXcQ").is_none());
    }

    #[test]
    fn parses_clip_response() {
        let json = r#"{"data":{"clip":{
            "title":"insane play",
            "viewCount":34000,
            "broadcaster":{"displayName":"SomeStreamer"},
            "curator":{"displayName":"Clipper"},
            "thumbnailURL":"https://clips-media.twitch.tv/x-preview-480x272.jpg"
        }}}"#;
        let resp: GqlResponse = serde_json::from_str(json).unwrap();
        let clip = resp.data.clip.unwrap();
        assert_eq!(clip.title.as_deref(), Some("insane play"));
        assert_eq!(clip.view_count, Some(34000));
        assert_eq!(
            clip.broadcaster.and_then(|b| b.display_name).as_deref(),
            Some("SomeStreamer")
        );
        assert_eq!(
            clip.curator.and_then(|c| c.display_name).as_deref(),
            Some("Clipper")
        );
    }
}
