//! "Last live" info for an offline channel, via InnerTube — no HTML scraping.
//!
//! Browses the channel's Streams ("Live") tab (`youtubei/v1/browse`, the same
//! JSON the web app renders it from) and takes the newest *finished* entry:
//! only finished streams carry a "Streamed X ago" stamp (live entries show
//! viewer counts, upcoming ones a schedule). The stamp is relative to when the
//! VOD was published — i.e. when the stream **ended** — and the thumbnail badge
//! carries the VOD length, so together they give Kick-style "last live X ago,
//! for Y" info. A handle/URL source is resolved to its `UC…` channel id first
//! via `youtubei/v1/navigation/resolve_url`.
//!
//! YouTube only publishes relative stamps ("Streamed 5 hours ago"), so the
//! times are approximate — coarse for old streams (months round to 30 days).

use bks_platform::LastStream;
use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};

use crate::api::{InnertubeContext, BROWSE_URL, RESOLVE_URL_URL};
use crate::resolve::{channel_page_url, is_channel_id};

/// The Streams tab's `browse` params blob — a serialized proto naming the tab,
/// the same constant for every channel (as the web app sends it).
const STREAMS_TAB_PARAMS: &str = "EgdzdHJlYW1z8gYECgJ6AA%3D%3D";

/// Fetches the channel's most recent past live stream for the offline "last
/// live …" tooltip line. `None` on any failure or when the channel has no
/// finished streams (the tooltip then just shows "offline").
pub async fn fetch_last_stream(source: &str) -> Option<LastStream> {
    let ctx = InnertubeContext::bootstrap().await.ok()?;
    let channel_id = resolve_channel_id(&ctx, source).await?;
    let resp = ctx
        .post(
            BROWSE_URL,
            "",
            json!({ "browseId": channel_id, "params": STREAMS_TAB_PARAMS }),
        )
        .await
        .ok()?;
    last_stream_from_browse(&resp, Utc::now())
}

/// Resolves the tab's source to a `UC…` channel id: a bare id passes through,
/// anything channel-shaped (handle, channel URL) goes through InnerTube's
/// `navigation/resolve_url`. A direct video reference yields `None` (its
/// channel isn't known until it's live — no last-stream line for those).
async fn resolve_channel_id(ctx: &InnertubeContext, source: &str) -> Option<String> {
    let s = source.trim();
    if is_channel_id(s) {
        return Some(s.to_string());
    }
    let url = channel_page_url(s)?;
    let resp = ctx
        .post(RESOLVE_URL_URL, "", json!({ "url": url }))
        .await
        .ok()?;
    resp["endpoint"]["browseEndpoint"]["browseId"]
        .as_str()
        .filter(|id| is_channel_id(id))
        .map(str::to_string)
}

/// Pure part of [`fetch_last_stream`]: the first entry (the tab is
/// newest-first) with a "Streamed … ago" stamp, in either the current
/// `lockupViewModel` shape or the legacy `videoRenderer` one.
fn last_stream_from_browse(resp: &Value, now: DateTime<Utc>) -> Option<LastStream> {
    find_first(resp, &|node| {
        if let Some(lockup) = node.get("lockupViewModel") {
            from_lockup(lockup, now)
        } else if let Some(video) = node.get("videoRenderer") {
            from_video_renderer(video, now)
        } else {
            None
        }
    })
}

/// Depth-first search returning `f`'s first hit. Array order (the grid's
/// newest-first entries) is what matters; object-key order is irrelevant here
/// since renderer lists always sit in arrays.
fn find_first<T>(v: &Value, f: &impl Fn(&Value) -> Option<T>) -> Option<T> {
    if let Some(t) = f(v) {
        return Some(t);
    }
    match v {
        Value::Object(map) => map.values().find_map(|c| find_first(c, f)),
        Value::Array(items) => items.iter().find_map(|c| find_first(c, f)),
        _ => None,
    }
}

