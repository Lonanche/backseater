//! IVR API (`api.ivr.fi`) — a public, unauthenticated Twitch data service.
//!
//! Twitch's own Helix "follow age" + "sub age" endpoints need a moderator token
//! scoped to the *target's* channel, which a chat client doesn't have. IVR is a
//! community API that exposes the same public data with no auth. We use one
//! call — `subage/{user}/{channel}` — for the usercard's
//! "following since" + subscription tenure lines.

use anyhow::Context;
use serde::Deserialize;

const BASE: &str = "https://api.ivr.fi/v2";

/// A chatter's follow + subscription standing in a channel, as IVR reports it.
#[derive(Clone, Debug, Default)]
pub struct SubAge {
    /// RFC-3339 timestamp they started following, or `None` if not following.
    pub following_since: Option<String>,
    /// The channel hides this user's sub status (still a real state to show).
    pub status_hidden: bool,
    /// Currently subscribed (their `meta` is present).
    pub subscribed: bool,
    /// Sub tier as IVR's string ("1"/"2"/"3"), when currently subscribed.
    pub tier: Option<String>,
    /// Cumulative months subscribed (counts past tenure too), 0 if never.
    pub total_months: u64,
}

/// IVR's `subage` response. `meta` is present only while currently subscribed;
/// `cumulative.months` is the lifetime month count (kept even after a lapse).
#[derive(Deserialize)]
struct SubAgeResponse {
    #[serde(default, rename = "statusHidden")]
    status_hidden: bool,
    #[serde(default, rename = "followedAt")]
    followed_at: Option<String>,
    #[serde(default)]
    meta: Option<SubMeta>,
    #[serde(default)]
    cumulative: Option<Cumulative>,
}

#[derive(Deserialize)]
struct SubMeta {
    #[serde(default)]
    tier: Option<String>,
}

#[derive(Deserialize)]
struct Cumulative {
    #[serde(default)]
    months: u64,
}

/// A channel's current live status, as IVR's `/twitch/user` reports it: whether
/// it's broadcasting and (when live) the stream title + start time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveStatus {
    pub live: bool,
    pub title: String,
    /// The game/category being streamed, empty when offline or unknown.
    pub game: String,
    /// When the stream started (for an uptime readout), parsed from IVR's
    /// `createdAt`. `None` when offline or the timestamp was missing/unparseable.
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When the most recent past broadcast started (IVR's `lastBroadcast`), for
    /// the offline "last live X ago" tooltip line. Only set when offline — while
    /// live, `lastBroadcast` describes the *current* stream. IVR reports no
    /// duration or category for it.
    pub last_started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The most recent past broadcast's title. Empty when live or unknown.
    pub last_title: String,
}

/// IVR's `/twitch/user` response (the subset we need). `stream` is present only
/// while broadcasting, so its presence *is* the live flag; `lastBroadcast` holds
/// the newest broadcast (the current one while live, the previous one after).
#[derive(Deserialize)]
struct UserResponse {
    #[serde(default)]
    stream: Option<StreamInfo>,
    #[serde(default, rename = "lastBroadcast")]
    last_broadcast: Option<LastBroadcast>,
}

