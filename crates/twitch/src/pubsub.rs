//! Anonymous Twitch channel-point + pinned-message + viewer-count connector
//! over the Hermes WebSocket.
//!
//! Channel-point redemptions, pinned-chat updates, and the live viewer count
//! are *not* delivered over IRC — twitch.tv subscribes to them on
//! `wss://hermes.twitch.tv` using the public web client id (no user auth),
//! which is what lets a logged-out viewer see "X redeemed <reward>", the pinned
//! banner, and the moving viewer number. We do the same: open Hermes, subscribe
//! to `community-points-channel-v1.<channel_id>`,
//! `pinned-chat-updates-v1.<channel_id>` and
//! `video-playback-by-id.<channel_id>`, and translate `reward-redeemed`
//! notifications into highlighted event rows, `pin-message`/`unpin-message`
//! into [`ChatEvent::PinMessage`]/[`ChatEvent::UnpinMessage`], and `viewcount`
//! pushes (every ~30s while live — the exact number the site shows; GQL's
//! `viewersCount` only moves in coarser buckets) into [`ChatEvent::Viewers`].
//!
//! Hermes wraps each notification as `{type:"notification", notification:{pubsub}}`
//! where `pubsub` is itself a JSON *string* (parsed a second time, like Kick's
//! Pusher frames).

use crate::builder::emote_url;
use anyhow::Context;
use bks_core::{plural, Author, Color, Message, MessageElement, Platform};
use bks_platform::{ChatEvent, ChatSink, EventKind};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Hermes endpoint + the public web client id (not a secret; the same id our
/// badge lookup uses). Works anonymously.
const HERMES_URL: &str = "wss://hermes.twitch.tv/v1?clientId=kimne78kx3ncx6brgo4mv6wki5h1ko";

/// The Hermes envelope. We only care about `notification` frames; `keepalive`
/// and `subscribeResponse` are housekeeping.
#[derive(Deserialize)]
struct HermesFrame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    notification: Option<Notification>,
}

#[derive(Deserialize)]
struct Notification {
    /// The inner PubSub payload — a JSON *string* we parse again.
    #[serde(default)]
    pubsub: String,
}

/// The decoded PubSub payload — shared envelope for all three topics; `data`'s
/// shape depends on `type` (`reward-redeemed` vs `pin-message`/`unpin-message`),
/// so it's kept raw and parsed per kind. The video-playback topic's payloads are
/// flat (no `data`): a `viewcount` carries `viewers` at the top level.
#[derive(Deserialize)]
struct PubSubPayload {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    data: serde_json::Value,
    #[serde(default)]
    viewers: Option<u64>,
}

#[derive(Deserialize)]
struct RedeemData {
    redemption: Redemption,
}

#[derive(Deserialize)]
struct Redemption {
    user: RedeemUser,
    reward: Reward,
    /// The viewer's typed message, present only when the reward requires text.
    /// Used to pair the event with its IRC message; absent/empty otherwise.
    #[serde(default)]
    user_input: String,
}

#[derive(Deserialize)]
struct RedeemUser {
    display_name: String,
}

#[derive(Deserialize)]
struct Reward {
    title: String,
    #[serde(default)]
    cost: u64,
}

// ---- `pinned-chat-updates-v1` payloads --------------------------------------
// The shapes below are the (undocumented) web-client ones, so every field is
// defaulted — a partial parse still yields a usable banner, and an unparseable
// payload is logged at debug instead of erroring.

/// `pin-message` (and `update-message`) data: the pin id, who pinned, and the
/// pinned chat message itself.
#[derive(Default, Deserialize)]
struct PinData {
    #[serde(default)]
    pinned_by: PinUser,
    #[serde(default)]
    message: Option<PinnedMessage>,
}

#[derive(Default, Deserialize)]
struct PinUser {
    #[serde(default)]
    display_name: String,
}

#[derive(Default, Deserialize)]
struct PinnedMessage {
    #[serde(default)]
    id: String,
    #[serde(default)]
    sender: PinSender,
    #[serde(default)]
    content: PinContent,
    /// Unix time (seconds) the message was sent; 0 when absent.
    #[serde(default)]
    sent_at: i64,
    /// Unix time (seconds) the pin expires; 0 = until unpinned / stream end.
    #[serde(default)]
    ends_at: i64,
}

#[derive(Default, Deserialize)]
struct PinSender {
    #[serde(default)]
    id: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    chat_color: String,
}

#[derive(Default, Deserialize)]
struct PinContent {
    #[serde(default)]
    text: String,
    #[serde(default)]
    fragments: Vec<PinFragment>,
}

