//! Kick channel resolution.
//!
//! Kick chat runs over Pusher, but you can only subscribe once you know the
//! channel's *chatroom id*. That id comes from `kick.com/api/v2/channels/{slug}`
//! — which sits behind Cloudflare. Cloudflare fingerprints the TLS ClientHello
//! and 403s every *plain* in-process Rust HTTP client (rustls *and* native-tls
//! both get "Request blocked by security policy"); only browser/curl/edge
//! fingerprints pass. We pass it by making the request with [`wreq`], which
//! forges a real Chrome BoringSSL handshake — so these anonymous reads
//! (channel / emotes / usercard / history) go straight to Kick, no broker proxy.
//! The OAuth token exchange (which needs the client *secret*) still goes through
//! the broker; only the read endpoints moved in-process.

use anyhow::Context;
use once_cell::sync::Lazy;
use serde::Deserialize;
use wreq::Client;
use wreq_util::Emulation;

const CHANNELS_URL: &str = "https://kick.com/api/v2/channels/";
const HISTORY_BASE: &str = "https://web.kick.com/api/v1/chat/";

/// One process-wide `wreq` client with a Chrome emulation profile, shared across
/// every Kick read. Building it sets up the BoringSSL fingerprint once; reusing it
/// also pools connections. Browser-looking headers are added per-request
/// ([`kick_get`]) since a bare request still 403s even with the right handshake.
static CLIENT: Lazy<Client> = Lazy::new(|| {
    Client::builder()
        .emulation(Emulation::Chrome136)
        // No default timeout otherwise — a stalled connection would hang a
        // channel join / usercard / history fetch forever.
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .expect("building wreq client")
});

/// Issues a GET to a Cloudflare-fronted Kick URL with the shared emulated client
/// plus browser-looking headers (Accept / UA-ish referer). Returns the response.
async fn kick_get(url: impl wreq::IntoUrl) -> wreq::Result<wreq::Response> {
    CLIENT
        .get(url)
        .header("accept", "application/json")
        .header("accept-language", "en-US,en;q=0.9")
        .header("referer", "https://kick.com/")
        .send()
        .await
}

