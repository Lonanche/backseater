//! Bounded fanout and join replay for one processed platform/channel stream.
//!
//! Publishing never waits for a UI consumer. A lagging feed gets an explicit
//! warning and a fresh snapshot, while other feeds continue receiving events.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use bks_platform::ChatEvent;
use tokio::sync::broadcast;

const HISTORY_CAPACITY: usize = 1000;
const STATE_CAPACITY: usize = 4096;
const BROADCAST_CAPACITY: usize = 2048;

#[derive(Clone)]
pub(crate) struct SourceEvent {
    pub sequence: u64,
    pub event: Arc<ChatEvent>,
}

pub(crate) struct Subscription {
    pub seed: Vec<SourceEvent>,
    pub through: u64,
    pub events: broadcast::Receiver<SourceEvent>,
}

#[derive(Hash, PartialEq, Eq)]
enum StateKey {
    Channel,
    Emotes,
    Live,
    Viewers,
    Modes,
    ModStatus,
    ModFeed,
    Pin,
    Cosmetics(String),
    Deleted(String),
    Cleared(Option<String>),
    Suspicious(String),
    AutoModResolved(String),
}

impl StateKey {
    fn persistent(&self) -> bool {
        matches!(
            self,
            Self::Channel
                | Self::Emotes
                | Self::Live
                | Self::Viewers
                | Self::Modes
                | Self::ModStatus
                | Self::ModFeed
                | Self::Pin
        )
    }
}

struct Inner {
    sequence: u64,
    history: VecDeque<SourceEvent>,
    state: HashMap<StateKey, SourceEvent>,
    events: broadcast::Sender<SourceEvent>,
}

pub(crate) struct SourceHub(Mutex<Inner>);

impl SourceHub {
    pub fn new() -> Self {
        Self::with_capacity(BROADCAST_CAPACITY)
    }

    fn with_capacity(capacity: usize) -> Self {
        let (events, _) = broadcast::channel(capacity);
        Self(Mutex::new(Inner {
            sequence: 0,
            history: VecDeque::new(),
            state: HashMap::new(),
            events,
        }))
    }

    pub fn publish(&self, mut event: ChatEvent) {
        // Kick's untimestamped clears mean "now". Capture that instant once so
        // replay cannot apply an old timeout to later messages from that user.
        if let ChatEvent::ClearChat { timestamp, .. } = &mut event {
            timestamp.get_or_insert_with(chrono::Utc::now);
        }
        let key = match &event {
            ChatEvent::Channel(_) => Some(StateKey::Channel),
            ChatEvent::Emotes { .. } => Some(StateKey::Emotes),
            ChatEvent::Live { .. } => Some(StateKey::Live),
            ChatEvent::Viewers { .. } => Some(StateKey::Viewers),
            ChatEvent::ChatModes { .. } => Some(StateKey::Modes),
            ChatEvent::ModStatus { .. } => Some(StateKey::ModStatus),
            ChatEvent::ModFeed { .. } => Some(StateKey::ModFeed),
            ChatEvent::PinMessage { .. } | ChatEvent::UnpinMessage { .. } => Some(StateKey::Pin),
            ChatEvent::Cosmetics { user_id, .. } => Some(StateKey::Cosmetics(user_id.clone())),
            ChatEvent::DeleteMessage { message_id, .. } => {
                Some(StateKey::Deleted(message_id.clone()))
            }
            ChatEvent::ClearChat { user, .. } => Some(StateKey::Cleared(user.clone())),
            ChatEvent::Suspicious { message, .. } => Some(StateKey::Suspicious(message.id.clone())),
            ChatEvent::AutoModResolved { message_id, .. } => {
                Some(StateKey::AutoModResolved(message_id.clone()))
            }
            _ => None,
        };
        let mut inner = self.0.lock().unwrap();
        inner.sequence += 1;
        let event = SourceEvent {
            sequence: inner.sequence,
            event: Arc::new(event),
        };
        if let Some(key) = key {
            inner.state.insert(key, event.clone());
            if inner.state.len() > STATE_CAPACITY {
                // Most events never reach this path: prune in batches so a busy
                // channel does not scan its whole cosmetics/deletion cache per line.
                let mut stale: Vec<_> = inner
                    .state
                    .iter()
                    .filter(|(key, _)| !key.persistent())
                    .map(|(_, value)| value.sequence)
                    .collect();
                stale.sort_unstable();
                let through = stale[STATE_CAPACITY / 4 - 1];
                inner
                    .state
                    .retain(|key, value| key.persistent() || value.sequence > through);
            }
        } else if !matches!(&*event.event, ChatEvent::System(_) | ChatEvent::Notice(_)) {
            // Routine notices have no historical flag and would create false
            // unread activity on join. Source errors remain visible in replay:
            // a failed connection must still explain itself to a later feed.
            inner.history.push_back(event.clone());
            while inner.history.len() > HISTORY_CAPACITY {
                inner.history.pop_front();
            }
        }
        let _ = inner.events.send(event);
    }