/// One content fragment: plain text, or an emote (the fragment `text` is then
/// the emote code). The emote id key differs across Twitch payload generations,
/// hence the aliases.
#[derive(Default, Deserialize)]
struct PinFragment {
    #[serde(default)]
    text: String,
    #[serde(default)]
    emote: Option<PinEmote>,
}

#[derive(Default, Deserialize)]
struct PinEmote {
    #[serde(default, alias = "emoticonID", alias = "emote_id")]
    id: String,
}

/// Installs rustls's `ring` crypto provider exactly once (the dep tree carries
/// both `ring` and `aws-lc-rs`, so rustls can't choose on its own). Shared with
/// the EventSub socket (`eventsub.rs`).
pub(crate) fn ensure_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Connects to Hermes and forwards channel-point redemptions for `channel_id` as
/// [`ChatEvent::Event`] rows. Runs until the socket closes or `tx` is dropped.
/// Errors are returned to the caller (which posts them as a system notice).
pub async fn run(channel_id: String, tx: ChatSink) -> anyhow::Result<()> {
    ensure_crypto_provider();

    let (ws, _) = tokio_tungstenite::connect_async(HERMES_URL)
        .await
        .context("connecting to Twitch Hermes")?;
    let (mut write, mut read) = ws.split();

    // Subscribe to the channel's community-points + pinned-chat + video-playback
    // topics (one socket, three subscriptions). Hermes wants a unique id per
    // request; a suffix off the channel id is enough here.
    for (tag, topic) in [
        (
            "points",
            format!("community-points-channel-v1.{channel_id}"),
        ),
        ("pins", format!("pinned-chat-updates-v1.{channel_id}")),
        ("vc", format!("video-playback-by-id.{channel_id}")),
    ] {
        let subscribe = format!(
            r#"{{"type":"subscribe","id":"sub-{tag}-{channel_id}","subscribe":{{"id":"ps-{tag}-{channel_id}","type":"pubsub","pubsub":{{"topic":"{topic}"}}}}}}"#
        );
        write
            .send(WsMessage::Text(subscribe))
            .await
            .with_context(|| format!("subscribing to {topic}"))?;
    }

    while let Some(frame) = read.next().await {
        // The UI dropped the receiver (tab closed / reconnected): close the
        // socket instead of holding it open for the rest of the session.
        if tx.is_closed() {
            break;
        }
        let text = match frame.context("hermes websocket error")? {
            WsMessage::Text(t) => t,
            WsMessage::Ping(p) => {
                let _ = write.send(WsMessage::Pong(p)).await;
                continue;
            }
            WsMessage::Close(_) => break,
            _ => continue,
        };

        let Ok(envelope) = serde_json::from_str::<HermesFrame>(&text) else {
            continue;
        };
        if envelope.kind != "notification" {
            continue; // keepalive / subscribeResponse — nothing to do.
        }
        let Some(notification) = envelope.notification else {
            continue;
        };
        let Ok(payload) = serde_json::from_str::<PubSubPayload>(&notification.pubsub) else {
            continue;
        };
        let event = match payload.kind.as_str() {
            "reward-redeemed" => {
                let Ok(data) = serde_json::from_value::<RedeemData>(payload.data) else {
                    continue;
                };
                ChatEvent::Event {
                    platform: Platform::Twitch,
                    kind: EventKind::Reward,
                    text: redeem_text(&data.redemption),
                    timestamp: chrono::Utc::now(),
                    // The viewer's message (for a text-requiring reward) arrives
                    // separately over IRC (with badges/emotes); the UI pairs it
                    // with this event and renders it under the header. So no
                    // message is attached here.
                    message: None,
                    details: redeem_details(&data.redemption),
                }
            }
            // A new pin, or an existing pin's duration updated — both carry the
            // full message, and one mod pin is active at a time, so both just
            // (re)set the banner.
            "pin-message" | "update-message" => {
                match serde_json::from_value::<PinData>(payload.data) {
                    Ok(data) => match pin_event(data) {
                        Some(event) => event,
                        None => continue, // update without the message — keep the current banner
                    },
                    Err(err) => {
                        tracing::debug!(
                            "unparsed twitch pin payload ({err}): {}",
                            notification.pubsub
                        );
                        continue;
                    }
                }
            }
            "unpin-message" => ChatEvent::UnpinMessage {
                platform: Platform::Twitch,
            },
            // The site's live viewer number, pushed every ~30s while
            // broadcasting. `stream-down` is deliberately NOT mapped to a
            // count-clear: the topic also fires it on ad transitions / brief
            // encoder drops mid-stream, which blanked a valid number; a real
            // offline is cleared by the IVR poll's `Live { live: false }`.
            "viewcount" => {
                let Some(count) = payload.viewers else {
                    continue;
                };
                tracing::debug!("twitch viewer count for channel {channel_id}: {count}");
                ChatEvent::Viewers {
                    platform: Platform::Twitch,
                    count: Some(count),
                }
            }
            _ => continue,
        };
        if tx.send(event).is_err() {
            break; // UI dropped the receiver.
        }
    }

    Ok(())
}