/// Fetches and parses the Cloudflare-fronted v2 channel endpoint for `slug` into
/// the raw struct. Shared by the join path ([`fetch_channel_info`]) and the
/// history-id fallback ([`fetch_history_channel_id`]).
async fn fetch_raw_channel(slug: &str) -> anyhow::Result<RawChannel> {
    let resp = kick_get(format!(
        "{CHANNELS_URL}{}",
        bks_core::encode_url_component(slug)
    ))
    .await
    .with_context(|| format!("resolving kick channel {slug}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("kick channel lookup for {slug} returned {}", resp.status());
    }
    resp.json()
        .await
        .with_context(|| format!("parsing kick channel response for {slug}"))
}

/// The bits we need to join a Kick chatroom + render its badges: the chatroom id
/// to subscribe to, the channel's user id, and the per-tier subscriber badges.
#[derive(Debug, Clone)]
pub struct ChannelInfo {
    pub chatroom_id: u64,
    pub user_id: u64,
    /// The id the web.kick.com chat-history endpoint keys on (the v2
    /// `chatroom.channel_id`), distinct from the Pusher `chatroom_id`. `0` if Kick
    /// didn't return it; history then falls back to a slug lookup.
    pub channel_id: u64,
    pub subscriber_badges: Vec<SubscriberBadge>,
    /// Whether the channel is currently broadcasting (from the v2 `livestream`).
    pub is_live: bool,
    /// The current stream title when live (empty otherwise / when unavailable).
    pub livestream_title: String,
    /// The current category/game (from the v2 `livestream.categories[0].name`),
    /// empty when offline or unavailable.
    pub livestream_category: String,
    /// When the current stream began (from the v2 `livestream.start_time`), for an
    /// uptime readout. `None` when offline.
    pub livestream_started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The channel's most recent past broadcast (the latest VOD), for the offline
    /// tooltip. `None` when live or when the channel has no VODs.
    pub last_stream: Option<LastStream>,
}

/// A subscriber tier badge: shown once a chatter has been subscribed `months`+.
#[derive(Debug, Clone)]
pub struct SubscriberBadge {
    pub months: u64,
    pub src: String,
}

/// A channel's most recent past broadcast, shown in the offline tooltip ("last
/// live Xh ago for Ym"). `ended_at` is derived from start + duration.
#[derive(Debug, Clone)]
pub struct LastStream {
    /// When that stream began.
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// When it ended (`started_at` + duration). Equals `started_at` if Kick
    /// reported a zero duration.
    pub ended_at: chrono::DateTime<chrono::Utc>,
    pub title: String,
    pub category: String,
}

// ---- Raw Kick v2 channel JSON (only the fields we read) ---------------------

#[derive(Deserialize)]
struct RawChannel {
    user_id: u64,
    chatroom: RawChatroom,
    #[serde(default)]
    subscriber_badges: Vec<RawSubscriberBadge>,
    #[serde(default)]
    livestream: Option<RawLivestream>,
}

#[derive(Deserialize)]
struct RawChatroom {
    id: u64,
    #[serde(default)]
    channel_id: u64,
}

#[derive(Deserialize)]
struct RawSubscriberBadge {
    #[serde(default)]
    months: u64,
    #[serde(default)]
    badge_image: Option<RawBadgeImage>,
}

#[derive(Deserialize)]
struct RawBadgeImage {
    #[serde(default)]
    src: String,
}

#[derive(Deserialize)]
struct RawLivestream {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    is_live: bool,
    #[serde(default)]
    session_title: String,
    #[serde(default)]
    start_time: String,
    #[serde(default)]
    categories: Vec<RawCategory>,
}

#[derive(Deserialize)]
struct RawCategory {
    #[serde(default)]
    name: String,
}

/// One past-broadcast (VOD) entry from `/channels/{slug}/videos` (newest-first).
#[derive(Deserialize)]
struct RawVideo {
    #[serde(default)]
    start_time: String,
    #[serde(default)]
    duration: i64,
    #[serde(default)]
    session_title: String,
    #[serde(default)]
    categories: Vec<RawCategory>,
}

/// Kick channel slugs are lowercase; strip a leading `#` and lowercase.
pub fn slugify(channel: &str) -> String {
    bks_core::channel_login(channel)
}

/// Resolves a channel name to its chatroom id (+ user id + subscriber badges +
/// live status) by calling Kick's Cloudflare-fronted channels endpoint directly
/// via the emulated client. When offline, also fetches the latest VOD so the
/// tooltip can show the last broadcast.
pub async fn fetch_channel_info(channel: &str) -> anyhow::Result<ChannelInfo> {
    let slug = slugify(channel);
    let raw = fetch_raw_channel(&slug).await?;

    let subscriber_badges = raw
        .subscriber_badges
        .into_iter()
        .filter_map(|b| {
            let src = b.badge_image?.src;
            (!src.is_empty()).then_some(SubscriberBadge {
                months: b.months,
                src,
            })
        })
        .collect();

    // `livestream` is null when offline; present (with `is_live`/`id`) when live.
    let live = raw.livestream.as_ref();
    let is_live = live.is_some_and(|l| l.is_live || l.id.is_some());

    let (livestream_title, livestream_category, livestream_started_at) = if is_live {
        let l = live.expect("is_live implies livestream present");
        let category = l
            .categories
            .first()
            .map(|c| c.name.clone())
            .unwrap_or_default();
        (
            l.session_title.clone(),
            category,
            parse_kick_time(&l.start_time),
        )
    } else {
        (String::new(), String::new(), None)
    };

    // Only fetch the VOD when offline (a live channel shows current status); this
    // keeps the live join path to a single request.
    let last_stream = if is_live {
        None
    } else {
        fetch_last_stream(&slug).await
    };

    Ok(ChannelInfo {
        chatroom_id: raw.chatroom.id,
        user_id: raw.user_id,
        channel_id: raw.chatroom.channel_id,
        subscriber_badges,
        is_live,
        livestream_title,
        livestream_category,
        livestream_started_at,
        last_stream,
    })
}

/// Fetches the channel's current concurrent viewer count from the lightweight
/// v2 livestream endpoint (`/channels/{slug}/livestream` — `data` is null when
/// offline). `Ok(None)` means *offline*; a live response that lacks the count
/// (seen transiently right at go-live) is an `Err`, so the polling caller keeps
/// the previous number instead of blanking a live stream's count.
pub async fn fetch_viewer_count(channel: &str) -> anyhow::Result<Option<u64>> {
    let slug = slugify(channel);
    let resp = kick_get(format!(
        "{CHANNELS_URL}{}/livestream",
        bks_core::encode_url_component(&slug)
    ))
    .await
    .with_context(|| format!("requesting kick livestream for {slug}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("kick livestream lookup for {slug} returned {}", resp.status());
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .with_context(|| format!("parsing kick livestream response for {slug}"))?;
    let count = viewer_count_from(&body)
        .with_context(|| format!("kick livestream response for {slug} has no viewer count"))?;
    tracing::debug!("kick viewer count for {slug}: {count:?}");
    Ok(count)
}

/// The viewer count in a v2 livestream response: `Ok(None)` when `data` is null
/// (offline), `Ok(Some(n))` when live, `None` (→ error upstream) when live but
/// the count field is null/absent. The `/livestream` endpoint names the field
/// `viewers` (verified live), unlike the channel endpoint's embedded livestream
/// object (`viewer_count`) — accept both in case Kick ever aligns them.
fn viewer_count_from(body: &serde_json::Value) -> Option<Option<u64>> {
    let data = &body["data"];
    if data.is_null() {
        return Some(None);
    }
    data["viewers"]
        .as_u64()
        .or(data["viewer_count"].as_u64())
        .map(Some)
}

/// Fetches the channel's most recent past broadcast from the VODs endpoint
/// (`/channels/{slug}/videos`, newest-first, so `[0]` is the latest) for the
/// offline tooltip. `None` on any error / no VODs (the tooltip then just shows
/// "offline" with no last-stream line).
async fn fetch_last_stream(slug: &str) -> Option<LastStream> {
    let resp = kick_get(format!(
        "{CHANNELS_URL}{}/videos",
        bks_core::encode_url_component(slug)
    ))
    .await
    .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let videos: Vec<RawVideo> = resp.json().await.ok()?;
    let latest = videos.into_iter().next()?;
    let started_at = parse_kick_time(&latest.start_time)?;
    let ended_at = started_at + chrono::Duration::milliseconds(latest.duration.max(0));
    Some(LastStream {
        started_at,
        ended_at,
        title: latest.session_title,
        category: latest
            .categories
            .into_iter()
            .next()
            .map(|c| c.name)
            .unwrap_or_default(),
    })
}

/// A pinned-message record, shared by the join-time seed (the `/history`
/// payload's `pinned_message` — Kick has no anonymous pin GET endpoint, see
/// [`crate::history`]) and the live `PinnedMessageCreatedEvent` (same shape).
/// `message` is a full chat-message payload (the same shape as a live
/// `ChatMessageEvent`), handed to the regular message builder by the connector.
#[derive(Deserialize)]
pub struct PinnedInfo {
    pub message: crate::builder::KickChatMessage,
    /// Pin duration in seconds; Kick sends it as a *string* live ("1200") and
    /// possibly a number elsewhere. `0` when absent.
    #[serde(default, deserialize_with = "flexible_u64")]
    pub duration: u64,
    /// The pinning moderator. The live Pusher event names it `pinnedBy`; the
    /// `/history` seed uses `pinned_by` — accept both.
    #[serde(default, rename = "pinnedBy", alias = "pinned_by")]
    pub pinned_by: Option<PinnedBy>,
    /// RFC-3339 expiry, when the endpoint provides one (takes precedence over
    /// `duration`, whose start time we don't know for a seeded pin).
    #[serde(default, alias = "finishes_at")]
    pub finish_at: Option<String>,
}

#[derive(Deserialize)]
pub struct PinnedBy {
    #[serde(default)]
    pub username: String,
}

/// Accepts a JSON number or a numeric string (Kick sends `"duration":"1200"`).
fn flexible_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(match v {
        serde_json::Value::Number(n) => n.as_u64().unwrap_or(0),
        serde_json::Value::String(s) => s.parse().unwrap_or(0),
        _ => 0,
    })
}

/// Parses Kick's `livestream.start_time` to UTC, `None` on an empty/bad value.
/// Kick sends `"YYYY-MM-DD HH:MM:SS"` — space-separated and with no timezone, but
/// the values are UTC — so it's neither RFC-3339 nor has an offset; we parse the
/// naive form and assume UTC. A real RFC-3339 value (offset/`Z` or a `T`) is also
/// accepted as a fallback in case the upstream format ever changes. The Pusher
/// `StreamerIsLive` event's `created_at` is RFC-3339, which the fallback covers,
/// so the connector reuses this for that too.
pub fn parse_kick_time(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::{NaiveDateTime, TimeZone};
    if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Some(chrono::Utc.from_utc_datetime(&naive));
    }
    // Fall back to a real RFC-3339 value (offset/`Z`/`T`), shared with the other
    // connectors — covers the Pusher `StreamerIsLive` `created_at`.
    bks_core::parse_rfc3339(s)
}

// ---- Native channel emotes --------------------------------------------------

/// One native Kick emote from the channel emote endpoint.
#[derive(Deserialize)]
struct RawEmote {
    id: u64,
    name: String,
}

/// One emote set from `kick.com/emotes/{slug}`: the channel's own set carries a
/// numeric `user_id`; the shared "Global"/"Emoji" sets identify by their `id`.
#[derive(Deserialize)]
struct RawEmoteSet {
    #[serde(default)]
    id: Option<serde_json::Value>,
    #[serde(default)]
    user_id: Option<u64>,
    #[serde(default)]
    emotes: Vec<RawEmote>,
}

/// The picker section label (emote tooltip provider) for a Kick emote set.
fn kick_set_provider(set: &str) -> &'static str {
    match set {
        "global" => "Kick Global",
        "emoji" => "Kick Emoji",
        // "channel" or anything else: the channel's own emotes.
        _ => "Kick",
    }
}

