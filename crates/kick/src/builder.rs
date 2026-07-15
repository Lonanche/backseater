//! Converts a Kick `ChatMessageEvent` payload into a [`Message`].
//!
//! Kick sends emotes inline in the message text as `[emote:{id}:{name}]`
//! markers (id numeric, name the display text). We split the content on those
//! markers into alternating [`Text`] and [`Emote`] tokens, mirroring how the
//! Twitch builder splits on the native `emotes` tag.
//!
//! [`Text`]: MessageElement::Text
//! [`Emote`]: MessageElement::Emote

use bks_core::{Author, Badge, Color, Emote, Message, MessageElement, Platform, ReplyParent};
use chrono::Utc;
use serde::Deserialize;

use crate::api::SubscriberBadge;

/// `https://files.kick.com/emotes/{id}/fullsize` — Kick's emote CDN. Takes any
/// `Display` id so both the inline-parse (`&str`) and picker (`u64`) paths reuse it.
pub(crate) fn emote_url(id: impl std::fmt::Display) -> String {
    format!("https://files.kick.com/emotes/{id}/fullsize")
}

/// The `data` payload of a Kick `ChatMessageEvent` (already JSON-parsed).
#[derive(Deserialize)]
pub struct KickChatMessage {
    pub id: String,
    pub content: String,
    pub created_at: Option<String>,
    pub sender: Sender,
    /// Present on a reply: carries the message being replied to. The live socket
    /// sends this as an object; the `/history` endpoint serializes it as a JSON
    /// *string* (`"{\"message_ref\":…}"`) instead — [`metadata_object_only`]
    /// accepts only the object form and treats the string as absent (a faded/
    /// pinned history entry doesn't need reply context).
    #[serde(default, deserialize_with = "metadata_object_only")]
    pub metadata: Option<Metadata>,
}

/// Deserializes `metadata` only when it's the object the live socket sends,
/// ignoring the JSON-string form `/history` uses (see [`KickChatMessage::metadata`]).
fn metadata_object_only<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Metadata>, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(match v {
        serde_json::Value::Object(_) => serde_json::from_value(v).ok(),
        _ => None,
    })
}

/// Extra chat-message data. For a reply, Kick nests the parent under
/// `original_sender` + `original_message`.
#[derive(Deserialize)]
pub struct Metadata {
    #[serde(default)]
    pub original_sender: Option<ReplyUser>,
    #[serde(default)]
    pub original_message: Option<ReplyMessage>,
}

#[derive(Deserialize)]
pub struct ReplyUser {
    pub username: String,
}

#[derive(Deserialize)]
pub struct ReplyMessage {
    /// Id of the parent message; lets the UI link the reply into a thread.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub content: String,
}

#[derive(Deserialize)]
pub struct Sender {
    /// Kick numeric user id; remembered so moderation can target this chatter
    /// (Kick's API can't resolve a username → id).
    #[serde(default)]
    pub id: u64,
    pub username: String,
    #[serde(default)]
    pub identity: Identity,
}

#[derive(Deserialize, Default)]
pub struct Identity {
    /// `#RRGGBB`, may be empty.
    #[serde(default)]
    pub color: Option<String>,
    /// Inline badges Kick sends with each message (mod/vip/og/subscriber/...).
    /// These have no image url — we resolve them to a tier image (subscriber) or
    /// a bundled asset (the rest).
    #[serde(default)]
    pub badges: Vec<KickBadge>,
    /// Newer badge array that carries its own CDN `image_url` (e.g. the global
    /// "level" badge). Kept separate from `badges` since the shape differs.
    #[serde(default)]
    pub badges_v2: Vec<KickBadgeV2>,
}

#[derive(Deserialize)]
pub struct KickBadge {
    /// e.g. `subscriber`, `moderator`, `vip`, `og`, `founder`, `broadcaster`.
    #[serde(rename = "type")]
    pub badge_type: String,
    /// Subscriber tenure in months (only meaningful for `subscriber`).
    #[serde(default)]
    pub count: u64,
}

/// A `badges_v2` entry — a self-describing badge with its own CDN image, used for
/// Kick's global "level" badge. We read its image + level straight off the event
/// (no asset bundling or channel resolution needed).
#[derive(Deserialize)]
pub struct KickBadgeV2 {
    /// e.g. `level`.
    #[serde(default)]
    pub name: String,
    /// The badge's CDN image, ready to render as-is.
    #[serde(default)]
    pub image_url: String,
    #[serde(default)]
    pub metadata: KickBadgeMetadata,
}