/// A pin payload's unix timestamp as UTC; `None` for 0/absent. Values that look
/// like milliseconds (some payload generations) are scaled down.
fn pin_time(secs: i64) -> Option<chrono::DateTime<chrono::Utc>> {
    if secs <= 0 {
        return None;
    }
    let secs = if secs > 1_000_000_000_000 {
        secs / 1000
    } else {
        secs
    };
    chrono::DateTime::from_timestamp(secs, 0)
}

/// Builds the [`ChatEvent::PinMessage`] for a `pin-message`/`update-message`
/// payload, turning the content fragments into text/emote tokens. `None` when
/// the payload carries no message (nothing to show).
fn pin_event(data: PinData) -> Option<ChatEvent> {
    let msg = data.message?;
    let mut elements: Vec<MessageElement> = Vec::new();
    for fragment in &msg.content.fragments {
        match &fragment.emote {
            Some(emote) if !emote.id.is_empty() => {
                elements.push(MessageElement::Emote(std::sync::Arc::new(
                    bks_core::Emote {
                        url: emote_url(&emote.id),
                        id: emote.id.clone(),
                        name: fragment.text.clone(),
                        animated: false,
                        tooltip: bks_core::EmoteTooltip::provider("Twitch"),
                    },
                )));
            }
            _ if !fragment.text.is_empty() => {
                elements.push(MessageElement::Text {
                    text: fragment.text.clone(),
                    color: None,
                });
            }
            _ => {}
        }
    }
    // A payload with no fragments still has the plain text.
    if elements.is_empty() && !msg.content.text.is_empty() {
        elements.push(MessageElement::Text {
            text: msg.content.text.clone(),
            color: None,
        });
    }
    let elements = bks_core::mentionize(bks_core::linkify(elements));
    let ends_at = pin_time(msg.ends_at);
    let message = Message {
        id: msg.id,
        platform: Platform::Twitch,
        channel: String::new(),
        timestamp: pin_time(msg.sent_at).unwrap_or_else(chrono::Utc::now),
        author: Author {
            login: msg.sender.display_name.to_lowercase(),
            display_name: msg.sender.display_name,
            color: Color::from_hex(&msg.sender.chat_color),
            badges: Vec::new(),
            paint: None,
            user_id: msg.sender.id,
        },
        raw_text: msg.content.text,
        elements,
        reply: None,
        first_message: false,
        highlighted: false,
        historical: false,
        reward_id: None,
    };
    Some(ChatEvent::PinMessage {
        platform: Platform::Twitch,
        pinned_by: data.pinned_by.display_name,
        message: Box::new(message),
        ends_at,
    })
}

/// "X redeemed <reward> (N points)." (singular when N == 1).
fn redeem_text(r: &Redemption) -> String {
    let unit = plural(r.reward.cost, "point", "points");
    format!(
        "{} redeemed {} ({} {unit}).",
        r.user.display_name, r.reward.title, r.reward.cost
    )
}

