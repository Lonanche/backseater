//! Moderator event feed over Twitch **EventSub** (WebSocket transport).
//!
//! IRC's CLEARCHAT carries only the target + duration — never *who* acted — and
//! there is no IRC unban event at all. When the logged-in user moderates the
//! channel, EventSub's `channel.moderate` (v2) delivers every moderation action
//! *with* the acting moderator (ban/timeout/unban/delete/clear/warn/slow-mode/
//! raids/mod-vip grants/...), which we format into rich [`ChatEvent::Notice`]
//! rows like the Kick connector's ("mod timed out user for 10m: reason").
//! `automod.message.hold`/`.update` (v2) additionally surface messages AutoMod
//! held for review as [`ChatEvent::AutoModHeld`]/[`AutoModResolved`] so the UI
//! can offer Allow/Deny. `channel.suspicious_user.message`/`.update` (v1)
//! surface Twitch's "Low Trust" feature: a monitored/restricted user's message
//! arrives as [`ChatEvent::Suspicious`] (the UI marks/inserts it), a treatment
//! change as a plain notice.
//!
//! Flow: connect to `wss://eventsub.wss.twitch.tv/ws`, read the welcome frame's
//! session id, then create the subscriptions over Helix
//! (`POST /eventsub/subscriptions`, websocket transport, user token — cost 0
//! when the token's user matches the condition's `moderator_user_id`). A 403
//! means "not a moderator here" — normal, the feed just stays off. Twitch drops
//! the socket if a keepalive is missed, and we treat a quiet socket the same way.

use anyhow::{bail, Context};
use bks_core::Platform;
use bks_platform::{AutoModStatus, ChatEvent, SuspiciousStatus};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

pub(crate) const EVENTSUB_URL: &str = "wss://eventsub.wss.twitch.tv/ws";
pub(crate) const SUBSCRIPTIONS_URL: &str = "https://api.twitch.tv/helix/eventsub/subscriptions";

/// What the EventSub feed needs from the logged-in session: the app id + user
/// token for the Helix subscription calls, the user's id (the `moderator_user_id`
/// condition), and the token's granted scopes (checked locally so we don't ask
/// Twitch for subscriptions an old token can't have).
#[derive(Clone)]
pub struct EventsubAuth {
    pub client_id: String,
    pub token: String,
    pub user_id: String,
    pub scopes: Vec<String>,
}

impl EventsubAuth {
    /// Whether this token can power any part of the moderator feed. `false`
    /// means don't even connect (an old token from before the scopes were added
    /// — a fresh `/login` picks them up).
    pub fn feed_available(&self) -> bool {
        can_moderate_feed(&self.scopes) || can_automod(&self.scopes) || can_suspicious(&self.scopes)
    }

    /// Whether the token carries the `channel.moderate` scope set. Public so
    /// the app can assert its login tiers actually cover the feed (the scope
    /// lists live in two crates; a test ties them together).
    pub fn wants_moderate(&self) -> bool {
        can_moderate_feed(&self.scopes)
    }

    /// Whether the token carries the AutoMod scope.
    pub fn wants_automod(&self) -> bool {
        can_automod(&self.scopes)
    }

    /// Whether the token carries the suspicious-user (Low Trust) scope.
    pub fn wants_suspicious(&self) -> bool {
        can_suspicious(&self.scopes)
    }
}

/// `channel.moderate` v2 wants the full read-or-manage scope set below; a token
/// missing any of them gets a 403 from the subscription call.
fn can_moderate_feed(scopes: &[String]) -> bool {
    let has = |s: &str| scopes.iter().any(|x| x == s);
    let any = |read: &str, manage: &str| has(read) || has(manage);
    any(
        "moderator:read:blocked_terms",
        "moderator:manage:blocked_terms",
    ) && any(
        "moderator:read:chat_settings",
        "moderator:manage:chat_settings",
    ) && any(
        "moderator:read:unban_requests",
        "moderator:manage:unban_requests",
    ) && any(
        "moderator:read:banned_users",
        "moderator:manage:banned_users",
    ) && any(
        "moderator:read:chat_messages",
        "moderator:manage:chat_messages",
    ) && any("moderator:read:warnings", "moderator:manage:warnings")
        && has("moderator:read:moderators")
        && has("moderator:read:vips")
}