/// Fetches the channel's native Kick emotes from `kick.com/emotes/{slug}`
/// (Cloudflare-fronted, so via the emulated client). Kick's emote slug uses dashes
/// where the channel slug uses underscores, so `_`→`-` is mapped here. Returns
/// ready-to-render [`bks_core::Emote`]s with the Kick CDN url.
pub async fn fetch_channel_emotes(channel: &str) -> anyhow::Result<Vec<bks_core::Emote>> {
    let slug = slugify(channel).replace('_', "-");
    let resp = kick_get(format!(
        "https://kick.com/emotes/{}",
        bks_core::encode_url_component(&slug)
    ))
    .await
    .with_context(|| format!("resolving kick emotes for {slug}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("kick emotes lookup for {slug} returned {}", resp.status());
    }
    let sets: Vec<RawEmoteSet> = resp
        .json()
        .await
        .with_context(|| format!("parsing kick emotes response for {slug}"))?;

    let mut emotes = Vec::new();
    for set in sets {
        // Classify the set: a numeric `user_id` marks the channel's own emotes; the
        // shared sets identify themselves by `id` ("Global" / "Emoji").
        let kind = if set.user_id.is_some() {
            "channel"
        } else {
            match set.id.as_ref().and_then(|v| v.as_str()) {
                Some("Emoji") => "emoji",
                _ => "global", // "Global" or any other shared set
            }
        };
        let provider = kick_set_provider(kind);
        for e in set.emotes {
            emotes.push(bks_core::Emote {
                url: crate::builder::emote_url(e.id),
                id: e.id.to_string(),
                tooltip: bks_core::EmoteTooltip::provider(provider),
                name: e.name,
                animated: false,
            });
        }
    }
    Ok(emotes)
}

// ---- Usercard ---------------------------------------------------------------

/// A chatter's standing *within a channel*, shown in the usercard header. Comes
/// from the per-channel `channels/{channel}/users/{slug}` endpoint, which — unlike
/// the account-level channel lookup — carries the relationship fields the card
/// wants: follow date, sub months, and whether they mod this channel, plus the
/// avatar.
///
/// Every string field tolerates an explicit JSON `null` (Kick leaves
/// `profile_pic` null for users who never set one) via [`null_to_default`],
/// since `#[serde(default)]` alone only covers a *missing* key, not a present
/// `null` — that mismatch was the "invalid type null, expected string" error.
#[derive(Debug, Clone, Deserialize)]
pub struct KickUserInfo {
    #[serde(default, deserialize_with = "null_to_default")]
    pub username: String,
    #[serde(default, deserialize_with = "null_to_default")]
    pub profile_pic: String,
    #[serde(default)]
    pub is_moderator: bool,
    /// RFC-3339 timestamp of when they followed this channel; `None` if not
    /// following. (`Option<String>` maps a JSON `null` to `None` natively.)
    #[serde(default)]
    pub following_since: Option<String>,
    /// Months subscribed to this channel; `0` when not subscribed.
    #[serde(default)]
    pub subscribed_for: u64,
}

/// Deserializes a field as its `Default` when the JSON value is `null` (or
/// absent), instead of erroring. Lets nullable Kick strings map to `""`/`None`.
fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + serde::Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Looks up a chatter's standing in `channel` via the per-channel
/// `channels/{channel}/users/{slug}` endpoint (Cloudflare-fronted, unauthenticated;
/// via the emulated client). `channel` is the channel slug and `slug` is the
/// chatter's login.
pub async fn fetch_user_info(channel: &str, slug: &str) -> anyhow::Result<KickUserInfo> {
    let channel = slugify(channel);
    let slug = slugify(slug);
    let resp = kick_get(format!(
        "{CHANNELS_URL}{}/users/{}",
        bks_core::encode_url_component(&channel),
        bks_core::encode_url_component(&slug)
    ))
    .await
    .with_context(|| format!("resolving kick user {slug} in {channel}"))?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "kick user lookup for {slug} in {channel} returned {}",
            resp.status()
        );
    }
    resp.json()
        .await
        .with_context(|| format!("parsing kick user response for {slug} in {channel}"))
}