/// Condensed form for the events panel: "redeemed <reward> · N pts".
fn redeem_details(r: &Redemption) -> bks_platform::EventDetails {
    let input = r.user_input.trim();
    bks_platform::EventDetails {
        actor: Some(r.user.display_name.clone()),
        compact: Some(format!("redeemed {} · {} pts", r.reward.title, r.reward.cost)),
        redeem_input: (!input.is_empty()).then(|| input.to_string()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redemption(name: &str, title: &str, cost: u64) -> Redemption {
        Redemption {
            user: RedeemUser {
                display_name: name.into(),
            },
            reward: Reward {
                title: title.into(),
                cost,
            },
            user_input: String::new(),
        }
    }

    #[test]
    fn redeem_details_carries_trimmed_user_input() {
        let mut r = redemption("alice", "Say something", 50);
        assert_eq!(redeem_details(&r).redeem_input, None);

        r.user_input = "  gg wp  ".into();
        assert_eq!(
            redeem_details(&r).redeem_input.as_deref(),
            Some("gg wp")
        );
    }

    #[test]
    fn redeem_text_singular_and_plural() {
        assert_eq!(
            redeem_text(&redemption("alice", "testtest", 1)),
            "alice redeemed testtest (1 point)."
        );
        assert_eq!(
            redeem_text(&redemption("alice", "Hydrate", 500)),
            "alice redeemed Hydrate (500 points)."
        );
    }

    #[test]
    fn parses_real_notification_shape() {
        // The double-encoded shape captured live from Hermes.
        let inner = r#"{"type":"reward-redeemed","data":{"redemption":{"user":{"display_name":"alice"},"reward":{"title":"testtest","cost":1}}}}"#;
        let payload: PubSubPayload = serde_json::from_str(inner).unwrap();
        assert_eq!(payload.kind, "reward-redeemed");
        let data: RedeemData = serde_json::from_value(payload.data).unwrap();
        assert_eq!(
            redeem_text(&data.redemption),
            "alice redeemed testtest (1 point)."
        );
    }

    #[test]
    fn parses_flat_viewcount_payload() {
        // Captured live from Hermes `video-playback-by-id`: flat, no `data`.
        let inner = r#"{"type":"viewcount","server_time":1783794771.584091,"viewers":4090,"collaboration_status":"none","collaboration_viewers":0,"costream_status":"","costream_viewers":0}"#;
        let payload: PubSubPayload = serde_json::from_str(inner).unwrap();
        assert_eq!(payload.kind, "viewcount");
        assert_eq!(payload.viewers, Some(4090));
    }

    #[test]
    fn pin_message_payload_becomes_pin_event() {
        let inner = r##"{
            "type": "pin-message",
            "data": {
                "id": "pin-1",
                "pinned_by": {"id": "100", "display_name": "ModName"},
                "message": {
                    "id": "msg-1",
                    "sender": {"id": "200", "display_name": "Chatter", "chat_color": "#FF0000"},
                    "content": {
                        "text": "hello Kappa",
                        "fragments": [
                            {"text": "hello "},
                            {"text": "Kappa", "emote": {"emoticonID": "25"}}
                        ]
                    },
                    "type": "MOD",
                    "starts_at": 1700000000,
                    "ends_at": 1700001200,
                    "sent_at": 1699999990
                }
            }
        }"##;
        let payload: PubSubPayload = serde_json::from_str(inner).unwrap();
        assert_eq!(payload.kind, "pin-message");
        let data: PinData = serde_json::from_value(payload.data).unwrap();
        let ChatEvent::PinMessage {
            platform,
            pinned_by,
            message,
            ends_at,
        } = pin_event(data).unwrap()
        else {
            panic!("expected PinMessage");
        };
        assert_eq!(platform, Platform::Twitch);
        assert_eq!(pinned_by, "ModName");
        assert_eq!(message.id, "msg-1");
        assert_eq!(message.author.display_name, "Chatter");
        assert_eq!(message.author.color, Color::from_hex("#FF0000"));
        assert_eq!(message.raw_text, "hello Kappa");
        assert_eq!(message.elements.len(), 2);
        match &message.elements[1] {
            MessageElement::Emote(e) => {
                assert_eq!(e.name, "Kappa");
                assert!(e.url.contains("/25/"));
            }
            other => panic!("expected emote, got {other:?}"),
        }
        assert_eq!(ends_at, chrono::DateTime::from_timestamp(1700001200, 0));
        assert_eq!(
            message.timestamp,
            chrono::DateTime::from_timestamp(1699999990, 0).unwrap()
        );
    }

    #[test]
    fn pin_event_without_message_is_skipped() {
        // An `update-message` may carry only the pin id — nothing to (re)show.
        let data: PinData =
            serde_json::from_value(serde_json::json!({"id": "pin-1", "ends_at": 1700001200}))
                .unwrap();
        assert!(pin_event(data).is_none());
    }

    #[test]
    fn pin_without_fragments_falls_back_to_plain_text() {
        let data: PinData = serde_json::from_value(serde_json::json!({
            "pinned_by": {"display_name": "Mod"},
            "message": {"id": "m", "content": {"text": "plain words"}, "ends_at": 0}
        }))
        .unwrap();
        let ChatEvent::PinMessage {
            message, ends_at, ..
        } = pin_event(data).unwrap()
        else {
            panic!("expected PinMessage");
        };
        assert_eq!(ends_at, None);
        assert_eq!(message.raw_text, "plain words");
        assert!(
            matches!(&message.elements[0], MessageElement::Text { text, .. } if text == "plain words")
        );
    }

    #[test]
    fn pin_time_handles_zero_seconds_and_millis() {
        assert_eq!(pin_time(0), None);
        assert_eq!(pin_time(-5), None);
        assert_eq!(
            pin_time(1700000000),
            chrono::DateTime::from_timestamp(1700000000, 0)
        );
        // Millisecond-looking values are scaled down.
        assert_eq!(
            pin_time(1700000000000),
            chrono::DateTime::from_timestamp(1700000000, 0)
        );
    }
}
