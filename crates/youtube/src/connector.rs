//! Anonymous YouTube live-chat connector over InnerTube.
//!
//! [`YouTubeSource`] implements [`ChatSource`]. On `join` it spawns a task that:
//! 1. resolves the tab's source (handle / URL / video) to a *currently live*
//!    video id ([`crate::resolve`]), retrying with backoff while offline;
//! 2. bootstraps an InnerTube context ([`crate::api`]);
//! 3. POSTs `youtubei/v1/next` to get the initial live-chat continuation token
//!    plus the stream title / owner channel id / start time;
//! 4. long-polls `get_live_chat`, honoring each response's `timeoutMs`, turning
//!    chat items into [`ChatEvent`]s via [`crate::builder`].
//!
//! There is no push socket (unlike Kick's Pusher) — the browser itself polls, so
//! we do too. A dropped/expired continuation re-runs step 3; a stream ending
//! sends an offline `Live` event and returns to step 1 to wait for the next.

use async_trait::async_trait;
use std::collections::HashSet;
use std::time::Duration;

use bks_core::Platform;
use bks_platform::{ChannelMeta, ChatEvent, ChatSink, ChatSource, ChatStream};
use chrono::Utc;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::api::{InnertubeContext, GET_LIVE_CHAT_URL, NEXT_URL, PLAYER_URL, UPDATED_METADATA_URL};
use crate::builder::{item_to_event, parse_runs_text};

/// How long to wait before re-checking a channel that isn't live yet, and before
/// reconnecting after a recoverable error.
const OFFLINE_RETRY: Duration = Duration::from_secs(15);
/// Floor for the server-provided poll delay, so a `timeoutMs: 0` can't busy-loop.
const MIN_POLL_DELAY: Duration = Duration::from_millis(1000);
/// How often to refresh the concurrent-viewer count while live (rides the chat
/// poll loop as its own throttled `updated_metadata` request).
const VIEWER_POLL: Duration = Duration::from_secs(30);

/// Anonymous YouTube connector. Each `join` resolves the source and runs its own
/// InnerTube poll loop; events reach the UI only through the returned stream.
#[derive(Default)]
pub struct YouTubeSource;

impl YouTubeSource {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ChatSource for YouTubeSource {
    async fn join(&self, channel: &str) -> anyhow::Result<ChatStream> {
        let channel = channel.trim().to_string();
        let (tx, rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            if let Err(err) = run(channel.clone(), tx.clone()).await {
                let _ = tx.send(ChatEvent::Error(format!("youtube failed: {err:#}")));
            }
        });

        Ok(rx)
    }
}

/// The outer loop: resolve → connect → poll, waiting for the stream to be live
/// and reconnecting when a live session ends. Returns only when the UI drops the
/// stream (a send error).
async fn run(source: String, tx: ChatSink) -> anyhow::Result<()> {
    // The `Channel` meta (for emote loading) is emitted once, keyed on the owner
    // channel id we learn on the first successful `next`. We (re)send it each live
    // session in case the owner id wasn't known before.
    //
    // The offline status (with the "last live …" info scraped from the /streams
    // tab) is sent once per offline stretch — on the first failed resolve and on a
    // stream ending — not on every retry poll, so waiting offline costs no extra
    // requests.
    let mut offline_sent = false;
    loop {
        let video_id = match crate::resolve::resolve_live_video_id(&source).await {
            Some(id) => id,
            None => {
                // Not live (or resolution failed). Wait, then retry — no chat until
                // then. We don't spam an error row; being offline is normal.
                if !offline_sent {
                    offline_sent = true;
                    let last = crate::streams::fetch_last_stream(&source).await;
                    let _ = tx.send(offline_live(last));
                }
                if sleep_or_stop(&tx, OFFLINE_RETRY).await {
                    return Ok(());
                }
                continue;
            }
        };
        offline_sent = false;

        match watch_live_chat(&source, &video_id, &tx).await {
            Ok(Outcome::StreamEnded) => {
                offline_sent = true;
                let last = crate::streams::fetch_last_stream(&source).await;
                let _ = tx.send(offline_live(last));
                if sleep_or_stop(&tx, OFFLINE_RETRY).await {
                    return Ok(());
                }
            }
            Ok(Outcome::Stopped) => return Ok(()),
            Err(err) => {
                tracing::warn!("youtube live chat error for {source}: {err:#}");
                if sleep_or_stop(&tx, OFFLINE_RETRY).await {
                    return Ok(());
                }
            }
        }
    }
}

