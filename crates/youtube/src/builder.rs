//! Turns a YouTube live-chat renderer (one `addChatItemAction` item) into either
//! a [`Message`] or a highlighted [`ChatEvent::Event`].
//!
//! The InnerTube `get_live_chat` feed delivers each chat item as a single-keyed
//! object like `{"liveChatTextMessageRenderer": {…}}`. We match on that key:
//! - `liveChatTextMessageRenderer` → a normal chat [`Message`]. Its `message.runs`
//!   are alternating text + `emoji` runs; emoji runs become inline [`Emote`]s
//!   (YouTube custom/channel emojis carry a thumbnail image), mirroring how the
//!   Kick builder splits `[emote:…]` markers.
//! - `liveChatPaidMessageRenderer` / `liveChatPaidStickerRenderer` (Super Chat /
//!   Super Sticker) → an [`EventKind::Bits`] event (money, closest existing kind).
//! - `liveChatMembershipItemRenderer` → an [`EventKind::Sub`] event.
//! - `liveChatSponsorshipsGiftPurchaseAnnouncementRenderer` → [`EventKind::Gift`].
//!
//! Anything else is ignored.
//!
//! We traverse `serde_json::Value` directly (rather than typed structs) because
//! the renderers are deeply nested and heterogeneous.

use std::sync::Arc;

use bks_core::{Author, Badge, Color, Emote, EmoteTooltip, Message, MessageElement, Platform};
use bks_platform::{ChatEvent, EventDetails, EventKind};
use chrono::{TimeZone, Utc};
use serde_json::Value;

/// The green YouTube uses for a channel member's name when they carry a
/// membership badge but no explicit name color.
const MEMBER_NAME_COLOR: Color = Color::rgb(43, 166, 64);

/// The result of parsing one chat item: a chat message, a highlighted event, or
/// nothing (an item kind we don't render).
pub enum ParsedItem {
    Message(Box<Message>),
    Event {
        kind: EventKind,
        text: String,
        timestamp: chrono::DateTime<chrono::Utc>,
        message: Option<Box<Message>>,
        /// Compact events-panel actor (bold name), if the event has a structured
        /// form; `None` falls the panel back to `text`.
        actor: Option<String>,
        /// Condensed detail following the actor ("sent $5.00", "became a member").
        compact: Option<String>,
    },
    Ignored,
}

/// Parses one `addChatItemAction` item into a [`ParsedItem`]. `channel` is the
/// tab's channel name (stored on the message), `video_id` the current live video.
pub fn build_item(channel: &str, item: &Value) -> ParsedItem {
    // The item is a single-keyed object: `{ "<rendererName>": { … } }`.
    let Some((name, renderer)) = item.as_object().and_then(|o| o.iter().next()) else {
        return ParsedItem::Ignored;
    };

    match name.as_str() {
        "liveChatTextMessageRenderer" => build_text_message(channel, renderer),
        "liveChatPaidMessageRenderer" => paid_event(renderer, EventKind::Bits, true),
        "liveChatPaidStickerRenderer" => paid_event(renderer, EventKind::Bits, false),
        "liveChatMembershipItemRenderer" => membership_event(renderer),
        "liveChatSponsorshipsGiftPurchaseAnnouncementRenderer" => gift_event(renderer),
        _ => ParsedItem::Ignored,
    }
}

/// Builds a chat [`Message`] from a `liveChatTextMessageRenderer`.
fn build_text_message(channel: &str, r: &Value) -> ParsedItem {
    let id = str_field(r, "id");
    // Normalize away a leading `@` handle sigil so it can't leak into name-based
    // matching (mentions/highlights/ignore/suppress) or usercard lookups.
    let author_name = bks_core::normalize_username(&parse_runs_text(&r["authorName"])).to_string();
    if id.is_empty() || author_name.is_empty() {
        return ParsedItem::Ignored;
    }

    let color = author_color(r);
    let author = build_author(r, author_name.to_string(), color);

    let mut elements = parse_message_runs(&r["message"], color);
    elements = bks_core::mentionize(bks_core::linkify(elements));
    let raw_text = elements_to_text(&elements);
    let timestamp = parse_timestamp_usec(&str_field(r, "timestampUsec"));

    ParsedItem::Message(Box::new(Message {
        id,
        platform: Platform::YouTube,
        channel: channel.to_string(),
        timestamp,
        author,
        elements,
        raw_text,
        reply: None,
        first_message: false,
        highlighted: false,
        historical: false,
        reward_id: None,
    }))
}

