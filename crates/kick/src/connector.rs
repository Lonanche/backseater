//! Anonymous Kick chat connector over Pusher WebSocket.
//!
//! Kick chat is a Pusher app. We resolve the channel's chatroom id via the REST
//! API ([`crate::api`]), open the public Pusher socket, subscribe to
//! `chatrooms.{id}.v2`, and translate `App\Events\ChatMessageEvent` frames into
//! [`Message`]s. Pusher wraps each event as `{event, channel, data}` where
//! `data` is itself a JSON *string* that we parse a second time.

use anyhow::Context;
use async_trait::async_trait;
use bks_core::{plural, Message, Platform};
use bks_platform::{ChannelMeta, ChatEvent, ChatSink, ChatSource, ChatStream, EventDetails, EventKind};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::api::{self, parse_kick_time, PinnedInfo, SubscriberBadge};
use crate::builder::{build_message, KickChatMessage};

/// Public Pusher endpoint + app key Kick uses for chat (same as the C++ client).
const PUSHER_URL: &str = "wss://ws-us2.pusher.com/app/32cbd69e4b950bf97679\
?protocol=7&client=js&version=8.4.0&flash=false";

/// Anonymous Kick connector. Each `join` resolves the channel and runs its own
/// Pusher socket; events cross to the UI only through the returned stream.
/// Channel resolution + history hit Kick's Cloudflare-fronted endpoints directly
/// via the emulated client (see [`crate::api`]) — no broker needed for reads.
#[derive(Default)]
pub struct KickSource;

