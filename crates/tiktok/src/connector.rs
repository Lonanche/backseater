use std::{
    collections::{HashSet, VecDeque},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use bks_core::Platform;
use bks_platform::{ChannelMeta, ChatEvent, ChatSink, ChatSource, ChatStream, EventKind};
use piratetok_live_rs::{
    errors::TikTokLiveError,
    http::api::{fetch_room_info, FetchParams},
    structs::{events::RoomInfo, TikTokLiveEvent},
    TikTokLive,
};

use crate::builder;

const OFFLINE_RETRY: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const SEEN_LIMIT: usize = 8192;

#[derive(Default)]
pub struct TikTokSource;

#[async_trait]
impl ChatSource for TikTokSource {
    async fn join(&self, channel: &str) -> anyhow::Result<ChatStream> {
        let channel = bks_core::normalize_tiktok_channel(channel).ok_or_else(|| {
            anyhow::anyhow!("Enter a TikTok username or a tiktok.com/@username/live URL")
        })?;
        let (tx, rx) = bks_platform::chat_channel();
        tokio::spawn(async move {
            until_closed(&tx, run(&channel, &tx)).await;
        });
        Ok(rx)
    }
}

async fn until_closed(tx: &ChatSink, reader: impl std::future::Future<Output = ()>) {
    tokio::select! {
        _ = tx.closed() => {},
        _ = reader => {},
    }
}

async fn next_event(
    connected: bool,
    receive: impl std::future::Future<Output = Option<TikTokLiveEvent>>,
) -> anyhow::Result<Option<TikTokLiveEvent>> {
    if connected {
        // The socket's stale timer sees heartbeats too; a quiet chat is healthy.
        Ok(receive.await)
    } else {
        tokio::time::timeout(CONNECT_TIMEOUT, receive)
            .await
            .map_err(|_| anyhow::anyhow!("TikTok connection setup timed out"))
    }
}

#[derive(Default)]
struct Seen {
    keys: HashSet<String>,
    order: VecDeque<String>,
}

impl Seen {
    fn insert(&mut self, key: String) -> bool {
        if !self.keys.insert(key.clone()) {
            return false;
        }
        self.order.push_back(key);
        if self.order.len() > SEEN_LIMIT {
            if let Some(old) = self.order.pop_front() {
                self.keys.remove(&old);
            }
        }
        true
    }

    fn event(&mut self, kind: &str, id: i64) -> bool {
        id > 0 && self.insert(format!("{kind}:{id}"))
    }
}

#[derive(Default)]
struct State {
    seen: Seen,
    live: Option<bool>,
    room_id: String,
    title: String,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    viewers: Option<u64>,
}

impl State {
    fn live_event(&self, channel: &str, live: bool) -> ChatEvent {
        ChatEvent::Live {
            platform: Platform::TikTok,
            historical: false,
            live,
            title: if live {
                self.title.clone()
            } else {
                String::new()
            },
            game: String::new(),
            started_at: if live { self.started_at } else { None },
            last_stream: None,
            link: live.then(|| Platform::TikTok.channel_url(channel)),
        }
    }

    async fn set_live(&mut self, channel: &str, live: bool, tx: &ChatSink) {
        if self.live != Some(live) {
            self.live = Some(live);
            let _ = tx.send(self.live_event(channel, live)).await;
        }
        if !live {
            self.clear_viewers(tx).await;
        }
    }

    async fn clear_viewers(&mut self, tx: &ChatSink) {
        if self.viewers.take().is_some() {
            let _ = tx
                .send(ChatEvent::Viewers {
                    platform: Platform::TikTok,
                    count: None,
                })
                .await;
        }
    }

    fn ingest(&mut self, channel: &str, event: TikTokLiveEvent) -> Vec<ChatEvent> {
        let event = match event {
            TikTokLiveEvent::Chat(msg) => builder::chat(channel, &msg, false)
                .map(Box::new)
                .map(ChatEvent::Message),
            TikTokLiveEvent::ChatHistory(msg) => builder::chat(channel, &msg, true)
                .map(Box::new)
                .map(ChatEvent::Message),
            TikTokLiveEvent::EmoteChat(msg) => builder::emote_chat(channel, &msg, false)
                .map(Box::new)
                .map(ChatEvent::Message),
            TikTokLiveEvent::EmoteChatHistory(msg) => builder::emote_chat(channel, &msg, true)
                .map(Box::new)
                .map(ChatEvent::Message),
            TikTokLiveEvent::RoomUserSeq(msg) => {
                let count = u64::try_from(msg.viewer_count).ok();
                if count == self.viewers {
                    return Vec::new();
                }
                self.viewers = count;
                Some(ChatEvent::Viewers {
                    platform: Platform::TikTok,
                    count,
                })
            }
            TikTokLiveEvent::Gift(msg) => {
                let Some(event) = builder::gift(&msg) else {
                    return Vec::new();
                };
                let key = if msg.is_combo_gift() && msg.group_id > 0 {
                    format!("gift-group:{}", msg.group_id)
                } else {
                    let Some(common) = msg.common.as_ref().filter(|c| c.msg_id > 0) else {
                        return Vec::new();
                    };
                    format!("gift:{}", common.msg_id)
                };
                self.seen.insert(key).then_some(event)
            }
            TikTokLiveEvent::SubNotify(msg) => {
                let id = msg.common.as_ref().map_or(0, |c| c.msg_id);
                self.seen.event("subscription", id).then(|| {
                    builder::event(
                        EventKind::Sub,
                        msg.sender.as_ref(),
                        msg.common.as_ref(),
                        if msg.sub_month > 1 {
                            format!("subscription update ({} months)", msg.sub_month)
                        } else {
                            "subscription update".into()
                        },
                    )
                })
            }
            TikTokLiveEvent::Follow(msg) => {
                let id = msg.common.as_ref().map_or(0, |c| c.msg_id);
                self.seen.event("follow", id).then(|| {
                    builder::event(
                        EventKind::Other,
                        msg.user.as_ref(),
                        msg.common.as_ref(),
                        "followed".into(),
                    )
                })
            }
            TikTokLiveEvent::ImDelete(msg) => {
                return msg
                    .delete_msg_ids_list
                    .into_iter()
                    .filter(|id| *id > 0)
                    .map(|id| ChatEvent::DeleteMessage {
                        platform: Platform::TikTok,
                        message_id: id.to_string(),
                    })
                    .collect();
            }
            _ => None,
        };
        match event {
            Some(ChatEvent::Message(msg)) if self.seen.insert(format!("chat:{}", msg.id)) => {
                vec![ChatEvent::Message(msg)]
            }
            Some(ChatEvent::Message(_)) | None => Vec::new(),
            Some(event) => vec![event],
        }
    }
}

async fn run(channel: &str, tx: &ChatSink) {
    let mut state = State::default();
    let mut attempt = 0u32;
    let mut last_error = String::new();
    loop {
        let started = Instant::now();
        let result = session(channel, tx, &mut state).await;
        let result = match result {
            Err(err)
                if matches!(
                    err.downcast_ref::<TikTokLiveError>(),
                    Some(TikTokLiveError::HostNotOnline(_))
                ) =>
            {
                Ok(())
            }
            other => other,
        };
        if tx.is_closed() {
            return;
        }
        if started.elapsed() >= Duration::from_secs(30) {
            attempt = 0;
        }
        let delay = match result {
            Ok(()) => {
                state.set_live(channel, false, tx).await;
                last_error.clear();
                attempt = 0;
                OFFLINE_RETRY
            }
            Err(err) => {
                state.clear_viewers(tx).await;
                let error = err.to_string();
                tracing::warn!("TikTok @{channel}: {error}");
                if error != last_error {
                    let _ = tx
                        .send(ChatEvent::Error(format!(
                            "TikTok @{channel}: {error}. Retrying automatically."
                        )))
                        .await;
                    last_error = error;
                }
                let delay = bks_core::reconnect_delay(attempt);
                attempt = attempt.saturating_add(1);
                delay
            }
        };
        tokio::time::sleep(delay).await;
    }
}

async fn session(channel: &str, tx: &ChatSink, state: &mut State) -> anyhow::Result<()> {
    let mut stream = tokio::time::timeout(
        Duration::from_secs(20),
        TikTokLive::builder(channel)
            .timeout(Duration::from_secs(12))
            .max_retries(0)
            .connect(),
    )
    .await
    .map_err(|_| TikTokLiveError::invalid("room lookup timed out"))??;
    // Dropping this session aborts both the socket and the optional metadata request.
    let mut metadata = tokio::task::JoinSet::new();
    let mut connected = false;
    loop {
        tokio::select! {
            info = metadata.join_next(), if !metadata.is_empty() => {
                if let Some(Ok(Ok(info))) = info {
                    apply_metadata(channel, state, info, tx).await;
                }
            }
            event = next_event(connected, stream.next_event()) => {
                let Some(event) = event? else { return Err(TikTokLiveError::ConnectionClosed.into()); };
                match event {
                    TikTokLiveEvent::Connected { room_id } => {
                        connected = true;
                        if state.room_id != room_id {
                            state.title.clear();
                            state.started_at = None;
                            state.room_id = room_id.clone();
                        }
                        let _ = tx.send(ChatEvent::Channel(ChannelMeta { platform: Platform::TikTok, id: room_id.clone(), name: channel.into() })).await;
                        state.set_live(channel, true, tx).await;
                        let _ = tx.send(ChatEvent::System(format!("Connected to TikTok @{channel} (read-only)"))).await;
                        metadata.spawn(async move { fetch_room_info(&room_id, FetchParams { timeout: Duration::from_secs(8), ..Default::default() }).await });
                    }
                    TikTokLiveEvent::ConnectionError { message } => anyhow::bail!(message),
                    TikTokLiveEvent::Disconnected => return Err(TikTokLiveError::ConnectionClosed.into()),
                    TikTokLiveEvent::LiveEnded(_) => return Ok(()),
                    other => for event in state.ingest(channel, other) {
                        if tx.send(event).await.is_err() { return Ok(()); }
                    }
                }
            }
        }
    }
}

async fn apply_metadata(channel: &str, state: &mut State, info: RoomInfo, tx: &ChatSink) {
    state.title = info.title;
    state.started_at = serde_json::from_str::<serde_json::Value>(&info.raw_json)
        .ok()
        .and_then(|json| json.pointer("/data/create_time").and_then(|v| v.as_i64()))
        .filter(|time| *time > 0)
        .and_then(|time| chrono::DateTime::from_timestamp(time, 0));
    let _ = tx.send(state.live_event(channel, true)).await;
    if state.viewers.is_none() {
        state.viewers = u64::try_from(info.viewers).ok();
        let _ = tx
            .send(ChatEvent::Viewers {
                platform: Platform::TikTok,
                count: state.viewers,
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use piratetok_live_rs::structs::proto::{
        messages::{WebcastChatMessage, WebcastImDeleteMessage, WebcastRoomUserSeqMessage},
        types::CommonMessageData,
        user::UserIdentity,
    };

    #[tokio::test(start_paused = true)]
    async fn quiet_connected_stream_does_not_use_the_setup_timeout() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let task = tokio::spawn(async move { next_event(true, rx.recv()).await });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(120)).await;
        assert!(!task.is_finished());
        tx.send(TikTokLiveEvent::Disconnected).await.unwrap();
        assert!(matches!(
            task.await.unwrap().unwrap(),
            Some(TikTokLiveEvent::Disconnected)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_connection_setup_times_out() {
        let result = next_event(false, std::future::pending()).await;
        assert!(result.unwrap_err().to_string().contains("setup timed out"));
    }

    #[test]
    fn subscription_enum_field_does_not_drop_the_notice() {
        // Field 11 is a scalar gift-source enum in newer wire messages, not a user.
        let wire = [
            0x0a, 2, 0x10, 42, 0x12, 5, 0x1a, 3, b'B', b'o', b'b', 0x20, 3, 0x58, 1,
        ];
        let decoded =
            piratetok_live_rs::decode::mapper::decode_message("WebcastSubNotifyMessage", &wire);
        let mut state = State::default();
        let event = decoded.into_iter().next().unwrap();
        assert!(matches!(state.ingest("creator", event.clone()).as_slice(),
            [ChatEvent::Event { kind: EventKind::Sub, text, .. }] if text == "Bob subscription update (3 months)"));
        assert!(state.ingest("creator", event).is_empty());
    }

    #[tokio::test]
    async fn dropping_receiver_cancels_pending_reader() {
        let (tx, rx) = bks_platform::chat_channel();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let reader = async move {
                let _drop_guard = dropped_tx;
                let _ = started_tx.send(());
                std::future::pending::<()>().await;
            };
            until_closed(&tx, reader).await;
        });
        started_rx.await.unwrap();
        drop(rx);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(dropped_rx.await.is_err());
    }

    #[test]
    fn deduplicates_replayed_messages_and_bounds_memory() {
        let mut state = State::default();
        let msg = WebcastChatMessage {
            common: Some(CommonMessageData {
                msg_id: 42,
                ..Default::default()
            }),
            user: Some(UserIdentity::default()),
            comment: "hello".into(),
            ..Default::default()
        };
        assert_eq!(
            state
                .ingest("creator", TikTokLiveEvent::Chat(msg.clone()))
                .len(),
            1
        );
        assert!(state
            .ingest("creator", TikTokLiveEvent::ChatHistory(msg))
            .is_empty());
        for id in 0..SEEN_LIMIT + 50 {
            state.seen.insert(id.to_string());
        }
        assert_eq!(state.seen.keys.len(), SEEN_LIMIT);
        assert_eq!(state.seen.order.len(), SEEN_LIMIT);
    }

    #[test]
    fn routes_viewer_updates_and_message_deletions() {
        let mut state = State::default();
        let count = WebcastRoomUserSeqMessage {
            viewer_count: 15,
            ..Default::default()
        };
        assert!(matches!(
            state
                .ingest("creator", TikTokLiveEvent::RoomUserSeq(count.clone()))
                .as_slice(),
            [ChatEvent::Viewers {
                count: Some(15),
                ..
            }]
        ));
        assert!(state
            .ingest("creator", TikTokLiveEvent::RoomUserSeq(count))
            .is_empty());
        let deletes = WebcastImDeleteMessage {
            delete_msg_ids_list: vec![42, 43],
            ..Default::default()
        };
        assert_eq!(
            state
                .ingest("creator", TikTokLiveEvent::ImDelete(deletes))
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn read_only_source_rejects_sends_and_invalid_sources() {
        assert!(TikTokSource.send("creator", "hello", None).await.is_err());
        assert!(TikTokSource.join("name&other=1").await.is_err());
    }

    #[tokio::test]
    async fn offline_transitions_emit_once_and_clear_viewers() {
        let (tx, mut rx) = bks_platform::chat_channel();
        let mut state = State {
            live: Some(true),
            viewers: Some(12),
            ..Default::default()
        };
        state.set_live("creator", false, &tx).await;
        state.set_live("creator", false, &tx).await;
        assert!(matches!(
            rx.recv().await,
            Some(ChatEvent::Live { live: false, .. })
        ));
        assert!(matches!(
            rx.recv().await,
            Some(ChatEvent::Viewers { count: None, .. })
        ));
        assert!(rx.try_recv().is_err());
    }
}