/// The name color for a renderer's author: YouTube's explicit `authorNameTextColor`,
/// else the member green when the author carries a membership badge.
fn author_color(r: &Value) -> Option<Color> {
    let has_member_badge = author_badges(r).iter().any(|b| b.membership);
    author_name_color(r).or(if has_member_badge {
        Some(MEMBER_NAME_COLOR)
    } else {
        None
    })
}

/// Builds an [`Author`] from any live-chat renderer that carries the standard
/// `authorName`/`authorExternalChannelId`/`authorBadges` fields (text messages
/// and Super Chats share this shape).
fn build_author(r: &Value, display_name: String, color: Option<Color>) -> Author {
    let badges = author_badges(r)
        .into_iter()
        .filter_map(|b| b.badge)
        .collect();
    Author {
        login: display_name.to_lowercase(),
        display_name,
        color,
        badges,
        user_id: str_field(r, "authorExternalChannelId"),
        paint: None,
    }
}

/// A Super Chat / Super Sticker → a Bits-kind event. `with_body` includes the
/// attached message (Super Chats have one, Super Stickers don't). Like a Twitch
/// resub, the donor's comment rides as a full [`Message`] (author color, badges,
/// inline emotes) so it renders as a chat line under the "sent $5.00" header
/// rather than being flattened into the event text; the panel's compact row
/// reads "**Author** · sent $5.00".
fn paid_event(r: &Value, kind: EventKind, with_body: bool) -> ParsedItem {
    let raw_author = parse_runs_text(&r["authorName"]);
    let author_name = bks_core::normalize_username(&raw_author).to_string();
    let amount = parse_runs_text(&r["purchaseAmountText"]);
    if author_name.is_empty() || amount.is_empty() {
        return ParsedItem::Ignored;
    }
    let timestamp = parse_timestamp_usec(&str_field(r, "timestampUsec"));
    let compact = format!("sent {amount}");

    // The donor's comment, if any, as a chat line under the header. Super Chats
    // color their name via `authorNameTextColor` (Super Chat tiers have their
    // own name colors) like a normal message; reuse the same author/element path.
    let color = author_color(r);
    let mut elements = if with_body {
        let mut els = parse_message_runs(&r["message"], color);
        els = bks_core::mentionize(bks_core::linkify(els));
        els
    } else {
        Vec::new()
    };
    let message = if elements.is_empty() {
        None
    } else {
        let raw_text = elements_to_text(&elements);
        Some(Box::new(Message {
            id: str_field(r, "id"),
            platform: Platform::YouTube,
            channel: String::new(),
            timestamp,
            author: build_author(r, author_name.clone(), color),
            elements: std::mem::take(&mut elements),
            raw_text,
            reply: None,
            first_message: false,
            highlighted: false,
            historical: false,
            reward_id: None,
        }))
    };

    ParsedItem::Event {
        kind,
        // Fallback for renderers that don't use the compact actor/detail row.
        text: format!("{author_name} sent {amount}"),
        timestamp,
        message,
        actor: Some(author_name),
        compact: Some(compact),
    }
}

/// A new member / member milestone → a Sub-kind event.
fn membership_event(r: &Value) -> ParsedItem {
    let raw_author = parse_runs_text(&r["authorName"]);
    let author = bks_core::normalize_username(&raw_author);
    // `headerSubtext` is "Welcome!" / "Member for N months"; `headerPrimaryText`
    // exists on milestones. Prefer whichever is present.
    let header = {
        let sub = parse_runs_text(&r["headerSubtext"]);
        if sub.is_empty() {
            parse_runs_text(&r["headerPrimaryText"])
        } else {
            sub
        }
    };
    if author.is_empty() {
        return ParsedItem::Ignored;
    }
    let compact = if header.is_empty() {
        "became a member".to_string()
    } else {
        header.clone()
    };
    let text = if header.is_empty() {
        format!("{author} became a member")
    } else {
        format!("{author}: {header}")
    };
    ParsedItem::Event {
        kind: EventKind::Sub,
        text,
        timestamp: parse_timestamp_usec(&str_field(r, "timestampUsec")),
        message: None,
        actor: Some(author.to_string()),
        compact: Some(compact),
    }
}