/// Why the inner watch loop returned.
enum Outcome {
    /// The live session ended (continuation gone, broadcast offline) — wait for next.
    StreamEnded,
    /// The UI dropped the stream; stop entirely.
    Stopped,
}

/// Connects to one live session and polls its chat until it ends or the UI drops.
async fn watch_live_chat(source: &str, video_id: &str, tx: &ChatSink) -> anyhow::Result<Outcome> {
    let ctx = InnertubeContext::bootstrap().await?;

    // `next` gives the initial continuation + live metadata.
    let next = ctx
        .post(NEXT_URL, video_id, json!({ "videoId": video_id }))
        .await?;

    let live_chat_renderer =
        &next["contents"]["twoColumnWatchNextResults"]["conversationBar"]["liveChatRenderer"];
    let Some(mut continuation) = initial_continuation(live_chat_renderer) else {
        // No live chat continuation → not a live stream (VOD/ended).
        return Ok(Outcome::StreamEnded);
    };

    let owner_id = live_owner_channel_id(&next).unwrap_or_default();
    let title = live_title(&next);
    // `next` rarely carries the microformat; the `player` endpoint's
    // `liveBroadcastDetails.startTimestamp` is the reliable source for the
    // tooltip's uptime readout. One extra request per live session.
    let mut started_at = live_started_at(&next);
    if started_at.is_none() {
        started_at = player_started_at(&ctx, video_id).await;
    }

    // Channel identity (for emote loading) + a live notice with the metadata.
    if tx
        .send(ChatEvent::Channel(ChannelMeta {
            platform: Platform::YouTube,
            id: owner_id,
            name: source.to_string(),
        }))
        .is_err()
    {
        return Ok(Outcome::Stopped);
    }
    let _ = tx.send(ChatEvent::Live {
        platform: Platform::YouTube,
        live: true,
        title,
        game: String::new(),
        started_at,
        last_stream: None,
        // The stream's own watch link — a YouTube live is a specific video, so
        // the tab tooltip can open it directly instead of the channel page.
        link: Some(format!("https://www.youtube.com/watch?v={video_id}")),
    });

    let mut seen: HashSet<String> = HashSet::new();
    // Skip the first page's backlog so we don't dump a wall of old messages on
    // join. History interleaving is handled elsewhere.
    let mut skip_backlog = true;
    let mut viewers: Option<u64> = None;
    let mut next_viewer_check = tokio::time::Instant::now();

    loop {
        // Throttled viewer-count refresh for the status bar. A failed *request*
        // keeps the previous count; a response that carries no viewership
        // (e.g. "No one watching") clears it — otherwise a stale number would
        // stay frozen on screen for the rest of the session.
        if tokio::time::Instant::now() >= next_viewer_check {
            next_viewer_check = tokio::time::Instant::now() + VIEWER_POLL;
            if let Some(fetched) = fetch_viewer_count(&ctx, video_id).await {
                tracing::debug!("youtube viewer count for {source}: {fetched:?}");
                if viewers != fetched {
                    viewers = fetched;
                    let _ = tx.send(ChatEvent::Viewers {
                        platform: Platform::YouTube,
                        count: viewers,
                    });
                }
            }
        }

        let body = json!({ "continuation": continuation });
        let resp = ctx.post(GET_LIVE_CHAT_URL, video_id, body).await?;

        let live_chat = &resp["continuationContents"]["liveChatContinuation"];
        if live_chat.is_null() {
            // Continuation expired or the broadcast went offline.
            return Ok(Outcome::StreamEnded);
        }

        if !skip_backlog {
            for action in live_chat["actions"].as_array().into_iter().flatten() {
                let item = &action["addChatItemAction"]["item"];
                if item.is_null() {
                    continue;
                }
                let Some(id) = item_id(item) else { continue };
                if !seen.insert(id) {
                    continue;
                }
                if let Some(event) = item_to_event(source, item) {
                    if tx.send(event).is_err() {
                        return Ok(Outcome::Stopped);
                    }
                }
            }
        } else {
            // Still record the backlog ids so we don't re-deliver them next page.
            for action in live_chat["actions"].as_array().into_iter().flatten() {
                if let Some(id) = item_id(&action["addChatItemAction"]["item"]) {
                    seen.insert(id);
                }
            }
            skip_backlog = false;
        }

        let (next_continuation, delay) = match next_continuation(live_chat) {
            Some(pair) => pair,
            // No further continuation → the live chat is done.
            None => return Ok(Outcome::StreamEnded),
        };
        continuation = next_continuation;

        if sleep_or_stop(tx, delay).await {
            return Ok(Outcome::Stopped);
        }
    }
}