impl KickSource {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ChatSource for KickSource {
    async fn join(&self, channel: &str) -> anyhow::Result<ChatStream> {
        let channel = api::slugify(channel);
        let (tx, rx) = mpsc::unbounded_channel();

        // A failed connection (network drop, Pusher hiccup) is retried with
        // capped exponential backoff instead of leaving the tab dead. The task
        // ends when the UI drops the receiver. History is fetched only on the
        // first attempt — a reconnect re-fetching it would duplicate the backlog.
        tokio::spawn(async move {
            let mut attempt: u32 = 0;
            loop {
                let started = std::time::Instant::now();
                let result = run_client(channel.clone(), tx.clone(), attempt == 0).await;
                if tx.is_closed() {
                    break; // tab gone — no retry
                }
                // `Ok` here means the server closed the socket cleanly while the
                // tab is still alive (Pusher does drop idle connections):
                // reconnect too, just without an error row.
                let err = result.err();
                // A connection that held for a while is a fresh outage, not a
                // continuation of the previous backoff.
                if started.elapsed() > std::time::Duration::from_secs(60) {
                    attempt = 0;
                }
                let delay = bks_core::reconnect_delay(attempt);
                match (&err, attempt) {
                    // First failure of an outage is user-visible; the retries
                    // behind it are just logged so a flapping network doesn't
                    // fill the chat with error rows.
                    (Some(err), 0) => {
                        let _ = tx.send(ChatEvent::Error(format!(
                            "kick error: {err:#} — reconnecting in {}s",
                            delay.as_secs()
                        )));
                    }
                    (Some(err), _) => {
                        tracing::warn!("kick reconnect attempt {attempt} failed: {err:#}")
                    }
                    (None, _) => {
                        tracing::warn!("kick connection to {channel} closed; reconnecting")
                    }
                }
                attempt += 1;
                tokio::time::sleep(delay).await;
            }
        });

        Ok(rx)
    }
}

/// The Pusher envelope: `{event, data}` with `data` a JSON string (or `"{}"`).
/// We only decode `data` for events we handle. (Kick omits the standard Pusher
/// top-level `channel` field, even on subscription confirmations.)
#[derive(Deserialize)]
struct PusherFrame {
    event: String,
    #[serde(default)]
    data: String,
}

/// `UserBannedEvent` data: the banned user, the moderator (optional), and
/// whether it's a permanent ban or a timeout (`duration` is in minutes).
#[derive(Deserialize)]
struct UserBannedEvent {
    user: KickUserRef,
    #[serde(default)]
    banned_by: Option<KickUserRef>,
    #[serde(default)]
    permanent: bool,
    #[serde(default)]
    duration: u64,
}

/// `UserUnbannedEvent` data: the unbanned user and the moderator (optional).
#[derive(Deserialize)]
struct UserUnbannedEvent {
    user: KickUserRef,
    #[serde(default)]
    unbanned_by: Option<KickUserRef>,
}

#[derive(Deserialize)]
struct KickUserRef {
    username: String,
}

/// `SubscriptionEvent` data: a user (re)subscribing for `months` total.
#[derive(Deserialize)]
struct SubscriptionEvent {
    username: String,
    #[serde(default)]
    months: u64,
}

/// `ChatMessageSentEvent` data: the sibling frame Kick fires alongside a
/// (re)subscription. It's the **only** frame that carries the resub's attached
/// message (`message.optional_message`) — `SubscriptionEvent` has just the month
/// count. We only care about the `action:"subscribe"` variant with a non-empty
/// message; it's buffered by username and re-attached when the paired
/// `SubscriptionEvent` arrives a moment later (see the subscribe handling and
/// KICK_EVENTS.md).
#[derive(Deserialize)]
struct ChatMessageSentEvent {
    message: SentMessage,
    user: SentUser,
}

#[derive(Deserialize)]
struct SentMessage {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    optional_message: Option<String>,
}

#[derive(Deserialize)]
struct SentUser {
    id: u64,
    username: String,
}

/// A resub message buffered from `ChatMessageSentEvent`, waiting for its paired
/// `SubscriptionEvent` to arrive so it can be attached as a chat line.
struct PendingSubMessage {
    user_id: u64,
    username: String,
    text: String,
}

/// `GiftedSubscriptionsEvent` data: `gifter_username` gifted to
/// `gifted_usernames`, having gifted `gifter_total` in the channel overall.
#[derive(Deserialize)]
struct GiftedSubscriptionsEvent {
    #[serde(default)]
    gifter_username: Option<String>,
    #[serde(default)]
    gifted_usernames: Vec<String>,
    #[serde(default)]
    gifter_total: u64,
}

/// `StreamHostEvent` data: another channel hosting this one (Kick's "raid").
#[derive(Deserialize)]
struct StreamHostEvent {
    host_username: String,
    #[serde(default)]
    number_viewers: u64,
}

/// `KicksGifted` data: a "Kicks" gift (Kick's bits/cheer equivalent).
#[derive(Deserialize)]
struct KicksGiftedEvent {
    sender: KickUserRef,
    gift: KicksGift,
}

#[derive(Deserialize)]
struct KicksGift {
    #[serde(default)]
    name: String,
    #[serde(default)]
    amount: u64,
}

/// `RewardRedeemedEvent` data: a channel-point reward redemption.
#[derive(Deserialize)]
struct RewardRedeemedEvent {
    #[serde(default)]
    reward_title: String,
    username: String,
}

/// `StreamerIsLive` data: the broadcaster went live. The nested `livestream`
/// carries the new session's title + start time. Arrives on the `channel.{id}`
/// Pusher channel we already subscribe to — this is what replaces the live poll.
#[derive(Deserialize)]
struct StreamerIsLiveEvent {
    livestream: StreamerIsLiveData,
}

#[derive(Deserialize)]
struct StreamerIsLiveData {
    #[serde(default)]
    session_title: String,
    /// RFC-3339 start time of the stream (covered by `parse_kick_time`'s fallback).
    #[serde(default)]
    created_at: String,
}

/// `MessageDeletedEvent` data: one message removed by a mod or auto-moderation.
/// `violated_rules` is set when AI moderation flagged it (e.g. `["hate"]`).
#[derive(Deserialize)]
struct MessageDeletedEvent {
    message: MessageRef,
    #[serde(default, rename = "aiModerated")]
    ai_moderated: bool,
    #[serde(default, rename = "violatedRules")]
    violated_rules: Vec<String>,
}

#[derive(Deserialize)]
struct MessageRef {
    id: String,
}

/// Installs rustls's `ring` crypto provider exactly once. Required because the
/// dependency tree pulls in both `ring` and `aws-lc-rs`, so rustls can't choose
/// a default on its own and TLS would otherwise panic.
fn ensure_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// One connection attempt: resolve the channel, subscribe over Pusher, pump
/// events until the socket dies (`Err`, retried by the caller) or closes/the
/// tab is gone (`Ok`). `fetch_history` is true only on the first attempt — the
/// backlog would duplicate if a reconnect replayed it.
async fn run_client(channel: String, tx: ChatSink, fetch_history: bool) -> anyhow::Result<()> {
    ensure_crypto_provider();

    let info = api::fetch_channel_info(&channel)
        .await
        .with_context(|| format!("resolving kick channel {channel}"))?;

    let (ws, _) = tokio_tungstenite::connect_async(PUSHER_URL)
        .await
        .context("connecting to Kick Pusher")?;
    let (mut write, mut read) = ws.split();

    // Kick spreads its (undocumented) Pusher feed across several channels with
    // *inconsistent* routing and naming: chat + bans arrive on `chatrooms.X.v2`,
    // but other public events (subs/gifts/hosts/kicks/rewards) can land on the
    // legacy `chatroom_X` / `chatrooms.X` channels or the broadcaster's
    // `channel.X` / `channel_X` — observed live (e.g. RewardRedeemedEvent on
    // `chatroom_X`). So we subscribe to all of them; Pusher
    // silently ignores any that don't exist.
    //
    // The broadcaster `channel.X` form is keyed by the v2 `chatroom.channel_id`
    // (== `info.channel_id`), NOT the `user_id` — confirmed live: `StreamerIsLive`
    // arrives on `channel.{channel_id}`. That event lands *only* there, so this id
    // must be right or live-status (now push-based, no poll) silently never fires.
    // We keep the `user_id` variants too, since other events were observed on them.
    let subscribed = [
        format!("chatrooms.{}.v2", info.chatroom_id),
        format!("chatrooms.{}", info.chatroom_id),
        format!("chatroom_{}", info.chatroom_id),
        format!("channel.{}", info.channel_id),
        format!("channel_{}", info.channel_id),
        format!("channel.{}", info.user_id),
        format!("channel_{}", info.user_id),
    ];
    for channel_name in &subscribed {
        let subscribe = format!(
            r#"{{"event":"pusher:subscribe","data":{{"auth":"","channel":"{channel_name}"}}}}"#
        );
        write
            .send(WsMessage::Text(subscribe))
            .await
            .with_context(|| format!("subscribing to {channel_name}"))?;
    }

    let _ = tx.send(ChatEvent::System(format!("connected to kick #{channel}")));
    let _ = tx.send(ChatEvent::Channel(ChannelMeta {
        platform: Platform::Kick,
        id: info.user_id.to_string(),
        name: channel.clone(),
    }));

    // Seed the initial live state from the join lookup: when live, so opening a tab
    // on an already-live stream shows it immediately (the live poll's first check
    // used to do this); when offline, to carry the last-stream info (most recent
    // VOD) the tooltip shows. After this, `StreamerIsLive`/`StopStreamBroadcast` on
    // the `channel.{id}` Pusher subscription drive transitions with no polling.
    // This offline seed is the only Live event with no live↔offline flip, so the UI
    // updates the stored status without pushing a duplicate "offline" notice row.
    {
        let last_stream = info
            .last_stream
            .as_ref()
            .map(|ls| bks_platform::LastStream {
                started_at: ls.started_at,
                ended_at: Some(ls.ended_at),
                title: ls.title.clone(),
                game: ls.category.clone(),
            });
        let _ = tx.send(ChatEvent::Live {
            platform: Platform::Kick,
            live: info.is_live,
            title: info.livestream_title.clone(),
            game: info.livestream_category.clone(),
            started_at: info.livestream_started_at,
            last_stream,
            link: None,
        });
    }

    // The pinned banner is seeded from the history payload's `pinned_message`
    // (Kick has no anonymous pin GET endpoint) — see `history::fetch_recent`. A pin
    // created/removed while connected arrives live as `PinnedMessageCreated/
    // DeletedEvent`.

    // Kick's Pusher feed sends no backlog, so fetch recent history (direct from
    // Kick) and replay it as faded messages. Run it in its own task so the
    // round-trip doesn't delay the live read loop below — the UI sorts `historical`
    // messages ahead of live ones by timestamp, so order across the two is fine.
    // The `Channel` event was already sent above (the bridge has loaded emotes).
    if fetch_history {
        let (tx, channel) = (tx.clone(), channel.clone());
        let sub_badges = info.subscriber_badges.clone();
        let channel_id = info.channel_id;
        tokio::spawn(async move {
            match crate::history::fetch_recent(&channel, channel_id, &sub_badges).await {
                Ok(events) => {
                    for event in events {
                        if tx.send(event).is_err() {
                            return; // UI dropped the stream.
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!("kick history fetch failed: {err:#}");
                }
            }
        });
    }

    // A resub's typed message rides `ChatMessageSentEvent`, which fires just
    // *before* the paired `SubscriptionEvent`; buffer the latest such message per
    // username so the `SubscriptionEvent` arm can attach it (rendered under the
    // sub line, Twitch-style). Keyed by lowercased username, cleared on use.
    let mut pending_sub_messages: std::collections::HashMap<String, PendingSubMessage> =
        std::collections::HashMap::new();

    while let Some(frame) = read.next().await {
        // The UI dropped the receiver (tab closed): only the chat-message send
        // would notice, so a quiet channel could hold the socket open for hours.
        if tx.is_closed() {
            break;
        }
        let frame = frame.context("kick websocket error")?;
        let text = match frame {
            WsMessage::Text(t) => t,
            WsMessage::Ping(p) => {
                let _ = write.send(WsMessage::Pong(p)).await;
                continue;
            }
            WsMessage::Close(_) => break,
            _ => continue,
        };

        let Ok(envelope) = serde_json::from_str::<PusherFrame>(&text) else {
            continue;
        };

        // Kick sends the same event under two name forms depending on which
        // channel it arrives on: prefixed (`App\Events\RewardRedeemedEvent` on
        // `chatrooms.X.v2`) and bare (`RewardRedeemedEvent` on legacy
        // `chatroom_X`) — observed live. Strip the prefix so one arm handles both.
        let event = envelope
            .event
            .strip_prefix("App\\Events\\")
            .unwrap_or(&envelope.event);

        // Log every event (name + raw data) at debug except the high-volume plain
        // chat messages (they'd drown out everything else); run with
        // RUST_LOG=bks_kick=debug (or RUST_LOG=debug) to see them. Sub events get a
        // louder, clearly-marked info-level dump of the full raw structure so an
        // attached resub message (if Kick sends one) is visible without enabling debug.
        if event != "ChatMessageEvent" {
            tracing::debug!(kind = %event, "kick event: {}", envelope.data);
        }
        if event == "SubscriptionEvent" {
            tracing::info!(
                "=== KICK SUB EVENT (full raw structure) === {}",
                envelope.data
            );
        }

        match event {
            // Keep the Pusher connection alive.
            "pusher:ping" => {
                let _ = write
                    .send(WsMessage::Text(
                        r#"{"event":"pusher:pong","data":{}}"#.to_string(),
                    ))
                    .await;
            }
            // Subscription confirmations: Kick's frame carries no channel name and
            // we subscribe to several redundant aliases, so a per-channel feed
            // notice would just be noise — the debug log above covers it.
            "pusher_internal:subscription_succeeded" => {}
            // A failed subscribe — surface it so a wrong channel id is obvious.
            "pusher:error" | "pusher:subscription_error" => {
                let _ = tx.send(ChatEvent::Error(format!(
                    "kick: subscription error: {}",
                    envelope.data
                )));
            }
            "ChatMessageEvent" => {
                let Ok(chat) = serde_json::from_str::<KickChatMessage>(&envelope.data) else {
                    continue;
                };
                let message = build_message(&channel, &info.subscriber_badges, chat);
                if tx.send(ChatEvent::Message(Box::new(message))).is_err() {
                    break; // UI dropped the stream.
                }
            }
            // Ban/timeout: fade the target's messages (ClearChat) and post a
            // notice naming the moderator + duration when known.
            "UserBannedEvent" => {
                if let Ok(ev) = serde_json::from_str::<UserBannedEvent>(&envelope.data) {
                    let _ = tx.send(ChatEvent::ClearChat {
                        platform: Platform::Kick,
                        user: Some(ev.user.username.clone()),
                        historical: false,
                    });
                    let _ = tx.send(ChatEvent::Notice(ban_notice(&ev)));
                }
            }
            // Unban/untimeout: a notice naming who was unbanned by whom. Past
            // messages stay struck (we don't un-fade them).
            "UserUnbannedEvent" => {
                if let Ok(ev) = serde_json::from_str::<UserUnbannedEvent>(&envelope.data) {
                    let _ = tx.send(ChatEvent::Notice(unban_notice(&ev)));
                }
            }
            // A single deleted message: strike + fade that row, and note when AI
            // moderation removed it (with the rules it flagged, if any).
            "MessageDeletedEvent" => {
                if let Ok(ev) = serde_json::from_str::<MessageDeletedEvent>(&envelope.data) {
                    let _ = tx.send(ChatEvent::DeleteMessage {
                        platform: Platform::Kick,
                        message_id: ev.message.id.clone(),
                    });
                    if let Some(notice) = deletion_notice(&ev) {
                        let _ = tx.send(ChatEvent::Notice(notice));
                    }
                }
            }
            // Public sub/gift events → highlighted event rows.
            "SubscriptionEvent" => {
                if let Ok(ev) = serde_json::from_str::<SubscriptionEvent>(&envelope.data) {
                    // The sub's typed message (if any) rode the preceding
                    // `ChatMessageSentEvent`; attach it as a chat line under the sub
                    // text, the same way Twitch resubs render.
                    let message = pending_sub_messages
                        .remove(&ev.username.to_lowercase())
                        .map(|pending| {
                            Box::new(sub_message(&channel, &info.subscriber_badges, pending))
                        });
                    let _ = tx.send(ChatEvent::Event {
                        platform: Platform::Kick,
                        kind: EventKind::Sub,
                        text: sub_event_text(&ev),
                        timestamp: chrono::Utc::now(),
                        message,
                        details: sub_event_details(&ev),
                    });
                }
            }
            "GiftedSubscriptionsEvent" => {
                if let Ok(ev) = serde_json::from_str::<GiftedSubscriptionsEvent>(&envelope.data) {
                    if let Some(text) = gift_event_text(&ev) {
                        let _ = tx.send(ChatEvent::Event {
                            platform: Platform::Kick,
                            kind: EventKind::Gift,
                            text,
                            timestamp: chrono::Utc::now(),
                            message: None,
                            details: gift_event_details(&ev),
                        });
                    }
                }
            }
            // Host: another channel hosting this one (Kick's equivalent of a raid).
            "StreamHostEvent" => {
                if let Ok(ev) = serde_json::from_str::<StreamHostEvent>(&envelope.data) {
                    let _ = tx.send(ChatEvent::Event {
                        platform: Platform::Kick,
                        kind: EventKind::Raid,
                        text: host_event_text(&ev),
                        timestamp: chrono::Utc::now(),
                        message: None,
                        details: EventDetails {
                            actor: Some(ev.host_username.clone()),
                            compact: Some(format!("hosted · {} viewers", ev.number_viewers)),
                            ..Default::default()
                        },
                    });
                }
            }
            // Kicks gifted: Kick's bits/cheer equivalent.
            "KicksGifted" => {
                if let Ok(ev) = serde_json::from_str::<KicksGiftedEvent>(&envelope.data) {
                    let _ = tx.send(ChatEvent::Event {
                        platform: Platform::Kick,
                        kind: EventKind::Bits,
                        text: kicks_event_text(&ev),
                        timestamp: chrono::Utc::now(),
                        message: None,
                        details: kicks_event_details(&ev),
                    });
                }
            }
            // A mod pinned a message: the payload's `message` is a full chat
            // message (same shape as a live one), so it goes through the regular
            // builder; `duration` (seconds) gives the expiry from now.
            "PinnedMessageCreatedEvent" => {
                match serde_json::from_str::<PinnedInfo>(&envelope.data) {
                    Ok(pin) => {
                        let event = pin_event(&channel, &info.subscriber_badges, pin, true);
                        let _ = tx.send(event);
                    }
                    Err(err) => {
                        tracing::debug!("unparsed kick pin event ({err}): {}", envelope.data)
                    }
                }
            }
            // The pin was removed (its data is empty) — clear the banner.
            "PinnedMessageDeletedEvent" => {
                let _ = tx.send(ChatEvent::UnpinMessage {
                    platform: Platform::Kick,
                });
            }
            // Channel-point reward redemption.
            "RewardRedeemedEvent" => {
                if let Ok(ev) = serde_json::from_str::<RewardRedeemedEvent>(&envelope.data) {
                    let _ = tx.send(ChatEvent::Event {
                        platform: Platform::Kick,
                        kind: EventKind::Reward,
                        text: reward_event_text(&ev),
                        timestamp: chrono::Utc::now(),
                        message: None,
                        details: EventDetails {
                            actor: Some(ev.username.clone()),
                            compact: Some(format!("redeemed {}", if ev.reward_title.is_empty() {
                                "a reward"
                            } else {
                                &ev.reward_title
                            })),
                            ..Default::default()
                        },
                    });
                }
            }
            // Stream going live: emit a `Live` transition (the broker live poll
            // used to do this). Arrives on the `channel.{channel_id}` Pusher channel,
            // which we subscribe to above. Kick's Pusher event carries no category,
            // so `game` is empty (the tab tooltip just won't show one).
            "StreamerIsLive" => {
                if let Ok(ev) = serde_json::from_str::<StreamerIsLiveEvent>(&envelope.data) {
                    let _ = tx.send(ChatEvent::Live {
                        platform: Platform::Kick,
                        live: true,
                        title: ev.livestream.session_title,
                        game: String::new(),
                        started_at: parse_kick_time(&ev.livestream.created_at),
                        last_stream: None,
                        link: None,
                    });
                }
            }
            // Stream ending → an offline `Live` transition (no title/start when off).
            // The last-stream tooltip info isn't in this event; it's refreshed on the
            // next join (the VOD isn't published the instant the stream ends anyway).
            "StopStreamBroadcast" => {
                let _ = tx.send(ChatEvent::Live {
                    platform: Platform::Kick,
                    live: false,
                    title: String::new(),
                    game: String::new(),
                    started_at: None,
                    last_stream: None,
                    link: None,
                });
            }
            // The sibling frame that carries a resub's typed message. Buffer it by
            // username so the paired `SubscriptionEvent` (which arrives right after)
            // can attach it as a chat line. Only the `subscribe` action with a
            // non-empty message is of interest — see KICK_EVENTS.md.
            "ChatMessageSentEvent" => {
                if let Ok(ev) = serde_json::from_str::<ChatMessageSentEvent>(&envelope.data) {
                    let is_sub = ev.message.action.as_deref() == Some("subscribe");
                    let text = ev.message.optional_message.unwrap_or_default();
                    if is_sub && !text.trim().is_empty() {
                        pending_sub_messages.insert(
                            ev.user.username.to_lowercase(),
                            PendingSubMessage {
                                user_id: ev.user.id,
                                username: ev.user.username,
                                text,
                            },
                        );
                    }
                }
            }
            // Deliberately ignored.
            // `ChannelSubscriptionEvent` is the other redundant sibling of a
            // (re)sub — we render the sub solely from `SubscriptionEvent`.
            // PollUpdate/leaderboard events are the high-volume feeds we
            // intentionally don't display.
            "ChannelSubscriptionEvent"
            | "PollUpdateEvent"
            | "PollDeleteEvent"
            | "GiftsLeaderboardUpdated"
            | "KicksLeaderboardUpdated"
            | "ChatMoveToSupportedChannelEvent"
            | "StreamHostedEvent" => {}
            // Everything else (Pusher housekeeping + not-yet-seen Kick events) is
            // ignored here; the debug log above already records them all. See
            // KICK_EVENTS.md for the captured shapes of the known ignored events.
            _ => {}
        }
    }

    Ok(())
}

/// Builds the [`ChatEvent::PinMessage`] for a pin record — live (`live` = a
/// `PinnedMessageCreatedEvent`, whose `duration` counts from now) or the join
/// seed (pinned some unknown time ago, so only an explicit `finish_at` gives an
/// expiry; otherwise the banner stays until the delete event / unpin).
pub(crate) fn pin_event(
    channel: &str,
    sub_badges: &[SubscriberBadge],
    pin: PinnedInfo,
    live: bool,
) -> ChatEvent {
    let explicit = pin.finish_at.as_deref().and_then(parse_kick_time);
    let ends_at = explicit.or_else(|| {
        (live && pin.duration > 0)
            .then(|| chrono::Utc::now() + chrono::Duration::seconds(pin.duration as i64))
    });
    let message = build_message(channel, sub_badges, pin.message);
    ChatEvent::PinMessage {
        platform: Platform::Kick,
        pinned_by: pin.pinned_by.map(|p| p.username).unwrap_or_default(),
        message: Box::new(message),
        ends_at,
    }
}

/// A human notice for a Kick ban/timeout, naming the moderator + duration when
/// the event provides them (e.g. "mod timed out user for 10m" / "user was
/// permanently banned").
fn ban_notice(ev: &UserBannedEvent) -> String {
    let target = &ev.user.username;
    // The verb (placed before the target) and an optional suffix (after it).
    let (verb, suffix) = if ev.permanent {
        ("permanently banned", String::new())
    } else if ev.duration > 0 {
        ("timed out", format!(" for {}", format_minutes(ev.duration)))
    } else {
        ("timed out", String::new())
    };
    match ev.banned_by.as_ref().map(|m| &m.username) {
        Some(mod_name) if !mod_name.is_empty() => format!("{mod_name} {verb} {target}{suffix}"),
        _ => format!("{target} was {verb}{suffix}"),
    }
}

/// A human notice for a Kick unban/untimeout, naming the moderator when known.
fn unban_notice(ev: &UserUnbannedEvent) -> String {
    let target = &ev.user.username;
    match ev.unbanned_by.as_ref().map(|m| &m.username) {
        Some(mod_name) if !mod_name.is_empty() => format!("{mod_name} unbanned {target}"),
        _ => format!("{target} was unbanned"),
    }
}

/// A notice for a deleted message — only when AI moderation removed it (a manual
/// mod delete is left silent; the struck row alone conveys it). Names the flagged
/// rules when present, e.g. "a message was removed by auto-moderation (hate)".
fn deletion_notice(ev: &MessageDeletedEvent) -> Option<String> {
    if !ev.ai_moderated {
        return None;
    }
    if ev.violated_rules.is_empty() {
        Some("a message was removed by auto-moderation".to_string())
    } else {
        Some(format!(
            "a message was removed by auto-moderation ({})",
            ev.violated_rules.join(", ")
        ))
    }
}

/// Formats a minute count compactly: "90m" → "1h30m", "10m" → "10m".
fn format_minutes(mins: u64) -> String {
    let (h, m) = (mins / 60, mins % 60);
    match (h, m) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h{m}m"),
    }
}

/// "username subscribed for N months." (singular when N == 1).
fn sub_event_text(ev: &SubscriptionEvent) -> String {
    let months = ev.months.max(1);
    let unit = plural(months, "month", "months");
    format!("{} subscribed for {months} {unit}.", ev.username)
}

/// Condensed panel form of [`sub_event_text`]: "subscribed" for a first month,
/// "resubbed · N mo" after.
fn sub_event_details(ev: &SubscriptionEvent) -> EventDetails {
    let months = ev.months.max(1);
    let compact = if months > 1 {
        format!("resubbed · {months} mo")
    } else {
        "subscribed".to_string()
    };
    EventDetails {
        actor: Some(ev.username.clone()),
        compact: Some(compact),
        ..Default::default()
    }
}

/// Condensed panel form of [`gift_event_text`]. Kick sends the whole batch as
/// one event, so the recipients ride the announcement directly (no
/// per-recipient children to group).
fn gift_event_details(ev: &GiftedSubscriptionsEvent) -> EventDetails {
    let actor = Some(
        ev.gifter_username
            .clone()
            .unwrap_or_else(|| "An anonymous user".to_string()),
    );
    match ev.gifted_usernames.as_slice() {
        [single] => EventDetails {
            actor,
            compact: Some(format!("gifted a sub to {single}")),
            ..Default::default()
        },
        many => EventDetails {
            actor,
            compact: Some(format!("gifted {} subs", many.len())),
            gift_count: Some(many.len() as u32),
            recipients: many.to_vec(),
            ..Default::default()
        },
    }
}

/// Condensed panel form of [`kicks_event_text`]: "gifted GiftName · N Kicks".
fn kicks_event_details(ev: &KicksGiftedEvent) -> EventDetails {
    let n = ev.gift.amount;
    let compact = if ev.gift.name.is_empty() {
        format!("gifted {n} {}", plural(n, "Kick", "Kicks"))
    } else {
        format!("gifted {} · {n} {}", ev.gift.name, plural(n, "Kick", "Kicks"))
    };
    EventDetails {
        actor: Some(ev.sender.username.clone()),
        compact: Some(compact),
        ..Default::default()
    }
}

/// Builds the chat-line [`Message`] for a resub's attached message, so it renders
/// under the sub text like a Twitch resub. The buffered text is the raw content,
/// parsed for inline `[emote:id:name]` markers via the shared builder; the author
/// carries no badges/color (the `ChatMessageSentEvent` sub frame doesn't send an
/// identity block), matching how a bare sub line looks.
fn sub_message(
    channel: &str,
    sub_badges: &[SubscriberBadge],
    pending: PendingSubMessage,
) -> Message {
    build_message(
        channel,
        sub_badges,
        KickChatMessage {
            id: format!("kick-sub-{}-{}", pending.user_id, chrono::Utc::now().timestamp_millis()),
            content: pending.text,
            created_at: None,
            sender: crate::builder::Sender {
                id: pending.user_id,
                username: pending.username,
                identity: crate::builder::Identity::default(),
            },
            metadata: None,
        },
    )
}

/// "gifter gifted N subscriptions to a, b, and c. They've gifted T in total."
/// `None` if the gift list is empty (nothing to show).
fn gift_event_text(ev: &GiftedSubscriptionsEvent) -> Option<String> {
    if ev.gifted_usernames.is_empty() {
        return None;
    }
    let gifter = ev.gifter_username.as_deref().unwrap_or("An anonymous user");
    let n = ev.gifted_usernames.len();
    let subs = plural(n as u64, "subscription", "subscriptions");
    let recipients = join_names(&ev.gifted_usernames);
    let mut text = format!("{gifter} gifted {n} {subs} to {recipients}.");
    if ev.gifter_total > 0 {
        let unit = plural(ev.gifter_total, "sub", "subs");
        text.push_str(&format!(
            " They've gifted {} {unit} in total.",
            ev.gifter_total
        ));
    }
    text.into()
}

/// "X hosted the stream with N viewers." (singular when N == 1).
fn host_event_text(ev: &StreamHostEvent) -> String {
    let n = ev.number_viewers;
    let unit = plural(n, "viewer", "viewers");
    format!("{} hosted the stream with {n} {unit}.", ev.host_username)
}

/// "X gifted GiftName (N Kicks)." (singular when N == 1).
fn kicks_event_text(ev: &KicksGiftedEvent) -> String {
    let n = ev.gift.amount;
    let unit = plural(n, "Kick", "Kicks");
    let gift = if ev.gift.name.is_empty() {
        "Kicks"
    } else {
        &ev.gift.name
    };
    format!("{} gifted {gift} ({n} {unit}).", ev.sender.username)
}

/// "X redeemed RewardTitle."
fn reward_event_text(ev: &RewardRedeemedEvent) -> String {
    let title = if ev.reward_title.is_empty() {
        "a reward"
    } else {
        &ev.reward_title
    };
    format!("{} redeemed {title}.", ev.username)
}

/// Joins names as "a", "a and b", or "a, b, and c" (Oxford comma).
fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [a] => a.clone(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(name: &str) -> KickUserRef {
        KickUserRef {
            username: name.into(),
        }
    }

    fn deleted(ai: bool, rules: &[&str]) -> MessageDeletedEvent {
        MessageDeletedEvent {
            message: MessageRef { id: "m1".into() },
            ai_moderated: ai,
            violated_rules: rules.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn deletion_notice_only_for_ai_with_rules() {
        assert_eq!(deletion_notice(&deleted(false, &[])), None);
        assert_eq!(
            deletion_notice(&deleted(true, &[])),
            Some("a message was removed by auto-moderation".to_string())
        );
        assert_eq!(
            deletion_notice(&deleted(true, &["hate", "spam"])),
            Some("a message was removed by auto-moderation (hate, spam)".to_string())
        );
    }

    #[test]
    fn ban_notice_with_moderator_and_duration() {
        let ev = UserBannedEvent {
            user: user("baduser"),
            banned_by: Some(user("modname")),
            permanent: false,
            duration: 90,
        };
        assert_eq!(ban_notice(&ev), "modname timed out baduser for 1h30m");
    }

    #[test]
    fn permanent_ban_with_moderator() {
        let ev = UserBannedEvent {
            user: user("baduser"),
            banned_by: Some(user("modname")),
            permanent: true,
            duration: 0,
        };
        assert_eq!(ban_notice(&ev), "modname permanently banned baduser");
    }

    #[test]
    fn ban_notice_without_moderator() {
        let ev = UserBannedEvent {
            user: user("baduser"),
            banned_by: None,
            permanent: false,
            duration: 10,
        };
        assert_eq!(ban_notice(&ev), "baduser was timed out for 10m");
    }

    #[test]
    fn unban_notice_with_and_without_moderator() {
        let with = UserUnbannedEvent {
            user: user("baduser"),
            unbanned_by: Some(user("modname")),
        };
        assert_eq!(unban_notice(&with), "modname unbanned baduser");
        let without = UserUnbannedEvent {
            user: user("baduser"),
            unbanned_by: None,
        };
        assert_eq!(unban_notice(&without), "baduser was unbanned");
    }

    #[test]
    fn subscription_text_singular_and_plural() {
        let one = SubscriptionEvent {
            username: "alice".into(),
            months: 1,
        };
        assert_eq!(sub_event_text(&one), "alice subscribed for 1 month.");
        let many = SubscriptionEvent {
            username: "alice".into(),
            months: 6,
        };
        assert_eq!(sub_event_text(&many), "alice subscribed for 6 months.");
    }

    #[test]
    fn chat_message_sent_carries_resub_message() {
        // The sibling frame Kick sends before a resub's SubscriptionEvent.
        let data = r#"{
            "message": {"action":"subscribe","optional_message":"[emote:42:KEKW] love it","months_subscribed":6},
            "user": {"id":123,"username":"Alice"}
        }"#;
        let ev: ChatMessageSentEvent = serde_json::from_str(data).unwrap();
        assert_eq!(ev.message.action.as_deref(), Some("subscribe"));
        assert_eq!(
            ev.message.optional_message.as_deref(),
            Some("[emote:42:KEKW] love it")
        );

        // The message we'd attach parses inline emotes like normal chat.
        let msg = sub_message(
            "chan",
            &[],
            PendingSubMessage {
                user_id: ev.user.id,
                username: ev.user.username,
                text: ev.message.optional_message.unwrap(),
            },
        );
        assert_eq!(msg.author.display_name, "Alice");
        assert_eq!(msg.raw_text, "[emote:42:KEKW] love it");
        assert!(msg
            .elements
            .iter()
            .any(|e| matches!(e, bks_core::MessageElement::Emote(_))));
    }

    #[test]
    fn chat_message_sent_ignores_non_sub_and_empty() {
        // A subscribe action with no message → nothing to buffer.
        let empty = r#"{"message":{"action":"subscribe","optional_message":null},"user":{"id":1,"username":"Bob"}}"#;
        let ev: ChatMessageSentEvent = serde_json::from_str(empty).unwrap();
        assert!(ev.message.optional_message.unwrap_or_default().trim().is_empty());
    }

    #[test]
    fn gift_text_lists_recipients_and_total() {
        let ev = GiftedSubscriptionsEvent {
            gifter_username: Some("gifter".into()),
            gifted_usernames: vec!["a".into(), "b".into(), "c".into()],
            gifter_total: 10,
        };
        assert_eq!(
            gift_event_text(&ev).unwrap(),
            "gifter gifted 3 subscriptions to a, b, and c. They've gifted 10 subs in total."
        );
    }

    #[test]
    fn gift_text_single_recipient_and_anonymous() {
        let ev = GiftedSubscriptionsEvent {
            gifter_username: None,
            gifted_usernames: vec!["a".into()],
            gifter_total: 0,
        };
        assert_eq!(
            gift_event_text(&ev).unwrap(),
            "An anonymous user gifted 1 subscription to a."
        );
    }

    #[test]
    fn gift_text_empty_is_none() {
        let ev = GiftedSubscriptionsEvent {
            gifter_username: Some("g".into()),
            gifted_usernames: vec![],
            gifter_total: 5,
        };
        assert_eq!(gift_event_text(&ev), None);
    }

    #[test]
    fn host_text_singular_and_plural() {
        let one = StreamHostEvent {
            host_username: "raider".into(),
            number_viewers: 1,
        };
        assert_eq!(
            host_event_text(&one),
            "raider hosted the stream with 1 viewer."
        );
        let many = StreamHostEvent {
            host_username: "raider".into(),
            number_viewers: 50,
        };
        assert_eq!(
            host_event_text(&many),
            "raider hosted the stream with 50 viewers."
        );
    }

    #[test]
    fn kicks_text_names_gift_and_amount() {
        let ev = KicksGiftedEvent {
            sender: user("alice"),
            gift: KicksGift {
                name: "Rocket".into(),
                amount: 100,
            },
        };
        assert_eq!(kicks_event_text(&ev), "alice gifted Rocket (100 Kicks).");
    }

    #[test]
    fn reward_text_names_reward() {
        let ev = RewardRedeemedEvent {
            reward_title: "Hydrate".into(),
            username: "bob".into(),
        };
        assert_eq!(reward_event_text(&ev), "bob redeemed Hydrate.");
    }

    #[test]
    fn pinned_message_created_event_becomes_pin() {
        // Trimmed from a live capture of `App\Events\PinnedMessageCreatedEvent`
        // (the Pusher `data` string, already unwrapped).
        let data = r##"{
            "message": {
                "id": "a5941a21-b590-4c91-9e7d-b0d7472bf17b",
                "chatroom_id": 83425049,
                "content": "dsadsadas",
                "type": "message",
                "created_at": "2026-07-03T13:01:58+00:00",
                "sender": {
                    "id": 84907698,
                    "username": "alice",
                    "slug": "alice",
                    "identity": {"color": "#FFD899", "badges": [{"type":"broadcaster","text":"Broadcaster","sort_order":3}]}
                },
                "metadata": {"message_ref": "1783083718897"},
                "thread_parent_id": ""
            },
            "duration": "1200",
            "pinnedBy": {"id": 84907698, "username": "alice", "slug": "alice"}
        }"##;
        let pin: PinnedInfo = serde_json::from_str(data).unwrap();
        assert_eq!(pin.duration, 1200);
        let before = chrono::Utc::now();
        let ChatEvent::PinMessage {
            platform,
            pinned_by,
            message,
            ends_at,
        } = pin_event("alice", &[], pin, true)
        else {
            panic!("expected PinMessage");
        };
        assert_eq!(platform, Platform::Kick);
        assert_eq!(pinned_by, "alice");
        assert_eq!(message.id, "a5941a21-b590-4c91-9e7d-b0d7472bf17b");
        assert_eq!(message.author.display_name, "alice");
        assert_eq!(message.raw_text, "dsadsadas");
        // A live pin expires `duration` seconds from now.
        let ends_at = ends_at.expect("live pin has an expiry");
        assert!(ends_at >= before + chrono::Duration::seconds(1199));
        assert!(ends_at <= chrono::Utc::now() + chrono::Duration::seconds(1201));
    }

    #[test]
    fn seeded_pin_without_finish_time_has_no_expiry() {
        let data = r#"{"message": {"id": "m1", "content": "hi", "sender": {"id": 5, "username": "Bob"}}, "duration": 1200}"#;
        let pin: PinnedInfo = serde_json::from_str(data).unwrap();
        // Seed (`live: false`): duration alone gives no expiry (its start is unknown).
        let ChatEvent::PinMessage { ends_at, .. } = pin_event("chan", &[], pin, false) else {
            panic!("expected PinMessage");
        };
        assert_eq!(ends_at, None);
        // But an explicit finish time is honored.
        let data = r#"{"message": {"id": "m1", "content": "hi", "sender": {"id": 5, "username": "Bob"}}, "finish_at": "2026-07-03T14:00:00+00:00"}"#;
        let pin: PinnedInfo = serde_json::from_str(data).unwrap();
        let ChatEvent::PinMessage { ends_at, .. } = pin_event("chan", &[], pin, false) else {
            panic!("expected PinMessage");
        };
        assert_eq!(
            ends_at,
            bks_core::parse_rfc3339("2026-07-03T14:00:00+00:00")
        );
    }
}