/// A gifted-membership announcement → a Gift-kind event.
fn gift_event(r: &Value) -> ParsedItem {
    let header = &r["header"]["liveChatSponsorshipsHeaderRenderer"];
    let raw_author = parse_runs_text(&header["authorName"]);
    let author = bks_core::normalize_username(&raw_author);
    let primary = parse_runs_text(&header["primaryText"]);
    if author.is_empty() && primary.is_empty() {
        return ParsedItem::Ignored;
    }
    // `primaryText` is like "Gifted 5 memberships"; prefix the gifter's name.
    let text = match (author.is_empty(), primary.is_empty()) {
        (false, false) => format!("{author} {}", lowercase_first(&primary)),
        (true, false) => primary.clone(),
        (false, true) => format!("{author} gifted memberships"),
        (true, true) => unreachable!(),
    };
    // With a gifter name, split it into the bold actor + condensed detail; an
    // anonymous gift keeps only the pre-formatted text (no actor).
    let (actor, compact) = if author.is_empty() {
        (None, None)
    } else {
        let detail = if primary.is_empty() {
            "gifted memberships".to_string()
        } else {
            lowercase_first(&primary)
        };
        (Some(author.to_string()), Some(detail))
    };
    ParsedItem::Event {
        kind: EventKind::Gift,
        text,
        timestamp: parse_timestamp_usec(&str_field(r, "timestampUsec")),
        message: None,
        actor,
        compact,
    }
}

/// Parses a `message`-style value (`{runs:[…]}` or `{simpleText}`) into rendered
/// elements: text runs stay text; `emoji` runs with a thumbnail become inline
/// emotes (custom/channel emoji); standard-unicode emoji fall back to their text.
fn parse_message_runs(value: &Value, text_color: Option<Color>) -> Vec<MessageElement> {
    let mut elements = Vec::new();
    let mut text_buf = String::new();

    let flush = |buf: &mut String, elements: &mut Vec<MessageElement>| {
        if !buf.is_empty() {
            elements.push(MessageElement::Text {
                text: std::mem::take(buf),
                color: text_color,
            });
        }
    };

    if let Some(simple) = value.get("simpleText").and_then(Value::as_str) {
        elements.push(MessageElement::Text {
            text: simple.to_string(),
            color: text_color,
        });
        return elements;
    }

    for run in value["runs"].as_array().into_iter().flatten() {
        if let Some(text) = run.get("text").and_then(Value::as_str) {
            text_buf.push_str(text);
        } else if let Some(emoji) = run.get("emoji") {
            match emoji_emote(emoji) {
                Some(emote) => {
                    flush(&mut text_buf, &mut elements);
                    elements.push(MessageElement::Emote(Arc::new(emote)));
                }
                None => text_buf.push_str(&emoji_fallback_text(emoji)),
            }
        }
    }
    flush(&mut text_buf, &mut elements);
    elements
}