    /// The snapshot and live subscription are captured under the publish lock:
    /// an event is either in the seed or the receiver, never lost between them.
    /// On lag, only unseen rows and state changes replay.
    pub fn subscribe(&self, after: u64) -> Subscription {
        let inner = self.0.lock().unwrap();
        let mut seed: Vec<_> = inner
            .state
            .values()
            .filter(|event| event.sequence > after)
            .cloned()
            .chain(
                inner
                    .history
                    .iter()
                    .filter(|event| event.sequence > after)
                    .cloned(),
            )
            .collect();
        seed.sort_unstable_by_key(|event| event.sequence);
        Subscription {
            seed,
            through: inner.sequence,
            events: inner.events.subscribe(),
        }
    }
}

/// Seeded rows predate this feed's subscription. Keep notifications and generic
/// moderation notices silent, matching a connector's normal join backlog.
/// Actionable source errors are retained unchanged and may mark a tab unread.
pub(crate) fn historical(event: &ChatEvent) -> ChatEvent {
    let mut event = event.clone();
    match &mut event {
        ChatEvent::Message(message) | ChatEvent::Suspicious { message, .. } => {
            message.historical = true
        }
        ChatEvent::Event {
            message, details, ..
        } => {
            details.historical = true;
            if let Some(message) = message {
                message.historical = true;
            }
        }
        ChatEvent::ClearChat { historical, .. }
        | ChatEvent::Live { historical, .. }
        | ChatEvent::AutoModHeld { historical, .. } => *historical = true,
        _ => {}
    }
    event
}

#[cfg(test)]
mod tests {
    use super::*;
    use bks_core::{Author, Message, Platform};

    fn message(text: &str) -> ChatEvent {
        ChatEvent::Message(Box::new(Message {
            id: text.into(),
            platform: Platform::Twitch,
            channel: "fixture".into(),
            timestamp: chrono::Utc::now(),
            author: Author::default(),
            raw_text: text.into(),
            elements: vec![],
            reply: None,
            first_message: false,
            highlighted: false,
            historical: false,
            reward_id: None,
        }))
    }

    #[test]
    fn a_late_join_receives_current_state_and_backlog_then_live_events() {
        let hub = SourceHub::new();
        hub.publish(ChatEvent::Viewers {
            platform: Platform::Kick,
            count: Some(1),
        });
        hub.publish(message("backlog"));
        hub.publish(ChatEvent::Viewers {
            platform: Platform::Kick,
            count: Some(2),
        });
        let mut joined = hub.subscribe(0);
        assert_eq!(joined.seed.len(), 2);
        assert!(
            matches!(&*joined.seed[0].event, ChatEvent::Message(message) if message.raw_text == "backlog")
        );
        assert!(matches!(
            &*joined.seed[1].event,
            ChatEvent::Viewers { count: Some(2), .. }
        ));
        hub.publish(message("live"));
        assert_eq!(
            joined.events.try_recv().unwrap().sequence,
            joined.through + 1
        );
    }

