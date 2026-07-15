//! YouTube link-preview provider: resolves a `youtube.com`/`youtu.be` video link
//! to its title / channel / view count / thumbnail via one InnerTube `player`
//! call — keyless, no quota, the same path the live-chat reads use.
//!
//! This is the first [`LinkPreviewProvider`] impl. It reuses [`extract_video_id`]
//! (so it claims exactly the video URL shapes the connector already understands)
//! and a process-wide cached [`InnertubeContext`] (bootstrapped once, refreshed
//! if a fetch fails — the context can expire).

use std::sync::Arc;

use async_trait::async_trait;
use bks_preview::{LinkPreview, LinkPreviewProvider, PreviewKind, PreviewTarget};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::api::{InnertubeContext, PLAYER_URL};
use crate::resolve::extract_video_id;

/// The YouTube video preview provider. Register one in the app's provider list.
#[derive(Default)]
pub struct YoutubePreviewProvider {
    /// The bootstrapped InnerTube session, shared across fetches and lazily
    /// (re)created. `None` until the first fetch, or after one expired.
    ctx: Mutex<Option<Arc<InnertubeContext>>>,
}

impl YoutubePreviewProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current context, bootstrapping one if needed.
    async fn context(&self) -> anyhow::Result<Arc<InnertubeContext>> {
        let mut guard = self.ctx.lock().await;
        if let Some(ctx) = guard.as_ref() {
            return Ok(ctx.clone());
        }
        let ctx = Arc::new(InnertubeContext::bootstrap().await?);
        *guard = Some(ctx.clone());
        Ok(ctx)
    }

    /// Drops the cached context so the next fetch bootstraps a fresh one (used
    /// after a fetch fails — the visitor data / key may have expired).
    async fn invalidate(&self) {
        *self.ctx.lock().await = None;
    }
}

#[async_trait]
impl LinkPreviewProvider for YoutubePreviewProvider {
    fn name(&self) -> &'static str {
        "youtube"
    }

    fn match_url(&self, url: &str) -> Option<PreviewTarget> {
        // Only claim URLs that look like YouTube video links — extract_video_id
        // returns None for channel/handle sources, and we don't want to claim a
        // bare 11-char word that isn't a URL, so require a youtube host marker.
        let lower = url.to_ascii_lowercase();
        if !lower.contains("youtube.com/") && !lower.contains("youtu.be/") {
            return None;
        }
        extract_video_id(url).map(|id| PreviewTarget {
            id,
            kind: PreviewKind::Video,
        })
    }

    async fn fetch(&self, target: &PreviewTarget) -> anyhow::Result<LinkPreview> {
        let ctx = self.context().await?;
        let resp = match ctx
            .post(PLAYER_URL, &target.id, json!({ "videoId": target.id }))
            .await
        {
            Ok(resp) => resp,
            Err(err) => {
                // A stale context can 400/403; drop it so the next attempt re-boots.
                self.invalidate().await;
                return Err(err);
            }
        };
        parse_player(&resp).ok_or_else(|| anyhow::anyhow!("no videoDetails in player response"))
    }
}

/// Builds a [`LinkPreview`] from an InnerTube `player` response's `videoDetails`.
/// `None` if the response carries no usable video details (private/removed video,
/// or an unexpected shape).
fn parse_player(resp: &Value) -> Option<LinkPreview> {
    let details = &resp["videoDetails"];
    let title = details["title"].as_str()?.to_string();
    if title.is_empty() {
        return None;
    }
    let author = details["author"].as_str().unwrap_or_default().to_string();
    let thumbnail_url = best_thumbnail(&details["thumbnail"]["thumbnails"]);
    let stats = details["viewCount"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|n| format!("{} views", compact_count(n)));

    Some(LinkPreview {
        kind: PreviewKind::Video,
        title,
        author,
        thumbnail_url,
        stats,
    })
}

/// The largest thumbnail URL in a `thumbnails[]` array (they're ordered
/// smallest→largest, but pick by width to be safe).
fn best_thumbnail(thumbnails: &Value) -> Option<String> {
    let arr = thumbnails.as_array()?;
    arr.iter()
        .max_by_key(|t| t["width"].as_u64().unwrap_or(0))
        .and_then(|t| t["url"].as_str())
        .map(|s| s.to_string())
}

/// Formats a raw count into a compact human string: 1234 → "1.2K", 1_500_000 →
/// "1.5M", 2_000_000_000 → "2B". Whole thousands drop the decimal ("12K" not
/// "12.0K").
fn compact_count(n: u64) -> String {
    const UNITS: &[(u64, char)] = &[(1_000_000_000, 'B'), (1_000_000, 'M'), (1_000, 'K')];
    for &(threshold, suffix) in UNITS {
        if n >= threshold {
            let value = n as f64 / threshold as f64;
            // One decimal, but trim a trailing ".0" (12.0K → 12K).
            let rounded = (value * 10.0).round() / 10.0;
            if (rounded.fract()).abs() < f64::EPSILON {
                return format!("{}{}", rounded as u64, suffix);
            }
            return format!("{rounded:.1}{suffix}");
        }
    }
    n.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> YoutubePreviewProvider {
        YoutubePreviewProvider::new()
    }

    #[test]
    fn matches_watch_and_short_urls() {
        let p = provider();
        assert_eq!(
            p.match_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
                .map(|t| t.id),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            p.match_url("https://youtu.be/dQw4w9WgXcQ?si=x").map(|t| t.id),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            p.match_url("youtube.com/live/dQw4w9WgXcQ").map(|t| t.id),
            Some("dQw4w9WgXcQ".to_string())
        );
    }

    #[test]
    fn does_not_match_non_youtube_or_channel() {
        let p = provider();
        // A bare 11-char word that isn't a YouTube URL must not be claimed.
        assert!(p.match_url("dQw4w9WgXcQ").is_none());
        assert!(p.match_url("https://twitch.tv/somechannel").is_none());
        // A channel/handle page has no video id.
        assert!(p.match_url("https://www.youtube.com/@somechannel").is_none());
    }

    #[test]
    fn compact_count_formats() {
        assert_eq!(compact_count(5), "5");
        assert_eq!(compact_count(999), "999");
        assert_eq!(compact_count(1_200), "1.2K");
        assert_eq!(compact_count(12_000), "12K");
        assert_eq!(compact_count(1_500_000), "1.5M");
        assert_eq!(compact_count(2_000_000_000), "2B");
    }

    #[test]
    fn parses_player_video_details() {
        let resp = json!({
            "videoDetails": {
                "title": "Never Gonna Give You Up",
                "author": "Rick Astley",
                "viewCount": "1600000000",
                "thumbnail": { "thumbnails": [
                    { "url": "https://i.ytimg.com/sm.jpg", "width": 120, "height": 90 },
                    { "url": "https://i.ytimg.com/lg.jpg", "width": 480, "height": 360 }
                ]}
            }
        });
        let preview = parse_player(&resp).expect("should parse");
        assert_eq!(preview.title, "Never Gonna Give You Up");
        assert_eq!(preview.author, "Rick Astley");
        assert_eq!(preview.stats.as_deref(), Some("1.6B views"));
        assert_eq!(
            preview.thumbnail_url.as_deref(),
            Some("https://i.ytimg.com/lg.jpg")
        );
    }

    #[test]
    fn missing_details_is_none() {
        assert!(parse_player(&json!({})).is_none());
        assert!(parse_player(&json!({ "videoDetails": { "title": "" } })).is_none());
    }
}