/// The initial continuation token from the `liveChatRenderer` (its first
/// `continuations[]` entry, whatever data variant it uses).
fn initial_continuation(renderer: &Value) -> Option<String> {
    renderer["continuations"]
        .as_array()?
        .iter()
        .find_map(continuation_token)
}

/// The next continuation token + poll delay from a `liveChatContinuation`, reading
/// whichever of `timedContinuationData` / `invalidationContinuationData` /
/// `reloadContinuationData` is present.
fn next_continuation(live_chat: &Value) -> Option<(String, Duration)> {
    for c in live_chat["continuations"].as_array()?.iter() {
        for key in [
            "timedContinuationData",
            "invalidationContinuationData",
            "reloadContinuationData",
        ] {
            let data = &c[key];
            if let Some(token) = data["continuation"].as_str() {
                if !token.is_empty() {
                    let ms = data["timeoutMs"].as_u64().unwrap_or(1000);
                    let delay = Duration::from_millis(ms).max(MIN_POLL_DELAY);
                    return Some((token.to_string(), delay));
                }
            }
        }
    }
    None
}

/// A continuation token from any `continuations[]` entry variant.
fn continuation_token(c: &Value) -> Option<String> {
    for key in [
        "timedContinuationData",
        "invalidationContinuationData",
        "reloadContinuationData",
        "liveChatReplayContinuationData",
    ] {
        if let Some(token) = c[key]["continuation"].as_str() {
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    None
}

/// The renderer id of a chat item (the single nested renderer's `id`).
fn item_id(item: &Value) -> Option<String> {
    let (_, renderer) = item.as_object()?.iter().next()?;
    renderer["id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The live broadcast's owner channel id, from the `next` response.
fn live_owner_channel_id(next: &Value) -> Option<String> {
    // Present under the watch-next results' owner renderer.
    let results = next["contents"]["twoColumnWatchNextResults"]["results"]["results"]["contents"]
        .as_array()?;
    for c in results {
        let owner = &c["videoSecondaryInfoRenderer"]["owner"]["videoOwnerRenderer"];
        if let Some(id) = owner["navigationEndpoint"]["browseEndpoint"]["browseId"].as_str() {
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    None
}

/// The live stream title from the `next` response's primary info renderer.
fn live_title(next: &Value) -> String {
    let results =
        next["contents"]["twoColumnWatchNextResults"]["results"]["results"]["contents"].as_array();
    for c in results.into_iter().flatten() {
        let title = &c["videoPrimaryInfoRenderer"]["title"];
        if !title.is_null() {
            let text = parse_runs_text(title);
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}

/// The live stream start time (the microformat `startTimestamp`), if the `next`
/// response carries it; `None` otherwise (the `player` fallback then runs).
fn live_started_at(next: &Value) -> Option<chrono::DateTime<Utc>> {
    next["microformat"]["playerMicroformatRenderer"]["liveBroadcastDetails"]["startTimestamp"]
        .as_str()
        .and_then(bks_core::parse_rfc3339)
}

/// The live stream start time from the `player` endpoint's
/// `liveBroadcastDetails` — the reliable source (`next` usually has no
/// microformat). `None` on any failure (the uptime readout just won't show).
async fn player_started_at(
    ctx: &InnertubeContext,
    video_id: &str,
) -> Option<chrono::DateTime<Utc>> {
    let resp = ctx
        .post(PLAYER_URL, video_id, json!({ "videoId": video_id }))
        .await
        .ok()?;
    resp["microformat"]["playerMicroformatRenderer"]["liveBroadcastDetails"]["startTimestamp"]
        .as_str()
        .and_then(bks_core::parse_rfc3339)
}

/// The live stream's concurrent viewer count from the `updated_metadata`
/// endpoint. The outer `Option` is the *request*: `None` = it failed (keep the
/// previous count). The inner is the count: `Some(n)` watching now, `None` = the
/// response carries no viewership (clear the shown count).
async fn fetch_viewer_count(ctx: &InnertubeContext, video_id: &str) -> Option<Option<u64>> {
    let resp = ctx
        .post(UPDATED_METADATA_URL, video_id, json!({ "videoId": video_id }))
        .await
        .ok()?;
    Some(viewership_count(&resp))
}

/// The number in an `updated_metadata` response's `updateViewershipAction`
/// ("1,234 watching now" → 1234). `None` when the action is absent or the text
/// has no digits ("No one watching").
fn viewership_count(resp: &Value) -> Option<u64> {
    let view_count = resp["actions"].as_array()?.iter().find_map(|a| {
        let vc = &a["updateViewershipAction"]["viewCount"]["videoViewCountRenderer"]["viewCount"];
        (!vc.is_null()).then_some(vc)
    })?;
    let text = parse_runs_text(view_count);
    let digits: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// An offline `Live` event (title/start cleared), carrying the most recent past
/// stream for the tooltip's "last live …" line when known.
fn offline_live(last_stream: Option<bks_platform::LastStream>) -> ChatEvent {
    ChatEvent::Live {
        platform: Platform::YouTube,
        live: false,
        title: String::new(),
        game: String::new(),
        started_at: None,
        last_stream,
        link: None,
    }
}

/// Sleeps `delay`, returning `true` when the UI has dropped the stream (checked
/// before and after the sleep) so the caller stops instead of polling into a void.
async fn sleep_or_stop(tx: &ChatSink, delay: Duration) -> bool {
    if tx.is_closed() {
        return true;
    }
    tokio::time::sleep(delay).await;
    tx.is_closed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewership_count_parses_watching_now_runs() {
        let resp: Value = serde_json::from_str(
            r#"{ "actions": [
                { "updateTitleAction": { "title": { "simpleText": "t" } } },
                { "updateViewershipAction": { "viewCount": { "videoViewCountRenderer": {
                    "viewCount": { "runs": [ { "text": "12,345" }, { "text": " watching now" } ] },
                    "isLive": true
                } } } }
            ] }"#,
        )
        .unwrap();
        assert_eq!(viewership_count(&resp), Some(12345));
    }

    #[test]
    fn viewership_count_absent_or_digitless_is_none() {
        let no_action: Value = serde_json::from_str(r#"{ "actions": [] }"#).unwrap();
        assert_eq!(viewership_count(&no_action), None);
        let no_digits: Value = serde_json::from_str(
            r#"{ "actions": [ { "updateViewershipAction": { "viewCount": { "videoViewCountRenderer": {
                "viewCount": { "simpleText": "No one watching" } } } } } ] }"#,
        )
        .unwrap();
        assert_eq!(viewership_count(&no_digits), None);
    }
}
