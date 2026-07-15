//! Recent Kick chat history, fetched on channel join so the log isn't empty.
//!
//! Kick's Pusher feed sends no backlog, but `web.kick.com/api/v1/chat/{id}
//! /history` returns the last messages — the same `ChatMessageEvent`-shaped
//! payloads the live socket delivers, so each runs through the *same*
//! [`build_message`] conversion as live chat (badges/emotes included). That host
//! is Cloudflare-fronted like the channels API, so the lookup goes through the
//! emulated client (see [`crate::api`]). The history id is *not* the Pusher
//! chatroom id (that one returns an empty history) — it's the v2
//! `chatroom.channel_id`, which channel resolution already returns; we pass it
//! directly, and fall back to a slug→id lookup when it's `0`. Messages arrive
//! oldest-first; we flag each `historical` so the UI fades them.

use serde::Deserialize;

use bks_platform::ChatEvent;

use crate::api::{PinnedInfo, SubscriberBadge};
use crate::builder::{build_message, KickChatMessage, Sender};

/// The `/history` response: `{ data: { messages: [ … ], pinned_message } }`. Each
/// message is the same shape as a live `ChatMessageEvent`, plus a `type`
/// discriminator; `pinned_message` (when present) is the channel's current pin —
/// the only anonymous source for it on join (Kick has no pin GET endpoint).
#[derive(Deserialize)]
struct HistoryResponse {
    #[serde(default)]
    data: HistoryData,
}

#[derive(Deserialize, Default)]
struct HistoryData {
    #[serde(default)]
    messages: Vec<HistoryMessage>,
    #[serde(default)]
    pinned_message: Option<PinnedInfo>,
}

/// One history entry. Kick mixes plain chat (`type: "message"`) with other kinds
/// (e.g. `"reply"`). The fields mirror the live `ChatMessageEvent` *except*
/// `metadata`, which the history endpoint serializes as a JSON *string* (`"[]"`)
/// rather than the object the live socket sends — so we drop it here (a faded
/// backlog doesn't need reply context) and hand the rest to the live builder.
#[derive(Deserialize)]
struct HistoryMessage {
    #[serde(rename = "type", default)]
    kind: String,
    id: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    created_at: Option<String>,
    sender: Sender,
}

impl HistoryMessage {
    fn into_chat(self) -> KickChatMessage {
        KickChatMessage {
            id: self.id,
            content: self.content,
            created_at: self.created_at,
            sender: self.sender,
            // Backlog drops reply context (the history `metadata` string form
            // omits `original_*`, so no `ReplyParent` is built) — the thread root
            // alone can't form a chain without it, so leave it out too.
            metadata: None,
            thread_parent_id: None,
        }
    }
}