#[derive(Deserialize, Default)]
pub struct KickBadgeMetadata {
    /// The numeric level, shown in the tooltip as "Level N".
    #[serde(default)]
    pub level: u64,
}

/// Splits Kick message `content` into text runs and emotes on `[emote:id:name]`.
pub fn parse_content(content: &str, text_color: Option<Color>) -> Vec<MessageElement> {
    let mut elements = Vec::new();
    let mut rest = content;

    let push_text = |elements: &mut Vec<MessageElement>, s: &str| {
        if !s.is_empty() {
            elements.push(MessageElement::Text {
                text: s.to_string(),
                color: text_color,
            });
        }
    };

    while let Some(start) = rest.find("[emote:") {
        // Text before the marker.
        push_text(&mut elements, &rest[..start]);

        let after = &rest[start + "[emote:".len()..];
        // Marker is `id:name]`; bail out if it's malformed and keep it as text.
        let Some(close) = after.find(']') else {
            push_text(&mut elements, &rest[start..]);
            rest = "";
            break;
        };
        let inner = &after[..close];
        match inner.split_once(':') {
            Some((id, name)) if !id.is_empty() => {
                elements.push(MessageElement::Emote(std::sync::Arc::new(Emote {
                    url: emote_url(id),
                    id: id.to_string(),
                    name: name.to_string(),
                    animated: false,
                    tooltip: bks_core::EmoteTooltip::provider("Kick"),
                })));
            }
            // Unparseable marker: keep the raw text so nothing is lost.
            _ => push_text(
                &mut elements,
                &rest[start..start + "[emote:".len() + close + 1],
            ),
        }
        rest = &after[close + 1..];
    }
    push_text(&mut elements, rest);

    bks_core::mentionize(bks_core::linkify(elements))
}

/// The subscriber badge image for `months` of tenure: the highest tier whose
/// `months` threshold the chatter meets. `None` if no tier qualifies.
fn subscriber_badge_url(badges: &[SubscriberBadge], months: u64) -> Option<&str> {
    badges
        .iter()
        .filter(|b| months >= b.months)
        .max_by_key(|b| b.months)
        .map(|b| b.src.as_str())
}

/// The hover-tooltip title for a Kick standard badge type.
/// Unknown types title-case the raw type as a sensible fallback
/// so a newly added Kick badge still gets a readable tooltip.
fn kick_badge_title(badge_type: &str) -> String {
    match badge_type {
        "bot" => "Bot",
        "broadcaster" => "Broadcaster",
        "founder" => "Founder",
        "moderator" => "Moderator",
        "og" => "OG",
        "sidekick" => "Sidekick",
        "staff" => "Staff",
        "sub_gifter" => "Sub Gifter",
        "trainwreckstv" => "TrainwrecksTV",
        "verified" => "Verified",
        "vip" => "VIP",
        other => return title_case(other),
    }
    .to_string()
}