/// Issues a GET to the web.kick.com chat-history endpoint for a known numeric
/// chat id (the v2 `chatroom.channel_id`) via the emulated client. Returns the raw
/// response body text. Used by [`crate::history`].
pub(crate) async fn fetch_history_body(channel_id: u64) -> anyhow::Result<String> {
    let resp = kick_get(format!("{HISTORY_BASE}{channel_id}/history"))
        .await
        .with_context(|| format!("fetching kick history for channel_id {channel_id}"))?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "kick history lookup for {channel_id} returned {}",
            resp.status()
        );
    }
    resp.text().await.context("reading kick history body")
}

/// Resolves a channel slug to its history chat id (the v2 `chatroom.channel_id`,
/// distinct from the Pusher `chatroom.id`) for the history fallback path.
pub(crate) async fn fetch_history_channel_id(slug: &str) -> anyhow::Result<u64> {
    Ok(fetch_raw_channel(slug).await?.chatroom.channel_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Timelike};

    #[test]
    fn parses_channel_json_fields() {
        let raw: RawChannel = serde_json::from_str(
            r#"{
                "user_id": 676,
                "chatroom": { "id": 12, "channel_id": 668 },
                "subscriber_badges": [
                    { "months": 3, "badge_image": { "src": "https://x/3.png" } },
                    { "months": 6, "badge_image": { "src": "" } }
                ],
                "livestream": {
                    "is_live": true,
                    "session_title": "hi",
                    "start_time": "2026-06-28 12:00:56",
                    "categories": [ { "name": "Just Chatting" } ]
                }
            }"#,
        )
        .unwrap();
        assert_eq!(raw.user_id, 676);
        assert_eq!(raw.chatroom.id, 12);
        assert_eq!(raw.chatroom.channel_id, 668);
        assert_eq!(raw.subscriber_badges.len(), 2);
        let live = raw.livestream.unwrap();
        assert!(live.is_live);
        assert_eq!(live.categories[0].name, "Just Chatting");
    }

    #[test]
    fn viewer_count_reads_live_and_offline_shapes() {
        // The real /livestream shape (verified live): the count is `viewers`.
        let live: serde_json::Value =
            serde_json::from_str(r#"{ "data": { "id": 5, "viewers": 531 } }"#).unwrap();
        assert_eq!(viewer_count_from(&live), Some(Some(531)));
        // The channel endpoint's embedded-livestream name, accepted as fallback.
        let alt: serde_json::Value =
            serde_json::from_str(r#"{ "data": { "id": 5, "viewer_count": 103 } }"#).unwrap();
        assert_eq!(viewer_count_from(&alt), Some(Some(103)));
        // data null = offline (a real clear).
        let offline: serde_json::Value = serde_json::from_str(r#"{ "data": null }"#).unwrap();
        assert_eq!(viewer_count_from(&offline), Some(None));
        // Live but the count is null/absent = unknown (an error upstream, so the
        // poll keeps the previous number instead of blanking a live stream).
        let no_count: serde_json::Value =
            serde_json::from_str(r#"{ "data": { "id": 5, "viewers": null } }"#).unwrap();
        assert_eq!(viewer_count_from(&no_count), None);
    }

    #[test]
    fn offline_channel_has_null_livestream() {
        let raw: RawChannel = serde_json::from_str(
            r#"{ "user_id": 1, "chatroom": { "id": 2 }, "livestream": null }"#,
        )
        .unwrap();
        assert!(raw.livestream.is_none());
        // channel_id absent → defaults to 0.
        assert_eq!(raw.chatroom.channel_id, 0);
    }

    #[test]
    fn parses_kick_space_separated_utc() {
        // Kick's `livestream.start_time`: "YYYY-MM-DD HH:MM:SS", no zone, UTC.
        let dt = parse_kick_time("2026-06-28 12:00:56").expect("should parse");
        assert_eq!((dt.year(), dt.month(), dt.day()), (2026, 6, 28));
        assert_eq!((dt.hour(), dt.minute(), dt.second()), (12, 0, 56));
    }

    #[test]
    fn parses_rfc3339_fallback() {
        // A future format change to a proper RFC-3339 value still parses.
        let dt = parse_kick_time("2026-06-28T12:00:56Z").expect("should parse");
        assert_eq!(dt.hour(), 12);
    }

    #[test]
    fn empty_or_bad_is_none() {
        assert!(parse_kick_time("").is_none());
        assert!(parse_kick_time("not a date").is_none());
    }

    #[test]
    fn emote_set_classification() {
        let sets: Vec<RawEmoteSet> = serde_json::from_str(
            r#"[
                { "user_id": 5, "emotes": [ { "id": 1, "name": "chanEmote" } ] },
                { "id": "Global", "emotes": [ { "id": 2, "name": "globalEmote" } ] },
                { "id": "Emoji", "emotes": [ { "id": 3, "name": ":smile:" } ] }
            ]"#,
        )
        .unwrap();
        assert!(sets[0].user_id.is_some());
        assert_eq!(sets[1].id.as_ref().unwrap().as_str(), Some("Global"));
        assert_eq!(sets[2].id.as_ref().unwrap().as_str(), Some("Emoji"));
    }
}