/// Builds an inline [`Emote`] from an `emoji` run. Returns `None` for a standard
/// unicode emoji (no custom image / `isCustomEmoji` false) — those render as text.
fn emoji_emote(emoji: &Value) -> Option<Emote> {
    // Standard unicode emoji carry an image too, but we only want *custom* channel
    // emojis as inline images; unicode ones render fine as text (and the font has
    // them). YouTube marks channel emojis with `isCustomEmoji: true`.
    if !emoji
        .get("isCustomEmoji")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let url = best_thumbnail(&emoji["image"])?;
    let name = emoji_fallback_text(emoji);
    let id = emoji
        .get("emojiId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| url.clone());
    Some(Emote {
        id,
        name: if name.is_empty() {
            "YouTube Emoji".to_string()
        } else {
            name
        },
        url,
        // YouTube custom emojis are static PNGs.
        animated: false,
        tooltip: EmoteTooltip::provider("YouTube"),
    })
}

/// The text to show for an emoji when it has no inline image: its shortcut
/// (`:shortcut:`), else its id, else its accessibility label.
fn emoji_fallback_text(emoji: &Value) -> String {
    if let Some(shortcut) = emoji["shortcuts"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(Value::as_str)
    {
        return shortcut.to_string();
    }
    if let Some(id) = emoji.get("emojiId").and_then(Value::as_str) {
        if !id.is_empty() {
            return id.to_string();
        }
    }
    emoji["image"]["accessibility"]["accessibilityData"]["label"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// A parsed author badge: the renderable [`Badge`] (custom member thumbnail;
/// `None` for icon-only mod/owner badges we don't bundle) and whether it's a
/// membership badge (drives the member name color).
struct AuthorBadge {
    badge: Option<Badge>,
    membership: bool,
}

/// Reads `authorBadges[].liveChatAuthorBadgeRenderer` into [`AuthorBadge`]s.
fn author_badges(r: &Value) -> Vec<AuthorBadge> {
    r["authorBadges"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| {
            let renderer = &b["liveChatAuthorBadgeRenderer"];
            if renderer.is_null() {
                return None;
            }
            let tooltip = renderer["tooltip"].as_str().unwrap_or_default().to_string();
            // Membership badges carry a custom thumbnail image; mod/owner/verified
            // are icon-only (`icon.iconType`) which we don't have art for yet.
            let membership = !renderer["customThumbnail"].is_null();
            let badge = best_thumbnail(&renderer["customThumbnail"]).map(|url| Badge {
                id: format!("member/{tooltip}"),
                url,
                title: (!tooltip.is_empty()).then(|| tooltip.clone()),
            });
            Some(AuthorBadge { badge, membership })
        })
        .collect()
}

/// The name color from `authorNameTextColor` (an ARGB int YouTube sends), if set.
fn author_name_color(r: &Value) -> Option<Color> {
    let argb = r.get("authorNameTextColor")?.as_i64()?;
    Some(argb_to_color(argb as u32))
}

/// Converts a YouTube ARGB integer to an [`Color`] (drops alpha).
fn argb_to_color(argb: u32) -> Color {
    Color::rgb((argb >> 16) as u8, (argb >> 8) as u8, argb as u8)
}

/// The best (largest) thumbnail URL from a `{thumbnails:[{url,width}]}` object.
fn best_thumbnail(image: &Value) -> Option<String> {
    image["thumbnails"]
        .as_array()?
        .iter()
        .max_by_key(|t| t["width"].as_u64().unwrap_or(0))
        .and_then(|t| t["url"].as_str())
        .map(normalize_url)
}

/// YouTube emoji/badge thumbnail URLs sometimes come protocol-relative (`//…`).
fn normalize_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        url.to_string()
    }
}

/// Concatenates the text of a `{runs}`/`{simpleText}` value (no emotes), trimmed.
pub fn parse_runs_text(value: &Value) -> String {
    if let Some(simple) = value.get("simpleText").and_then(Value::as_str) {
        return simple.trim().to_string();
    }
    let mut text = String::new();
    for run in value["runs"].as_array().into_iter().flatten() {
        if let Some(t) = run.get("text").and_then(Value::as_str) {
            text.push_str(t);
        } else if let Some(emoji) = run.get("emoji") {
            text.push_str(&emoji_fallback_text(emoji));
        }
    }
    text.trim().to_string()
}

/// The plain text of a rendered element stream, for `raw_text`/search.
fn elements_to_text(elements: &[MessageElement]) -> String {
    let mut out = String::new();
    for el in elements {
        match el {
            MessageElement::Text { text, .. } => out.push_str(text),
            MessageElement::Emote(e) => out.push_str(&e.name),
            MessageElement::Mention { login } => {
                out.push('@');
                out.push_str(login);
            }
            MessageElement::Link { text, .. } => out.push_str(text),
            MessageElement::Badge(_) => {}
        }
    }
    out
}

/// Parses a microsecond epoch string (`timestampUsec`) into a UTC datetime,
/// falling back to now for a missing/garbage value.
fn parse_timestamp_usec(usec: &str) -> chrono::DateTime<Utc> {
    usec.parse::<i64>()
        .ok()
        .and_then(|us| Utc.timestamp_micros(us).single())
        .unwrap_or_else(Utc::now)
}

/// A convenience to read a top-level string field (empty if absent).
fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Lowercases the first character of `s` (for gluing "Gifted…" after a name).
fn lowercase_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Route the parsed item into a `ChatEvent`. `channel`/`video_id` context is
/// carried by [`build_item`]; this is the connector-facing convenience.
pub fn item_to_event(channel: &str, item: &Value) -> Option<ChatEvent> {
    match build_item(channel, item) {
        ParsedItem::Message(msg) => Some(ChatEvent::Message(msg)),
        ParsedItem::Event {
            kind,
            text,
            timestamp,
            mut message,
            actor,
            compact,
        } => {
            // The attached message is built without channel context in
            // `build_item`; fill it in here (it's stored on each row).
            if let Some(msg) = message.as_mut() {
                msg.channel = channel.to_string();
            }
            Some(ChatEvent::Event {
                platform: Platform::YouTube,
                kind,
                text,
                timestamp,
                message,
                details: EventDetails {
                    actor,
                    compact,
                    ..Default::default()
                },
            })
        }
        ParsedItem::Ignored => None,
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
                MessageElement::Link { text, .. } => format!("L:{text}"),
                _ => "?".into(),
            })
            .collect()
    }

    #[test]
    fn text_message_basic() {
        let item = serde_json::json!({
            "liveChatTextMessageRenderer": {
                "id": "abc",
                "timestampUsec": "1700000000000000",
                "authorName": { "simpleText": "Alice" },
                "authorExternalChannelId": "UC123",
                "message": { "runs": [{ "text": "hello world" }] }
            }
        });
        match build_item("chan", &item) {
            ParsedItem::Message(msg) => {
                assert_eq!(msg.author.display_name, "Alice");
                assert_eq!(msg.author.user_id, "UC123");
                assert_eq!(kinds(&msg.elements), vec!["T:hello world"]);
                assert_eq!(msg.platform, Platform::YouTube);
            }
            _ => panic!("expected message"),
        }
    }

    #[test]
    fn author_handle_at_sign_is_stripped() {
        let item = serde_json::json!({
            "liveChatTextMessageRenderer": {
                "id": "abc",
                "timestampUsec": "1700000000000000",
                "authorName": { "simpleText": "@StreamElements" },
                "message": { "runs": [{ "text": "hi" }] }
            }
        });
        match build_item("chan", &item) {
            ParsedItem::Message(msg) => {
                assert_eq!(msg.author.display_name, "StreamElements");
                assert_eq!(msg.author.login, "streamelements");
            }
            _ => panic!("expected message"),
        }
    }

    #[test]
    fn custom_emoji_becomes_inline_emote() {
        let item = serde_json::json!({
            "liveChatTextMessageRenderer": {
                "id": "abc",
                "timestampUsec": "1700000000000000",
                "authorName": { "simpleText": "Bob" },
                "message": { "runs": [
                    { "text": "hi " },
                    { "emoji": {
                        "emojiId": "UC123/xyz",
                        "isCustomEmoji": true,
                        "shortcuts": [":cool:"],
                        "image": { "thumbnails": [
                            { "url": "//example.com/small.png", "width": 24 },
                            { "url": "//example.com/big.png", "width": 48 }
                        ]}
                    }},
                    { "text": " there" }
                ]}
            }
        });
        match build_item("chan", &item) {
            ParsedItem::Message(msg) => {
                assert_eq!(
                    kinds(&msg.elements),
                    vec!["T:hi ", "E::cool::UC123/xyz", "T: there"]
                );
                if let MessageElement::Emote(e) = &msg.elements[1] {
                    assert_eq!(e.url, "https://example.com/big.png");
                } else {
                    panic!("expected emote");
                }
            }
            _ => panic!("expected message"),
        }
    }

    #[test]
    fn standard_unicode_emoji_stays_text() {
        let item = serde_json::json!({
            "liveChatTextMessageRenderer": {
                "id": "abc",
                "timestampUsec": "1700000000000000",
                "authorName": { "simpleText": "Bob" },
                "message": { "runs": [
                    { "emoji": {
                        "emojiId": "😀",
                        "isCustomEmoji": false,
                        "shortcuts": [":grinning:"],
                        "image": { "thumbnails": [{ "url": "//x/y.png", "width": 24 }] }
                    }}
                ]}
            }
        });
        match build_item("chan", &item) {
            ParsedItem::Message(msg) => {
                assert_eq!(kinds(&msg.elements), vec!["T::grinning:"]);
            }
            _ => panic!("expected message"),
        }
    }

    #[test]
    fn super_chat_is_bits_event() {
        let item = serde_json::json!({
            "liveChatPaidMessageRenderer": {
                "id": "sc1",
                "authorName": { "simpleText": "Carol" },
                "purchaseAmountText": { "simpleText": "$5.00" },
                "message": { "runs": [{ "text": "great stream" }] }
            }
        });
        match build_item("chan", &item) {
            ParsedItem::Event {
                kind,
                text,
                message,
                actor,
                compact,
                ..
            } => {
                assert_eq!(kind, EventKind::Bits);
                // The donor comment now rides as an attached chat line (like a
                // Twitch resub), not flattened into the event text.
                assert_eq!(text, "Carol sent $5.00");
                assert_eq!(actor.as_deref(), Some("Carol"));
                assert_eq!(compact.as_deref(), Some("sent $5.00"));
                let msg = message.expect("super chat body should attach a message");
                assert_eq!(msg.author.display_name, "Carol");
                assert_eq!(kinds(&msg.elements), vec!["T:great stream"]);
            }
            _ => panic!("expected event"),
        }
    }

    #[test]
    fn super_chat_without_message_has_no_attached_line() {
        let item = serde_json::json!({
            "liveChatPaidMessageRenderer": {
                "id": "sc2",
                "authorName": { "simpleText": "Carol" },
                "purchaseAmountText": { "simpleText": "$2.00" }
            }
        });
        match build_item("chan", &item) {
            ParsedItem::Event { text, message, .. } => {
                assert_eq!(text, "Carol sent $2.00");
                assert!(message.is_none());
            }
            _ => panic!("expected event"),
        }
    }

    #[test]
    fn membership_is_sub_event() {
        let item = serde_json::json!({
            "liveChatMembershipItemRenderer": {
                "id": "m1",
                "authorName": { "simpleText": "Dave" },
                "headerSubtext": { "runs": [{ "text": "Welcome!" }] }
            }
        });
        match build_item("chan", &item) {
            ParsedItem::Event { kind, text, .. } => {
                assert_eq!(kind, EventKind::Sub);
                assert_eq!(text, "Dave: Welcome!");
            }
            _ => panic!("expected event"),
        }
    }

    #[test]
    fn gift_is_gift_event() {
        let item = serde_json::json!({
            "liveChatSponsorshipsGiftPurchaseAnnouncementRenderer": {
                "id": "g1",
                "header": { "liveChatSponsorshipsHeaderRenderer": {
                    "authorName": { "simpleText": "Eve" },
                    "primaryText": { "runs": [{ "text": "Gifted 5 memberships" }] }
                }}
            }
        });
        match build_item("chan", &item) {
            ParsedItem::Event { kind, text, .. } => {
                assert_eq!(kind, EventKind::Gift);
                assert_eq!(text, "Eve gifted 5 memberships");
            }
            _ => panic!("expected event"),
        }
    }

    #[test]
    fn member_badge_sets_name_color_and_badge() {
        let item = serde_json::json!({
            "liveChatTextMessageRenderer": {
                "id": "abc",
                "timestampUsec": "1700000000000000",
                "authorName": { "simpleText": "Frank" },
                "message": { "runs": [{ "text": "yo" }] },
                "authorBadges": [{
                    "liveChatAuthorBadgeRenderer": {
                        "tooltip": "Member (2 months)",
                        "customThumbnail": { "thumbnails": [{ "url": "//x/badge.png", "width": 16 }] }
                    }
                }]
            }
        });
        match build_item("chan", &item) {
            ParsedItem::Message(msg) => {
                assert_eq!(msg.author.color, Some(MEMBER_NAME_COLOR));
                assert_eq!(msg.author.badges.len(), 1);
                assert_eq!(msg.author.badges[0].url, "https://x/badge.png");
                assert_eq!(
                    msg.author.badges[0].title.as_deref(),
                    Some("Member (2 months)")
                );
            }
            _ => panic!("expected message"),
        }
    }

    #[test]
    fn explicit_name_color_wins() {
        // 0xFF2196F3 → RGB(33,150,243).
        let item = serde_json::json!({
            "liveChatTextMessageRenderer": {
                "id": "abc",
                "timestampUsec": "1700000000000000",
                "authorName": { "simpleText": "Grace" },
                "authorNameTextColor": 4280391411_u32,
                "message": { "runs": [{ "text": "hi" }] }
            }
        });
        match build_item("chan", &item) {
            ParsedItem::Message(msg) => {
                assert_eq!(msg.author.color, Some(Color::rgb(33, 150, 243)));
            }
            _ => panic!("expected message"),
        }
    }

    #[test]
    fn unknown_renderer_is_ignored() {
        let item = serde_json::json!({ "liveChatViewerEngagementMessageRenderer": {} });
        assert!(matches!(build_item("chan", &item), ParsedItem::Ignored));
    }
}