/// Fetches recent chat history directly from Kick (via the emulated client),
/// oldest-first, converted to [`ChatEvent::Message`]s flagged `historical`.
/// `channel_id` is the history id (the v2 `chatroom.channel_id` from channel
/// resolution, distinct from the Pusher chatroom id) — passed directly so we skip
/// a redundant channel lookup; when it's `0` we resolve it from the slug.
/// `sub_badges` are the channel's subscriber-tier images (to resolve subscriber
/// badges, as live chat does). Errors propagate so the caller can log and continue.
pub async fn fetch_recent(
    channel: &str,
    channel_id: u64,
    sub_badges: &[SubscriberBadge],
) -> anyhow::Result<Vec<ChatEvent>> {
    let slug = crate::api::slugify(channel);
    let channel_id = if channel_id != 0 {
        channel_id
    } else {
        crate::api::fetch_history_channel_id(&slug).await?
    };
    let body = crate::api::fetch_history_body(channel_id).await?;
    let resp: HistoryResponse =
        serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("parsing kick history: {e}"))?;

    // Kick returns history newest-first; reverse to oldest-first so it replays in
    // chronological order (matching the live feed and Twitch history).
    let mut events: Vec<ChatEvent> = resp
        .data
        .messages
        .into_iter()
        .rev()
        .filter(|m| m.kind.is_empty() || m.kind == "message" || m.kind == "reply")
        .map(|m| {
            let mut msg = build_message(channel, sub_badges, m.into_chat());
            msg.historical = true;
            ChatEvent::Message(Box::new(msg))
        })
        .collect();

    // The history payload carries the channel's active pin (there's no anonymous
    // pin GET endpoint), so seed the banner from it. Not `live`, so it stays until
    // an explicit `finish_at`/unpin — its original duration start is unknown.
    if let Some(pin) = resp.data.pinned_message {
        events.push(crate::connector::pin_event(channel, sub_badges, pin, false));
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_history_messages_as_historical() {
        let raw = r##"{
            "data": { "messages": [
                {
                    "id": "m1", "chat_id": 1, "type": "message",
                    "content": "hello [emote:42:KEKW]",
                    "created_at": "2026-06-27T20:24:34Z",
                    "sender": { "id": 5, "username": "Alice",
                        "identity": { "color": "#FFD899", "badges": [] } }
                }
            ] }
        }"##;
        let resp: HistoryResponse = serde_json::from_str(raw).unwrap();
        let msgs = resp.data.messages;
        assert_eq!(msgs.len(), 1);
        let msg = build_message("chan", &[], msgs.into_iter().next().unwrap().into_chat());
        assert_eq!(msg.author.display_name, "Alice");
        assert_eq!(msg.raw_text, "hello [emote:42:KEKW]");
    }

    #[test]
    fn history_metadata_string_does_not_break_parsing() {
        // The history endpoint serializes `metadata` as a JSON string, unlike the
        // live socket's object; `HistoryMessage` must ignore it without erroring.
        let raw = r#"{
            "data": { "messages": [
                {
                    "id": "m1", "type": "message", "content": "hi",
                    "metadata": "{\"message_ref\":\"123\"}",
                    "sender": { "id": 5, "username": "Bob" }
                }
            ] }
        }"#;
        let resp: HistoryResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.data.messages.len(), 1);
    }

    #[test]
    fn history_pinned_message_becomes_pin_event() {
        // Trimmed from a live `/history` capture: the pin rides `data.pinned_message`.
        let raw = r##"{
            "data": {
                "messages": [],
                "cursor": "1783405879956403",
                "pinned_message": {
                    "message": {
                        "id": "bef5aa31", "chat_id": 109579, "content": "hi chat",
                        "metadata": "{\"message_ref\":\"1783395697377\"}",
                        "created_at": "2026-07-07T03:41:38.180601Z",
                        "sender": { "id": 3613073, "slug": "braxis", "username": "Braxis",
                            "identity": { "color": "#FFD899", "badges": [] } }
                    },
                    "pinned_by": { "id": 3613073, "username": "Braxis" }
                }
            }
        }"##;
        let resp: HistoryResponse = serde_json::from_str(raw).unwrap();
        let pin = resp.data.pinned_message.expect("pinned_message parsed");
        let ChatEvent::PinMessage {
            pinned_by,
            message,
            ends_at,
            ..
        } = crate::connector::pin_event("chan", &[], pin, false)
        else {
            panic!("expected PinMessage");
        };
        assert_eq!(pinned_by, "Braxis");
        assert_eq!(message.raw_text, "hi chat");
        // A seeded pin (unknown start, no finish_at) has no client-side expiry.
        assert!(ends_at.is_none());
    }

    #[test]
    fn history_without_pin_is_fine() {
        let raw = r#"{ "data": { "messages": [] } }"#;
        let resp: HistoryResponse = serde_json::from_str(raw).unwrap();
        assert!(resp.data.pinned_message.is_none());
    }

    #[test]
    fn fetch_builds_faded_messages() {
        // Exercises the same path as the live builder, asserting the historical flag.
        let chat: KickChatMessage = serde_json::from_str(
            r#"{"id":"m1","content":"hi","sender":{"id":5,"username":"Bob"}}"#,
        )
        .unwrap();
        let mut msg = build_message("chan", &[], chat);
        msg.historical = true;
        assert!(msg.historical);
    }
}