/// `automod.message.hold`/`.update` (and the approve/deny Helix call) need
/// `moderator:manage:automod`.
fn can_automod(scopes: &[String]) -> bool {
    scopes.iter().any(|s| s == "moderator:manage:automod")
}

/// `channel.suspicious_user.message`/`.update` need
/// `moderator:read:suspicious_users`.
fn can_suspicious(scopes: &[String]) -> bool {
    scopes.iter().any(|s| s == "moderator:read:suspicious_users")
}

/// The envelope every EventSub WebSocket frame shares.
#[derive(Deserialize)]
pub(crate) struct Frame {
    pub(crate) metadata: Metadata,
    #[serde(default)]
    pub(crate) payload: Value,
}

#[derive(Deserialize)]
pub(crate) struct Metadata {
    pub(crate) message_type: String,
}

/// The outcome of a single subscription-create call.
pub(crate) enum SubResult {
    /// Created — carries the subscription id (for later deletion).
    Created(String),
    /// Twitch declined in a *normal* way (401/403: not a moderator there, or the
    /// token has gone stale) — the feed stays off for this channel; don't retry.
    Declined,
}

/// Creates one websocket-transport subscription. `Ok(SubResult)` on a definitive
/// answer (created / normally declined); `Err` on anything transient worth a
/// reconnect (network, `session_id` gone stale) — *except* the transport-limit
/// 429, which is fatal at the socket level (see [`is_transport_limit`]).
pub(crate) async fn subscribe(
    client: &reqwest::Client,
    auth: &EventsubAuth,
    session_id: &str,
    broadcaster_id: &str,
    sub_type: &str,
    version: &str,
) -> anyhow::Result<SubResult> {
    let body = serde_json::json!({
        "type": sub_type,
        "version": version,
        "condition": {
            "broadcaster_user_id": broadcaster_id,
            "moderator_user_id": auth.user_id,
        },
        "transport": { "method": "websocket", "session_id": session_id },
    });
    let resp = client
        .post(SUBSCRIPTIONS_URL)
        .header("Client-Id", &auth.client_id)
        .bearer_auth(&auth.token)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("subscribing to {sub_type}"))?;
    let status = resp.status();
    if status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let id = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v["data"][0]["id"].as_str().map(str::to_string))
            .unwrap_or_default();
        return Ok(SubResult::Created(id));
    }
    let text = resp.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::UNAUTHORIZED {
        tracing::info!("EventSub {sub_type} not available ({status}): {text}");
        return Ok(SubResult::Declined);
    }
    bail!("EventSub {sub_type} subscription failed ({status}): {text}")
}

/// A subscription-create error is the transport-limit 429 — Twitch allows only 3
/// WebSocket connections with enabled subscriptions per (client id, user id).
/// This is fatal for the socket, not the channel: the manager multiplexes all
/// channels onto one socket precisely to stay under that cap, so hitting it means
/// something is wrong, and retrying opens *another* socket that makes it worse.
pub(crate) fn is_transport_limit(err: &anyhow::Error) -> bool {
    err.to_string().contains("websocket transports limit exceeded")
}

/// Deletes a subscription by id (best effort — used when a channel unregisters so
/// its slots free up for others). Failures are logged, not surfaced.
pub(crate) async fn delete_subscription(
    client: &reqwest::Client,
    auth: &EventsubAuth,
    subscription_id: &str,
) {
    if subscription_id.is_empty() {
        return;
    }
    let result = client
        .delete(SUBSCRIPTIONS_URL)
        .header("Client-Id", &auth.client_id)
        .bearer_auth(&auth.token)
        .query(&[("id", subscription_id)])
        .send()
        .await;
    if let Err(err) = result {
        tracing::debug!("EventSub delete subscription {subscription_id} failed: {err:#}");
    }
}