/// Turns an `under_scored` or lowercase badge type into "Title Case" for the
/// fallback tooltip of an unmapped badge.
fn title_case(s: &str) -> String {
    s.split(['_', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Builds the chatter's full badge list from their identity. The `badges_v2`
/// entries (Kick's global "level" badge, which carries its own CDN image) come
/// first — that matches Kick's own display order (level `sort_order` 1 precedes
/// the inline subscriber/vip/... badges) — followed by the inline `badges`.
pub fn build_badges(identity: &Identity, sub_badges: &[SubscriberBadge]) -> Vec<Badge> {
    let level = identity.badges_v2.iter().filter_map(level_badge);
    // Kick sends inline badges with no image: subscriber badges get a per-tier
    // image (resolved here from the channel set); the rest keep an empty url for
    // the app to fill from bundled assets.
    let inline = identity.badges.iter().map(|b| {
        if b.badge_type == "subscriber" {
            // Include the month count Kick sends, e.g. "Subscriber (5 months)".
            let title = format!(
                "Subscriber ({} {})",
                b.count,
                bks_core::plural(b.count, "month", "months")
            );
            Badge {
                id: format!("subscriber/{}", b.count),
                url: subscriber_badge_url(sub_badges, b.count)
                    .unwrap_or_default()
                    .to_string(),
                title: Some(title),
            }
        } else {
            Badge {
                id: b.badge_type.clone(),
                url: String::new(),
                title: Some(kick_badge_title(&b.badge_type)),
            }
        }
    });
    level.chain(inline).collect()
}

/// Converts a `badges_v2` level badge into a [`Badge`] with its CDN image and a
/// "Level N" tooltip. Returns `None` for entries without an image or that aren't
/// the level badge (the only `badges_v2` kind we render today).
fn level_badge(b: &KickBadgeV2) -> Option<Badge> {
    if b.name != "level" || b.image_url.is_empty() {
        return None;
    }
    Some(Badge {
        id: format!("level/{}", b.metadata.level),
        url: b.image_url.clone(),
        title: Some(format!("Level {}", b.metadata.level)),
    })
}

/// Builds a platform-agnostic [`Message`] from a Kick chat payload. `sub_badges`
/// are the channel's per-tier subscriber badge images (from channel resolution);
/// they fill in the subscriber badge URL since Kick doesn't send it inline.
pub fn build_message(
    channel: &str,
    sub_badges: &[SubscriberBadge],
    msg: KickChatMessage,
) -> Message {
    let color = msg
        .sender
        .identity
        .color
        .as_deref()
        .filter(|c| !c.is_empty())
        .and_then(Color::from_hex);

    let timestamp = msg
        .created_at
        .as_deref()
        .and_then(bks_core::parse_rfc3339)
        .unwrap_or_else(Utc::now);

    let badges = build_badges(&msg.sender.identity, sub_badges);

    // Some Kick accounts (e.g. bots) carry a leading `@` in their username;
    // normalize it away so it doesn't leak into name-based matching or lookups.
    let username = bks_core::normalize_username(&msg.sender.username).to_string();
    let author = Author {
        login: username.to_lowercase(),
        display_name: username,
        color,
        badges,
        user_id: msg.sender.id.to_string(),
        paint: None,
    };

    let elements = parse_content(&msg.content, None);

    // On a reply, Kick nests the parent under `metadata.original_*`; surface it
    // as a `ReplyParent` for the "replying to" line. Unlike Twitch, the body
    // carries no mention prefix, so `content` is already what the user typed.
    let reply = msg.metadata.and_then(|m| {
        let author = bks_core::normalize_username(&m.original_sender?.username).to_string();
        let (text, parent_id) = m
            .original_message
            .map(|o| (o.content, o.id))
            .unwrap_or_default();
        Some(ReplyParent {
            author,
            text,
            // Kick replies are flat (no separate thread root), so the parent id
            // doubles as the thread root — chains still link one level deep.
            thread_root_id: parent_id.clone(),
            parent_id,
        })
    });

    Message {
        id: msg.id,
        platform: Platform::Kick,
        channel: channel.to_string(),
        timestamp,
        author,
        raw_text: msg.content,
        elements,
        reply,
        first_message: false,
        highlighted: false,
        historical: false,
        reward_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(elements: &[MessageElement]) -> Vec<String> {
        elements
            .iter()
            .map(|e| match e {
                MessageElement::Text { text, .. } => format!("T:{text}"),
                MessageElement::Emote(em) => format!("E:{}:{}", em.name, em.id),
                _ => "?".into(),
            })
            .collect()
    }

    #[test]
    fn plain_text_is_one_run() {
        assert_eq!(
            kinds(&parse_content("hello world", None)),
            vec!["T:hello world"]
        );
    }

    #[test]
    fn splits_around_an_emote() {
        let els = parse_content("hi [emote:42:KEKW] there", None);
        assert_eq!(kinds(&els), vec!["T:hi ", "E:KEKW:42", "T: there"]);
        match &els[1] {
            MessageElement::Emote(e) => {
                assert_eq!(e.url, "https://files.kick.com/emotes/42/fullsize")
            }
            _ => panic!("expected emote"),
        }
    }

    #[test]
    fn handles_adjacent_emotes() {
        let els = parse_content("[emote:1:a][emote:2:b]", None);
        assert_eq!(kinds(&els), vec!["E:a:1", "E:b:2"]);
    }

    #[test]
    fn malformed_marker_kept_as_text() {
        let els = parse_content("oops [emote:nope", None);
        assert_eq!(kinds(&els), vec!["T:oops ", "T:[emote:nope"]);
    }

    #[test]
    fn badge_titles_match_friendly_names_with_fallback() {
        assert_eq!(kick_badge_title("moderator"), "Moderator");
        assert_eq!(kick_badge_title("vip"), "VIP");
        assert_eq!(kick_badge_title("og"), "OG");
        assert_eq!(kick_badge_title("sub_gifter"), "Sub Gifter");
        // An unmapped type title-cases its raw form so it still reads cleanly.
        assert_eq!(kick_badge_title("super_fan"), "Super Fan");
    }

    #[test]
    fn level_badge_from_v2_carries_cdn_image_and_title() {
        // Real-shape identity from xQc's chat: a `badges_v2` level badge (with its
        // own CDN image) plus an inline subscriber badge.
        let identity: Identity = serde_json::from_str(
            r##"{
                "color": "#1475E1",
                "badges": [{"type":"subscriber","text":"Subscriber","count":7,"sort_order":9}],
                "badges_v2": [{"name":"level","badge_type":"global","image_url":"https://ext.cdn.kick.com/chat/badges/35_abc.png","metadata":{"level":35},"selected":true,"sort_order":1}]
            }"##,
        )
        .unwrap();
        let badges = build_badges(
            &identity,
            &[SubscriberBadge {
                months: 0,
                src: "t0".into(),
            }],
        );
        // Level badge comes first, with its CDN image and "Level N" title.
        assert_eq!(badges[0].id, "level/35");
        assert_eq!(
            badges[0].url,
            "https://ext.cdn.kick.com/chat/badges/35_abc.png"
        );
        assert_eq!(badges[0].title.as_deref(), Some("Level 35"));
        // Followed by the inline subscriber badge.
        assert_eq!(badges[1].id, "subscriber/7");
        assert_eq!(badges[1].title.as_deref(), Some("Subscriber (7 months)"));
    }

    #[test]
    fn non_level_or_imageless_v2_badge_is_skipped() {
        let identity: Identity = serde_json::from_str(
            r#"{"badges_v2":[
                {"name":"level","image_url":"","metadata":{"level":3}},
                {"name":"other","image_url":"https://x/y.png","metadata":{}}
            ]}"#,
        )
        .unwrap();
        assert!(build_badges(&identity, &[]).is_empty());
    }

    #[test]
    fn reply_metadata_becomes_reply_parent() {
        let data = r#"{
            "id": "m1",
            "content": "agreed",
            "sender": { "id": 5, "username": "Bob" },
            "metadata": {
                "original_sender": { "username": "Alice" },
                "original_message": { "id": "m0", "content": "hot take" }
            }
        }"#;
        let chat: KickChatMessage = serde_json::from_str(data).unwrap();
        let msg = build_message("chan", &[], chat);
        let reply = msg.reply.expect("reply parent");
        assert_eq!(reply.author, "Alice");
        assert_eq!(reply.text, "hot take");
        assert_eq!(reply.parent_id.as_deref(), Some("m0"));
        // Kick has no separate thread root; parent id doubles as it.
        assert_eq!(reply.thread_root_id.as_deref(), Some("m0"));
    }

    #[test]
    fn plain_message_has_no_reply() {
        let data = r#"{"id":"m1","content":"hi","sender":{"id":5,"username":"Bob"}}"#;
        let chat: KickChatMessage = serde_json::from_str(data).unwrap();
        assert!(build_message("chan", &[], chat).reply.is_none());
    }

    #[test]
    fn author_handle_at_sign_is_stripped() {
        let data =
            r#"{"id":"m1","content":"hi","sender":{"id":5,"username":"@StreamElements"}}"#;
        let chat: KickChatMessage = serde_json::from_str(data).unwrap();
        let msg = build_message("chan", &[], chat);
        assert_eq!(msg.author.display_name, "StreamElements");
        assert_eq!(msg.author.login, "streamelements");
    }

    #[test]
    fn subscriber_badge_picks_highest_met_tier() {
        let tiers = vec![
            SubscriberBadge {
                months: 0,
                src: "t0".into(),
            },
            SubscriberBadge {
                months: 3,
                src: "t3".into(),
            },
            SubscriberBadge {
                months: 12,
                src: "t12".into(),
            },
        ];
        assert_eq!(subscriber_badge_url(&tiers, 1), Some("t0"));
        assert_eq!(subscriber_badge_url(&tiers, 5), Some("t3"));
        assert_eq!(subscriber_badge_url(&tiers, 99), Some("t12"));
        assert_eq!(subscriber_badge_url(&[], 5), None);
    }
}