#[derive(Deserialize)]
struct LastBroadcast {
    #[serde(default, rename = "startedAt")]
    started_at: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

#[derive(Deserialize)]
struct StreamInfo {
    #[serde(default)]
    title: String,
    /// The category/game — IVR nests it as an object (`{"displayName": "..."}`),
    /// not a string. Absent/null when the stream has no category set.
    #[serde(default)]
    game: Option<Game>,
    #[serde(default, rename = "createdAt")]
    created_at: Option<String>,
}

#[derive(Deserialize)]
struct Game {
    #[serde(default, rename = "displayName")]
    display_name: String,
}

/// Fetches `channel`'s current live status from IVR (no auth). The endpoint
/// returns a list; the single requested login is its first (and only) entry.
pub async fn fetch_live_status(channel: &str) -> anyhow::Result<LiveStatus> {
    let login = bks_core::channel_login(channel);
    let resp: Vec<UserResponse> = crate::http::client()
        .get(format!("{BASE}/twitch/user"))
        .query(&[("login", login.as_str())])
        .send()
        .await
        .context("requesting IVR user")?
        .error_for_status()
        .context("IVR user request failed")?
        .json()
        .await
        .context("parsing IVR user response")?;

    Ok(status_from_users(resp))
}

/// Maps IVR's `/twitch/user` list to a [`LiveStatus`]: the first entry's `stream`
/// is the live state (present only while broadcasting); offline, `lastBroadcast`
/// fills the last-live fields. Pure, so the JSON shape (notably the nested `game`
/// object) is unit-testable without the network.
fn status_from_users(resp: Vec<UserResponse>) -> LiveStatus {
    let Some(user) = resp.into_iter().next() else {
        return LiveStatus::default();
    };
    match user.stream {
        Some(s) => LiveStatus {
            live: true,
            title: s.title,
            game: s.game.map(|g| g.display_name).unwrap_or_default(),
            started_at: s.created_at.as_deref().and_then(bks_core::parse_rfc3339),
            ..LiveStatus::default()
        },
        None => {
            let (last_started_at, last_title) = user
                .last_broadcast
                .map(|lb| {
                    (
                        lb.started_at.as_deref().and_then(bks_core::parse_rfc3339),
                        lb.title.unwrap_or_default(),
                    )
                })
                .unwrap_or_default();
            LiveStatus {
                last_started_at,
                last_title,
                ..LiveStatus::default()
            }
        }
    }
}

/// Fetches `user`'s follow + sub standing in `channel` from IVR (no auth). Both
/// are logins; IVR resolves them itself.
pub async fn fetch_subage(user: &str, channel: &str) -> anyhow::Result<SubAge> {
    let user = bks_core::encode_url_component(&bks_core::channel_login(user));
    let channel = bks_core::encode_url_component(&bks_core::channel_login(channel));
    let resp: SubAgeResponse = crate::http::client()
        .get(format!("{BASE}/twitch/subage/{user}/{channel}"))
        .send()
        .await
        .context("requesting IVR subage")?
        .error_for_status()
        .context("IVR subage request failed")?
        .json()
        .await
        .context("parsing IVR subage response")?;

    Ok(SubAge {
        following_since: resp.followed_at.filter(|s| !s.is_empty()),
        status_hidden: resp.status_hidden,
        subscribed: resp.meta.is_some(),
        tier: resp.meta.and_then(|m| m.tier).filter(|t| !t.is_empty()),
        total_months: resp.cumulative.map(|c| c.months).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> LiveStatus {
        status_from_users(serde_json::from_str(json).unwrap())
    }

    #[test]
    fn live_stream_reads_nested_game_object() {
        // Real IVR shape: `game` is an object (`{"displayName": ...}`), not a string.
        let json = r#"[{"stream":{"title":"SOLOQ A LIL :)","createdAt":"2026-06-28T08:26:22Z","game":{"displayName":"League of Legends"}}}]"#;
        let s = parse(json);
        assert!(s.live);
        assert_eq!(s.title, "SOLOQ A LIL :)");
        assert_eq!(s.game, "League of Legends");
        assert!(s.started_at.is_some());
    }

    #[test]
    fn live_stream_without_game_leaves_category_empty() {
        let json = r#"[{"stream":{"title":"just chatting offline category?","createdAt":"2026-06-28T08:26:22Z"}}]"#;
        let s = parse(json);
        assert!(s.live);
        assert_eq!(s.game, "");
    }

    #[test]
    fn null_stream_is_offline() {
        assert_eq!(parse(r#"[{"stream":null}]"#), LiveStatus::default());
        assert_eq!(parse("[]"), LiveStatus::default());
    }

    #[test]
    fn offline_reads_last_broadcast() {
        let json = r#"[{"stream":null,"lastBroadcast":{"startedAt":"2026-06-28T08:26:22Z","title":"yesterday's stream"}}]"#;
        let s = parse(json);
        assert!(!s.live);
        assert!(s.last_started_at.is_some());
        assert_eq!(s.last_title, "yesterday's stream");
    }

    #[test]
    fn live_ignores_last_broadcast() {
        // While live, `lastBroadcast` is the *current* stream — not a past one.
        let json = r#"[{"stream":{"title":"live now","createdAt":"2026-06-28T08:26:22Z"},"lastBroadcast":{"startedAt":"2026-06-28T08:26:22Z","title":"live now"}}]"#;
        let s = parse(json);
        assert!(s.live);
        assert_eq!(s.last_started_at, None);
        assert_eq!(s.last_title, "");
    }

    #[test]
    fn never_streamed_last_broadcast_is_null_fields() {
        let json = r#"[{"stream":null,"lastBroadcast":{"startedAt":null,"title":null}}]"#;
        assert_eq!(parse(json), LiveStatus::default());
    }
}