/// The current web shape: title + "Streamed … ago" under
/// `metadata.lockupMetadataViewModel`, the VOD length as a thumbnail badge.
fn from_lockup(lockup: &Value, now: DateTime<Utc>) -> Option<LastStream> {
    let meta = &lockup["metadata"]["lockupMetadataViewModel"];
    let ago = meta["metadata"]["contentMetadataViewModel"]["metadataRows"]
        .as_array()?
        .iter()
        .flat_map(|row| row["metadataParts"].as_array().into_iter().flatten())
        .find_map(|part| parse_streamed_ago(part["text"]["content"].as_str()?))?;
    let title = meta["title"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let duration = lockup["contentImage"]["thumbnailViewModel"]["overlays"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|ov| {
            ov["thumbnailBottomOverlayViewModel"]["badges"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .find_map(|badge| parse_hms(badge["thumbnailBadgeViewModel"]["text"].as_str()?));
    Some(build(now, ago, duration, title))
}

/// The legacy shape (older client versions): `publishedTimeText.simpleText`,
/// `title.runs[0].text`, `lengthText.simpleText`.
fn from_video_renderer(video: &Value, now: DateTime<Utc>) -> Option<LastStream> {
    let ago = parse_streamed_ago(video["publishedTimeText"]["simpleText"].as_str()?)?;
    let title = video["title"]["runs"][0]["text"]
        .as_str()
        .or_else(|| video["title"]["simpleText"].as_str())
        .unwrap_or_default()
        .to_string();
    let duration = video["lengthText"]["simpleText"]
        .as_str()
        .and_then(parse_hms);
    Some(build(now, ago, duration, title))
}

/// Assembles the [`LastStream`]: the "ago" stamp marks the stream's *end*, and
/// the VOD length walks back to its start. Without a length badge (rare), the
/// end time is unknown to the caller's math, so only the start is set.
fn build(
    now: DateTime<Utc>,
    ago: chrono::Duration,
    duration: Option<chrono::Duration>,
    title: String,
) -> LastStream {
    let ended = now - ago;
    match duration {
        Some(d) => LastStream {
            started_at: ended - d,
            ended_at: Some(ended),
            title,
            game: String::new(),
        },
        None => LastStream {
            started_at: ended,
            ended_at: None,
            title,
            game: String::new(),
        },
    }
}

/// "Streamed 5 hours ago" → 5h. Only finished streams carry this stamp.
static STREAMED_AGO_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^Streamed (\d+) (second|minute|hour|day|week|month|year)s? ago$").unwrap()
});

/// Parses the relative stamp; months/years are approximated (30/365 days).
fn parse_streamed_ago(text: &str) -> Option<chrono::Duration> {
    let caps = STREAMED_AGO_RE.captures(text)?;
    let n: i64 = caps[1].parse().ok()?;
    Some(match &caps[2] {
        "second" => chrono::Duration::seconds(n),
        "minute" => chrono::Duration::minutes(n),
        "hour" => chrono::Duration::hours(n),
        "day" => chrono::Duration::days(n),
        "week" => chrono::Duration::days(7 * n),
        "month" => chrono::Duration::days(30 * n),
        _ => chrono::Duration::days(365 * n),
    })
}

/// Parses a length badge — "8:31:48", "51:48", or "0:48" — into a duration.
fn parse_hms(text: &str) -> Option<chrono::Duration> {
    let parts: Vec<&str> = text.split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut secs: i64 = 0;
    for part in &parts {
        secs = secs * 60 + part.trim().parse::<i64>().ok()?;
    }
    Some(chrono::Duration::seconds(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lockup_entry_parses_end_based() {
        let now = Utc::now();
        // Trimmed real shape: title + views/"Streamed … ago" metadata parts +
        // a length badge on the thumbnail.
        let resp: Value = serde_json::from_str(
            r#"{"contents":[{"lockupViewModel":{
                "contentImage":{"thumbnailViewModel":{"overlays":[
                    {"thumbnailBottomOverlayViewModel":{"badges":[
                        {"thumbnailBadgeViewModel":{"text":"8:31:48"}}]}}]}},
                "metadata":{"lockupMetadataViewModel":{
                    "title":{"content":"SOLO WIPEDAY DAY 1 !DISCORD"},
                    "metadata":{"contentMetadataViewModel":{"metadataRows":[
                        {"metadataParts":[
                            {"text":{"content":"5.9K views"}},
                            {"text":{"content":"Streamed 5 hours ago"}}]}]}}}}}}]}"#,
        )
        .unwrap();
        let last = last_stream_from_browse(&resp, now).expect("a past stream");
        assert_eq!(last.title, "SOLO WIPEDAY DAY 1 !DISCORD");
        let ended = last.ended_at.expect("end known from the length badge");
        assert_eq!(now - ended, chrono::Duration::hours(5));
        assert_eq!(
            ended - last.started_at,
            chrono::Duration::hours(8)
                + chrono::Duration::minutes(31)
                + chrono::Duration::seconds(48)
        );
    }

    #[test]
    fn live_and_upcoming_lockups_are_skipped() {
        let now = Utc::now();
        // A live entry has no "Streamed … ago" part; the finished one after it wins.
        let resp: Value = serde_json::from_str(
            r#"{"contents":[
                {"lockupViewModel":{"metadata":{"lockupMetadataViewModel":{
                    "title":{"content":"LIVE now"},
                    "metadata":{"contentMetadataViewModel":{"metadataRows":[
                        {"metadataParts":[{"text":{"content":"1.2K watching"}}]}]}}}}}},
                {"lockupViewModel":{"metadata":{"lockupMetadataViewModel":{
                    "title":{"content":"yesterday"},
                    "metadata":{"contentMetadataViewModel":{"metadataRows":[
                        {"metadataParts":[{"text":{"content":"Streamed 1 day ago"}}]}]}}}}}}]}"#,
        )
        .unwrap();
        let last = last_stream_from_browse(&resp, now).expect("the finished entry");
        assert_eq!(last.title, "yesterday");
        // No length badge → only the (end-stamped) start is known.
        assert_eq!(last.ended_at, None);
        assert_eq!(now - last.started_at, chrono::Duration::days(1));
    }

    #[test]
    fn legacy_video_renderer_parses() {
        let now = Utc::now();
        let resp: Value = serde_json::from_str(
            r#"{"contents":[{"videoRenderer":{
                "title":{"runs":[{"text":"old shape"}]},
                "lengthText":{"simpleText":"51:48"},
                "publishedTimeText":{"simpleText":"Streamed 2 weeks ago"}}}]}"#,
        )
        .unwrap();
        let last = last_stream_from_browse(&resp, now).expect("a past stream");
        assert_eq!(last.title, "old shape");
        assert_eq!(now - last.ended_at.unwrap(), chrono::Duration::days(14));
        assert_eq!(
            last.ended_at.unwrap() - last.started_at,
            chrono::Duration::minutes(51) + chrono::Duration::seconds(48)
        );
    }

    #[test]
    fn no_finished_entries_is_none() {
        assert!(
            last_stream_from_browse(&serde_json::json!({"contents": []}), Utc::now()).is_none()
        );
    }

    #[test]
    fn hms_parsing() {
        assert_eq!(
            parse_hms("8:31:48"),
            Some(chrono::Duration::seconds(8 * 3600 + 31 * 60 + 48))
        );
        assert_eq!(
            parse_hms("51:48"),
            Some(chrono::Duration::seconds(51 * 60 + 48))
        );
        assert_eq!(parse_hms("0:48"), Some(chrono::Duration::seconds(48)));
        assert_eq!(parse_hms("not a time"), None);
    }

    #[test]
    fn streamed_ago_units() {
        assert_eq!(
            parse_streamed_ago("Streamed 1 hour ago"),
            Some(chrono::Duration::hours(1))
        );
        assert_eq!(
            parse_streamed_ago("Streamed 3 months ago"),
            Some(chrono::Duration::days(90))
        );
        assert_eq!(parse_streamed_ago("1.2K watching"), None);
        assert_eq!(parse_streamed_ago("5.9K views"), None);
    }
}