    #[test]
    fn slow_subscribers_do_not_block_fast_ones_and_recover_unseen_moderation() {
        let hub = SourceHub::with_capacity(2);
        let mut slow = hub.subscribe(0);
        let mut fast = hub.subscribe(0);
        hub.publish(message("already seen"));
        let seen = slow.events.try_recv().unwrap().sequence;
        fast.events.try_recv().unwrap();
        for index in 0..10 {
            hub.publish(ChatEvent::DeleteMessage {
                platform: Platform::Twitch,
                message_id: index.to_string(),
            });
            assert!(fast.events.try_recv().is_ok());
        }
        assert!(matches!(
            slow.events.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
        let recovered = hub.subscribe(seen);
        assert_eq!(recovered.seed.len(), 10);
        assert!(recovered
            .seed
            .iter()
            .all(|event| matches!(&*event.event, ChatEvent::DeleteMessage { .. })));
    }

    #[test]
    fn replay_is_bounded_but_preserves_infrequent_channel_state() {
        let hub = SourceHub::new();
        hub.publish(ChatEvent::Emotes {
            platform: Platform::Kick,
            emotes: vec![],
        });
        for index in 0..STATE_CAPACITY + 20 {
            hub.publish(message(&index.to_string()));
            hub.publish(ChatEvent::DeleteMessage {
                platform: Platform::Kick,
                message_id: index.to_string(),
            });
        }
        let replay = hub.subscribe(0);
        assert!(replay.seed.len() <= HISTORY_CAPACITY + STATE_CAPACITY);
        assert!(replay
            .seed
            .iter()
            .any(|event| matches!(&*event.event, ChatEvent::Emotes { .. })));
    }

    #[test]
    fn replayed_clear_keeps_its_original_time_and_suppresses_notices() {
        let hub = SourceHub::new();
        hub.publish(ChatEvent::ClearChat {
            platform: Platform::Kick,
            user: Some("user".into()),
            historical: false,
            timestamp: None,
        });
        let replay = hub.subscribe(0);
        let original = &replay.seed[0].event;
        let ChatEvent::ClearChat {
            timestamp: Some(at),
            ..
        } = &**original
        else {
            panic!("clear must be timestamped")
        };
        assert!(
            matches!(historical(original), ChatEvent::ClearChat { timestamp: Some(replayed_at), historical: true, .. } if replayed_at == *at)
        );
    }

    #[test]
    fn recovery_replays_latest_state_without_repeating_already_delivered_rows() {
        let hub = SourceHub::new();
        hub.publish(message("already delivered"));
        hub.publish(ChatEvent::ModStatus {
            platform: Platform::Twitch,
            is_mod: true,
            is_broadcaster: true,
        });
        let delivered = hub.subscribe(0).through;
        hub.publish(ChatEvent::ModStatus {
            platform: Platform::Twitch,
            is_mod: false,
            is_broadcaster: false,
        });
        hub.publish(ChatEvent::UnpinMessage {
            platform: Platform::Twitch,
        });
        hub.publish(message("unseen"));
        let recovery = hub.subscribe(delivered);
        assert_eq!(recovery.seed.len(), 3);
        assert!(matches!(
            &*recovery.seed[0].event,
            ChatEvent::ModStatus {
                is_mod: false,
                is_broadcaster: false,
                ..
            }
        ));
        assert!(matches!(
            &*recovery.seed[1].event,
            ChatEvent::UnpinMessage { .. }
        ));
        assert!(
            matches!(&*recovery.seed[2].event, ChatEvent::Message(message) if message.raw_text == "unseen")
        );
        assert!(hub.subscribe(recovery.through).seed.is_empty());
    }

    #[test]
    fn notices_are_live_only_but_source_failures_remain_visible_on_join() {
        let hub = SourceHub::new();
        let mut live = hub.subscribe(0);
        hub.publish(ChatEvent::Notice("routine moderation notice".into()));
        hub.publish(ChatEvent::Error("connection failed".into()));
        assert!(matches!(
            &*live.events.try_recv().unwrap().event,
            ChatEvent::Notice(_)
        ));
        assert!(matches!(
            &*live.events.try_recv().unwrap().event,
            ChatEvent::Error(_)
        ));
        let joined = hub.subscribe(0);
        assert_eq!(joined.seed.len(), 1);
        assert!(
            matches!(&*joined.seed[0].event, ChatEvent::Error(text) if text == "connection failed")
        );
    }
}