/// Maps one notification to the chat events it produces. `now` is passed in so
/// timeout durations (derived from `expires_at`) are testable.
pub(crate) fn notification_events(
    sub_type: &str,
    event: &Value,
    now: DateTime<Utc>,
) -> Vec<ChatEvent> {
    match sub_type {
        "channel.moderate" => moderate_notice(event, now)
            .map(ChatEvent::Notice)
            .into_iter()
            .collect(),
        "automod.message.hold" => automod_held(event).into_iter().collect(),
        "automod.message.update" => automod_resolved(event).into_iter().collect(),
        "channel.suspicious_user.message" => suspicious_message(event, now).into_iter().collect(),
        "channel.suspicious_user.update" => suspicious_update(event)
            .map(ChatEvent::Notice)
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

/// The acting moderator's display name (falling back to login).
fn moderator_name(event: &Value) -> String {
    name_of(event, "moderator_user_name", "moderator_user_login")
}

fn name_of(obj: &Value, name_key: &str, login_key: &str) -> String {
    obj[name_key]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| obj[login_key].as_str())
        .unwrap_or("someone")
        .to_string()
}

/// The target chatter named inside an action's metadata object.
fn target_name(obj: &Value) -> String {
    name_of(obj, "user_name", "user_login")
}

/// Appends `: reason` when the action carries a non-empty reason.
fn with_reason(mut text: String, obj: &Value) -> String {
    if let Some(reason) = obj["reason"].as_str() {
        let reason = reason.trim();
        if !reason.is_empty() {
            text.push_str(": ");
            text.push_str(reason);
        }
    }
    text
}

/// Formats a `channel.moderate` (v2) event into one human notice line, in the
/// same voice as the Kick connector's ("mod timed out user for 1h30m: reason").
/// Shared-chat variants (`shared_chat_ban`, ...) read the same as their plain
/// counterparts. `None` for actions not worth a chat row.
fn moderate_notice(event: &Value, now: DateTime<Utc>) -> Option<String> {
    let moderator = moderator_name(event);
    let action = event["action"].as_str()?;
    let base = action.strip_prefix("shared_chat_").unwrap_or(action);
    // Each action's metadata rides in a field usually named after the action
    // itself; the term/unban-request families share container names.
    let container = match base {
        "add_blocked_term"
        | "remove_blocked_term"
        | "add_permitted_term"
        | "remove_permitted_term" => "automod_terms",
        "approve_unban_request" | "deny_unban_request" => "unban_request",
        _ => action,
    };
    let obj = &event[container];

    let text = match base {
        "ban" => with_reason(format!("{moderator} banned {}", target_name(obj)), obj),
        "timeout" => {
            let mut text = format!("{moderator} timed out {}", target_name(obj));
            if let Some(secs) = expires_in_secs(obj["expires_at"].as_str(), now) {
                text.push_str(&format!(" for {}", format_secs(secs)));
            }
            with_reason(text, obj)
        }
        "unban" => format!("{moderator} unbanned {}", target_name(obj)),
        "untimeout" => format!("{moderator} removed the timeout on {}", target_name(obj)),
        "delete" => {
            let mut text = format!("{moderator} deleted a message by {}", target_name(obj));
            if let Some(body) = obj["message_body"].as_str().filter(|s| !s.is_empty()) {
                text.push_str(&format!(": {}", clip(body, 80)));
            }
            text
        }
        "clear" => format!("{moderator} cleared chat"),
        "warn" => with_reason(format!("{moderator} warned {}", target_name(obj)), obj),
        "raid" => format!("{moderator} started a raid to {}", target_name(obj)),
        "unraid" => format!("{moderator} canceled the raid to {}", target_name(obj)),
        "mod" => format!("{moderator} granted moderator to {}", target_name(obj)),
        "unmod" => format!("{moderator} removed moderator from {}", target_name(obj)),
        "vip" => format!("{moderator} granted VIP to {}", target_name(obj)),
        "unvip" => format!("{moderator} removed VIP from {}", target_name(obj)),
        "slow" => match obj["wait_time_seconds"].as_u64() {
            Some(secs) => format!("{moderator} enabled slow mode ({}s)", secs),
            None => format!("{moderator} enabled slow mode"),
        },
        "slowoff" => format!("{moderator} disabled slow mode"),
        "emoteonly" => format!("{moderator} enabled emote-only mode"),
        "emoteonlyoff" => format!("{moderator} disabled emote-only mode"),
        "followers" => match obj["follow_duration_minutes"].as_u64() {
            Some(mins) if mins > 0 => {
                format!(
                    "{moderator} enabled followers-only mode ({})",
                    format_secs(mins * 60)
                )
            }
            _ => format!("{moderator} enabled followers-only mode"),
        },
        "followersoff" => format!("{moderator} disabled followers-only mode"),
        "uniquechat" => format!("{moderator} enabled unique-chat mode"),
        "uniquechatoff" => format!("{moderator} disabled unique-chat mode"),
        "subscribers" => format!("{moderator} enabled subscribers-only mode"),
        "subscribersoff" => format!("{moderator} disabled subscribers-only mode"),
        "add_blocked_term" => format!("{moderator} added blocked {}", terms(obj)),
        "remove_blocked_term" => format!("{moderator} removed blocked {}", terms(obj)),
        "add_permitted_term" => format!("{moderator} added permitted {}", terms(obj)),
        "remove_permitted_term" => format!("{moderator} removed permitted {}", terms(obj)),
        "approve_unban_request" => {
            format!("{moderator} approved {}'s unban request", target_name(obj))
        }
        "deny_unban_request" => {
            format!("{moderator} denied {}'s unban request", target_name(obj))
        }
        other => {
            tracing::debug!("unhandled channel.moderate action: {other}");
            return None;
        }
    };
    Some(text)
}

/// "term “x”" / "terms “x”, “y”" for the blocked/permitted-term actions.
fn terms(obj: &Value) -> String {
    let list: Vec<&str> = obj["terms"]
        .as_array()
        .map(|a| a.iter().filter_map(|t| t.as_str()).collect())
        .unwrap_or_default();
    match list.as_slice() {
        [] => "term".to_string(),
        [one] => format!("term “{one}”"),
        many => format!(
            "terms {}",
            many.iter()
                .map(|t| format!("“{t}”"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Seconds until `expires_at` (RFC 3339), rounded — the timeout's remaining =
/// full duration, since the event arrives as the timeout starts. `None` when
/// absent/unparsable/past.
fn expires_in_secs(expires_at: Option<&str>, now: DateTime<Utc>) -> Option<u64> {
    let expires = DateTime::parse_from_rfc3339(expires_at?).ok()?;
    let ms = (expires.with_timezone(&Utc) - now).num_milliseconds();
    if ms <= 0 {
        return None;
    }
    Some(((ms as f64) / 1000.0).round() as u64)
}

/// Compact duration: the two most significant units ("30s", "10m", "1h30m",
/// "1d2h"), matching the Kick connector's timeout notices.
fn format_secs(total: u64) -> String {
    let (d, h, m, s) = (
        total / 86_400,
        (total % 86_400) / 3_600,
        (total % 3_600) / 60,
        total % 60,
    );
    let parts = [(d, 'd'), (h, 'h'), (m, 'm'), (s, 's')];
    let first = parts.iter().position(|(n, _)| *n > 0);
    match first {
        None => "0s".to_string(),
        Some(i) => parts[i..]
            .iter()
            .take(2)
            .filter(|(n, _)| *n > 0)
            .map(|(n, u)| format!("{n}{u}"))
            .collect(),
    }
}

/// Truncates to `max` chars with an ellipsis.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// An `automod.message.hold` (v2) notification → [`ChatEvent::AutoModHeld`].
fn automod_held(event: &Value) -> Option<ChatEvent> {
    let message_id = event["message_id"].as_str()?.to_string();
    let text = event["message"]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let reason = hold_reason(event);
    let timestamp = event["held_at"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    Some(ChatEvent::AutoModHeld {
        platform: Platform::Twitch,
        message_id,
        user: name_of(event, "user_name", "user_login"),
        text,
        reason,
        timestamp,
    })
}

/// "automod: swearing, level 4" / "blocked term" — why the message was held.
fn hold_reason(event: &Value) -> String {
    match event["reason"].as_str() {
        Some("blocked_term") => "blocked term".to_string(),
        _ => {
            let automod = &event["automod"];
            match (automod["category"].as_str(), automod["level"].as_u64()) {
                (Some(category), Some(level)) => format!("automod: {category}, level {level}"),
                (Some(category), None) => format!("automod: {category}"),
                _ => "automod".to_string(),
            }
        }
    }
}

/// An `automod.message.update` (v2) notification → [`ChatEvent::AutoModResolved`].
fn automod_resolved(event: &Value) -> Option<ChatEvent> {
    let message_id = event["message_id"].as_str()?.to_string();
    let status = match event["status"].as_str()?.to_ascii_lowercase().as_str() {
        "approved" => AutoModStatus::Approved,
        "denied" => AutoModStatus::Denied,
        "expired" => AutoModStatus::Expired,
        _ => return None,
    };
    let moderator = if status == AutoModStatus::Expired {
        String::new()
    } else {
        moderator_name(event)
    };
    Some(ChatEvent::AutoModResolved {
        platform: Platform::Twitch,
        message_id,
        status,
        moderator,
    })
}

/// A `channel.suspicious_user.message` (v1) notification → [`ChatEvent::Suspicious`].
/// The payload carries the whole message (id + text + native-emote fragments),
/// rebuilt here into a [`bks_core::Message`] — for a *restricted* user this copy
/// is the only one that exists (Twitch withholds their chat from the normal read
/// connection); for a *monitored* user it just marks the IRC copy.
fn suspicious_message(event: &Value, now: DateTime<Utc>) -> Option<ChatEvent> {
    let status = match event["low_trust_status"].as_str()? {
        "restricted" => SuspiciousStatus::Restricted,
        "active_monitoring" => SuspiciousStatus::Monitored,
        other => {
            tracing::debug!("suspicious_user.message with unhandled status {other}");
            return None;
        }
    };
    let body = &event["message"];
    let id = body["message_id"].as_str()?.to_string();
    let text = body["text"].as_str().unwrap_or_default().to_string();
    let login = event["user_login"].as_str().unwrap_or_default().to_string();

    // Fragments → elements like the pinned-message parse: native emotes inline,
    // everything else (text/cheermotes/mentions) as its literal text.
    let mut elements: Vec<bks_core::MessageElement> = Vec::new();
    if let Some(fragments) = body["fragments"].as_array() {
        for fragment in fragments {
            let frag_text = fragment["text"].as_str().unwrap_or_default();
            match fragment["emote"]["id"].as_str() {
                Some(emote_id) if !emote_id.is_empty() => {
                    elements.push(bks_core::MessageElement::Emote(std::sync::Arc::new(
                        bks_core::Emote {
                            url: format!("{}/{emote_id}/default/dark/2.0", crate::helix::EMOTE_CDN),
                            id: emote_id.to_string(),
                            name: frag_text.to_string(),
                            animated: false,
                            tooltip: bks_core::EmoteTooltip::provider("Twitch"),
                        },
                    )));
                }
                _ if !frag_text.is_empty() => {
                    elements.push(bks_core::MessageElement::Text {
                        text: frag_text.to_string(),
                        color: None,
                    });
                }
                _ => {}
            }
        }
    }
    if elements.is_empty() && !text.is_empty() {
        elements.push(bks_core::MessageElement::Text {
            text: text.clone(),
            color: None,
        });
    }

    let message = bks_core::Message {
        id,
        platform: Platform::Twitch,
        channel: event["broadcaster_user_login"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        // The notification has no send time; delivery is within a beat of it.
        timestamp: now,
        author: bks_core::Author {
            display_name: name_of(event, "user_name", "user_login"),
            login,
            // The payload carries no name color — the UI's stable per-user
            // fallback color applies, same as a colorless IRC chatter.
            color: None,
            badges: Vec::new(),
            paint: None,
            user_id: event["user_id"].as_str().unwrap_or_default().to_string(),
        },
        raw_text: text,
        elements: bks_core::mentionize(bks_core::linkify(elements)),
        reply: None,
        first_message: false,
        highlighted: false,
        historical: false,
        reward_id: None,
    };
    Some(ChatEvent::Suspicious {
        platform: Platform::Twitch,
        status,
        detail: suspicious_detail(event),
        message: Box::new(message),
    })
}

/// The extra context Twitch attaches to a suspicious user: ban-evasion
/// assessment and/or shared-channel bans. Empty when manually flagged with
/// neither ("" — the status label alone tells the story).
fn suspicious_detail(event: &Value) -> String {
    let types: Vec<&str> = event["types"]
        .as_array()
        .map(|a| a.iter().filter_map(|t| t.as_str()).collect())
        .unwrap_or_default();
    let mut parts: Vec<String> = Vec::new();
    if types.contains(&"ban_evader") {
        match event["ban_evasion_evaluation"].as_str() {
            Some(eval @ ("likely" | "possible")) => parts.push(format!("{eval} ban evader")),
            _ => parts.push("ban evader".to_string()),
        }
    }
    if types.contains(&"banned_in_shared_channel") {
        let n = event["shared_ban_channel_ids"]
            .as_array()
            .map_or(0, |a| a.len());
        match n {
            0 => parts.push("banned in a shared channel".to_string()),
            n => parts.push(format!(
                "banned in {n} shared {}",
                bks_core::plural(n as u64, "channel", "channels")
            )),
        }
    }
    parts.join(" · ")
}

/// A `channel.suspicious_user.update` (v1) notification (a mod changed someone's
/// treatment) → a notice line in the moderator feed's voice.
fn suspicious_update(event: &Value) -> Option<String> {
    let moderator = moderator_name(event);
    let user = target_name(event);
    Some(match event["low_trust_status"].as_str()? {
        "restricted" => format!("{moderator} restricted {user} as a suspicious user"),
        "active_monitoring" => format!("{moderator} started monitoring {user} as a suspicious user"),
        "none" | "no_treatment" => {
            format!("{moderator} removed {user}'s suspicious-user treatment")
        }
        other => {
            tracing::debug!("suspicious_user.update with unhandled status {other}");
            return None;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn notice(event: Value) -> Option<String> {
        moderate_notice(&event, Utc::now())
    }

    #[test]
    fn format_secs_picks_two_units() {
        assert_eq!(format_secs(30), "30s");
        assert_eq!(format_secs(600), "10m");
        assert_eq!(format_secs(5400), "1h30m");
        assert_eq!(format_secs(3601), "1h");
        assert_eq!(format_secs(90_000), "1d1h");
        assert_eq!(format_secs(0), "0s");
    }

    #[test]
    fn timeout_notice_has_moderator_duration_and_reason() {
        let now = Utc::now();
        let expires = (now + chrono::Duration::seconds(600)).to_rfc3339();
        let event = json!({
            "moderator_user_name": "StreamMod",
            "action": "timeout",
            "timeout": {
                "user_name": "BadUser",
                "reason": "spam",
                "expires_at": expires,
            },
        });
        assert_eq!(
            moderate_notice(&event, now),
            Some("StreamMod timed out BadUser for 10m: spam".to_string())
        );
    }

    #[test]
    fn ban_without_reason_omits_the_colon() {
        let event = json!({
            "moderator_user_name": "StreamMod",
            "action": "ban",
            "ban": { "user_name": "BadUser", "reason": null },
        });
        assert_eq!(notice(event), Some("StreamMod banned BadUser".to_string()));
    }

    #[test]
    fn unban_and_untimeout_notices() {
        let unban = json!({
            "moderator_user_name": "StreamMod",
            "action": "unban",
            "unban": { "user_name": "BadUser" },
        });
        assert_eq!(
            notice(unban),
            Some("StreamMod unbanned BadUser".to_string())
        );
        let untimeout = json!({
            "moderator_user_name": "StreamMod",
            "action": "untimeout",
            "untimeout": { "user_name": "BadUser" },
        });
        assert_eq!(
            notice(untimeout),
            Some("StreamMod removed the timeout on BadUser".to_string())
        );
    }

    #[test]
    fn delete_notice_clips_the_body() {
        let event = json!({
            "moderator_user_name": "StreamMod",
            "action": "delete",
            "delete": {
                "user_name": "Chatter",
                "message_id": "abc",
                "message_body": "x".repeat(100),
            },
        });
        let text = notice(event).unwrap();
        assert!(text.starts_with("StreamMod deleted a message by Chatter: "));
        assert!(text.ends_with('…'));
    }

    #[test]
    fn shared_chat_action_reads_like_the_plain_one() {
        // Shared-chat variants nest their metadata under the *full* action name.
        let event = json!({
            "moderator_user_name": "StreamMod",
            "action": "shared_chat_ban",
            "shared_chat_ban": { "user_name": "BadUser", "reason": "rude" },
        });
        assert_eq!(
            notice(event),
            Some("StreamMod banned BadUser: rude".to_string())
        );
    }

    #[test]
    fn slow_mode_and_terms_notices() {
        let slow = json!({
            "moderator_user_name": "StreamMod",
            "action": "slow",
            "slow": { "wait_time_seconds": 30 },
        });
        assert_eq!(
            notice(slow),
            Some("StreamMod enabled slow mode (30s)".to_string())
        );
        let terms = json!({
            "moderator_user_name": "StreamMod",
            "action": "add_blocked_term",
            "automod_terms": { "action": "add", "list": "blocked", "terms": ["crac", "crac2"] },
        });
        assert_eq!(
            notice(terms),
            Some("StreamMod added blocked terms “crac”, “crac2”".to_string())
        );
    }

    #[test]
    fn moderator_falls_back_to_login() {
        let event = json!({
            "moderator_user_name": "",
            "moderator_user_login": "streammod",
            "action": "clear",
            "clear": null,
        });
        assert_eq!(notice(event), Some("streammod cleared chat".to_string()));
    }

    #[test]
    fn unknown_action_is_skipped() {
        let event = json!({
            "moderator_user_name": "StreamMod",
            "action": "some_future_action",
        });
        assert_eq!(notice(event), None);
    }

    #[test]
    fn automod_hold_becomes_held_event() {
        let event = json!({
            "user_name": "Chatter",
            "user_login": "chatter",
            "message_id": "msg-1",
            "message": { "text": "bad words here" },
            "reason": "automod",
            "automod": { "category": "swearing", "level": 4 },
            "held_at": "2026-07-03T12:00:00Z",
        });
        match automod_held(&event) {
            Some(ChatEvent::AutoModHeld {
                message_id,
                user,
                text,
                reason,
                ..
            }) => {
                assert_eq!(message_id, "msg-1");
                assert_eq!(user, "Chatter");
                assert_eq!(text, "bad words here");
                assert_eq!(reason, "automod: swearing, level 4");
            }
            other => panic!("expected AutoModHeld, got {other:?}"),
        }
    }

    #[test]
    fn blocked_term_hold_reason() {
        let event = json!({
            "user_name": "Chatter",
            "message_id": "msg-2",
            "message": { "text": "hi" },
            "reason": "blocked_term",
            "blocked_term": { "terms_found": [] },
        });
        match automod_held(&event) {
            Some(ChatEvent::AutoModHeld { reason, .. }) => assert_eq!(reason, "blocked term"),
            other => panic!("expected AutoModHeld, got {other:?}"),
        }
    }

    #[test]
    fn automod_update_resolves_with_status_and_moderator() {
        let event = json!({
            "moderator_user_name": "StreamMod",
            "message_id": "msg-1",
            "status": "Approved",
        });
        match automod_resolved(&event) {
            Some(ChatEvent::AutoModResolved {
                message_id,
                status,
                moderator,
                ..
            }) => {
                assert_eq!(message_id, "msg-1");
                assert_eq!(status, AutoModStatus::Approved);
                assert_eq!(moderator, "StreamMod");
            }
            other => panic!("expected AutoModResolved, got {other:?}"),
        }
    }

    #[test]
    fn expired_update_has_no_moderator() {
        let event = json!({
            "moderator_user_name": "irrelevant",
            "message_id": "msg-1",
            "status": "expired",
        });
        match automod_resolved(&event) {
            Some(ChatEvent::AutoModResolved {
                status, moderator, ..
            }) => {
                assert_eq!(status, AutoModStatus::Expired);
                assert_eq!(moderator, "");
            }
            other => panic!("expected AutoModResolved, got {other:?}"),
        }
    }

    #[test]
    fn suspicious_message_restricted_with_detail() {
        let event = json!({
            "broadcaster_user_login": "streamer",
            "user_id": "1050263434",
            "user_login": "baduser",
            "user_name": "BadUser",
            "low_trust_status": "restricted",
            "shared_ban_channel_ids": ["100", "200"],
            "types": ["ban_evader", "banned_in_shared_channel"],
            "ban_evasion_evaluation": "likely",
            "message": {
                "message_id": "msg-9",
                "text": "hello Kappa",
                "fragments": [
                    { "type": "text", "text": "hello ", "cheermote": null, "emote": null },
                    { "type": "emote", "text": "Kappa", "cheermote": null,
                      "emote": { "id": "25", "emote_set_id": "0" } },
                ],
            },
        });
        match suspicious_message(&event, Utc::now()) {
            Some(ChatEvent::Suspicious {
                status,
                detail,
                message,
                ..
            }) => {
                assert_eq!(status, SuspiciousStatus::Restricted);
                assert_eq!(detail, "likely ban evader · banned in 2 shared channels");
                assert_eq!(message.id, "msg-9");
                assert_eq!(message.author.login, "baduser");
                assert_eq!(message.author.display_name, "BadUser");
                assert_eq!(message.raw_text, "hello Kappa");
                assert!(message.elements.iter().any(|e| matches!(
                    e,
                    bks_core::MessageElement::Emote(em) if em.id == "25" && em.name == "Kappa"
                )));
            }
            other => panic!("expected Suspicious, got {other:?}"),
        }
    }

    #[test]
    fn suspicious_message_monitored_without_detail() {
        let event = json!({
            "user_login": "chatter",
            "user_name": "Chatter",
            "low_trust_status": "active_monitoring",
            "types": ["manually_added"],
            "ban_evasion_evaluation": "unknown",
            "message": { "message_id": "msg-1", "text": "hi", "fragments": [] },
        });
        match suspicious_message(&event, Utc::now()) {
            Some(ChatEvent::Suspicious { status, detail, message, .. }) => {
                assert_eq!(status, SuspiciousStatus::Monitored);
                assert_eq!(detail, "");
                assert!(matches!(
                    message.elements.as_slice(),
                    [bks_core::MessageElement::Text { text, .. }] if text == "hi"
                ));
            }
            other => panic!("expected Suspicious, got {other:?}"),
        }
    }

    #[test]
    fn suspicious_update_notices() {
        let event = |status: &str| {
            json!({
                "moderator_user_name": "StreamMod",
                "user_name": "BadUser",
                "low_trust_status": status,
            })
        };
        assert_eq!(
            suspicious_update(&event("restricted")),
            Some("StreamMod restricted BadUser as a suspicious user".to_string())
        );
        assert_eq!(
            suspicious_update(&event("active_monitoring")),
            Some("StreamMod started monitoring BadUser as a suspicious user".to_string())
        );
        assert_eq!(
            suspicious_update(&event("none")),
            Some("StreamMod removed BadUser's suspicious-user treatment".to_string())
        );
    }

    #[test]
    fn scope_checks() {
        let full: Vec<String> = [
            "moderator:manage:banned_users",
            "moderator:manage:chat_messages",
            "moderator:read:blocked_terms",
            "moderator:read:chat_settings",
            "moderator:read:unban_requests",
            "moderator:read:warnings",
            "moderator:read:moderators",
            "moderator:read:vips",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert!(can_moderate_feed(&full));
        // An old token from before the feed's scopes were added.
        let old: Vec<String> = ["chat:read", "moderator:manage:banned_users"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(!can_moderate_feed(&old));
        assert!(!can_automod(&old));
        assert!(can_automod(&["moderator:manage:automod".to_string()]));
        assert!(!can_suspicious(&old));
        assert!(can_suspicious(&[
            "moderator:read:suspicious_users".to_string()
        ]));
    }
}
