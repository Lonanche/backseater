//! The expandability seam. Every platform implements [`ChatSource`]; the UI
//! depends only on this trait and on [`ChatEvent`], never on a concrete platform.
//! Moderation actions are concrete per-platform types (`TwitchActions`,
//! `KickActions`) — their REST shapes differ too much to share one trait.

use async_trait::async_trait;
use bks_core::{Badge, Emote, Message, NamePaint, Platform};
use tokio::sync::mpsc;

/// The kind of a public channel event, so the UI can filter events (e.g. an
/// events panel that only shows subs + raids). Connectors classify each event
/// they emit into one of these; the pre-formatted `text` is still what's shown.
/// `Other` covers events that don't fit a checklist category (rituals,
/// announcements, ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum EventKind {
    /// Subscription / resubscription (someone subscribing for themselves).
    Sub,
    /// Gifted subscription(s) (one or many, named or anonymous).
    Gift,
    /// A raid (Twitch) or host (Kick) — another channel sending its viewers over.
    Raid,
    /// Bits cheer (Twitch) or Kicks gifted (Kick).
    Bits,
    /// A channel-points reward redemption.
    Reward,
    /// A Twitch watch streak (`viewermilestone` USERNOTICE) — a viewer watched
    /// several consecutive streams.
    WatchStreak,
    /// Anything else (rituals, announcements, bits-badge tiers, ...).
    Other,
}

impl EventKind {
    /// Every kind, for building a default "all enabled" checklist.
    pub const ALL: [EventKind; 7] = [
        EventKind::Sub,
        EventKind::Gift,
        EventKind::Raid,
        EventKind::Bits,
        EventKind::Reward,
        EventKind::WatchStreak,
        EventKind::Other,
    ];

    /// A short human label for the events-panel checklist.
    pub fn label(self) -> &'static str {
        match self {
            EventKind::Sub => "Subscriptions",
            EventKind::Gift => "Gifts",
            EventKind::Raid => "Raids / Hosts",
            EventKind::Bits => "Bits / Kicks",
            EventKind::Reward => "Channel points",
            EventKind::WatchStreak => "Watch streaks",
            EventKind::Other => "Other",
        }
    }
}

/// Identity of a joined channel, resolved once the connection is live. The
/// `id` is the platform's stable numeric/opaque channel id (Twitch room-id,
/// Kick chatroom id, ...) used to fetch per-channel resources like 3rd-party
/// emotes; `name` is the human-facing channel name used as a display/lookup key.
///
/// This replaces leaking a raw, platform-specific id through [`ChatEvent`]:
/// connectors fill it in however their platform provides it, and consumers
/// (e.g. emote loading) stay platform-agnostic.
#[derive(Clone, Debug)]
pub struct ChannelMeta {
    pub platform: Platform,
    pub id: String,
    pub name: String,
}

/// Something that happened in a channel. The connector pushes these onto a
/// [`ChatStream`]; the UI drains them.
#[derive(Clone, Debug)]
pub enum ChatEvent {
    /// A chat message. Boxed because a `Message` is much larger than the other
    /// variants (author, badges, token stream, optional reply parent), so storing
    /// it inline would bloat every `ChatEvent` to that size.
    Message(Box<Message>),
    /// A connector-generated status notice (connected, joined, ...). Informational
    /// only — the UI logs these rather than showing them in chat. Use [`ChatEvent::Error`]
    /// for problems the user should see and be able to copy.
    System(String),
    /// A user-facing error (login/send/moderation failure, a bad command, ...).
    /// Shown in the chat log as a distinct, selectable, copyable row — the kind of
    /// message a user would want to paste when reporting a problem.
    Error(String),
    /// A user-visible informational notice — moderation outcomes the user should
    /// see in chat ("mod timed out user for 10m", "mod deleted a message by user",
    /// ...). Shown as a muted system row, unlike [`ChatEvent::System`] (status
    /// chatter, log-only) and [`ChatEvent::Error`] (problems, highlighted).
    Notice(String),
    /// A public channel event (sub, resub, gift sub, raid, ...), with a
    /// ready-made human-readable string and a [`EventKind`] so the UI can filter
    /// them (e.g. an events panel that shows only chosen kinds). Shown as a
    /// highlighted row, not chat. These are visible without auth, so connectors
    /// emit them anonymously.
    ///
    /// On a sub/resub the chatter can attach a chat message; when present it's
    /// carried in `message` as a full [`Message`] (author, badges, timestamp,
    /// token stream) so the UI renders it under the system text like a normal
    /// chat line, instead of flattening it into `text`. `None` for events with
    /// no attached message (gifts, raids, ...). `timestamp` is when the event
    /// happened (the events panel shows it).
    Event {
        platform: Platform,
        kind: EventKind,
        text: String,
        timestamp: chrono::DateTime<chrono::Utc>,
        message: Option<Box<Message>>,
    },
    /// Chat was cleared, optionally scoped to a single user (timeout/ban). The
    /// `platform` scopes the fade to that platform's messages (the same login can
    /// exist on Twitch and Kick). `historical` marks a clear replayed from the
    /// join backlog: it still fades the target's backfilled messages, but the UI
    /// posts no notice for it — the timeout happened before this session, and a
    /// fresh "X was timed out" row on every launch misreads as a live action.
    ClearChat {
        platform: Platform,
        user: Option<String>,
        historical: bool,
    },
    /// A single message was deleted (by a mod or auto-moderation). The matching
    /// row is struck through + faded, like a ban fade but scoped to one message.
    /// `platform` disambiguates ids that could collide across platforms.
    DeleteMessage {
        platform: Platform,
        message_id: String,
    },
    /// A moderator pinned a chat message (or an existing pin was updated /
    /// re-observed on join). Both Twitch and Kick allow one active mod pin per
    /// channel, so this *replaces* the platform's current pin. `message` is the
    /// pinned chat message in full (author, tokens, timestamp) so the UI renders
    /// it like a chat line inside the pinned banner; `pinned_by` is the display
    /// name of the pinning moderator (empty if unknown); `ends_at` is when the
    /// pin expires (`None` = until unpinned / stream end).
    PinMessage {
        platform: Platform,
        pinned_by: String,
        message: Box<Message>,
        ends_at: Option<chrono::DateTime<chrono::Utc>>,
    },
    /// The platform's active pinned message was removed (unpinned by a mod or
    /// expired server-side). Clears the pinned banner for `platform`.
    UnpinMessage { platform: Platform },
    /// Whether a rich moderator event feed (Twitch EventSub `channel.moderate`)
    /// is live for `platform`. While active, moderation notices arrive as
    /// [`ChatEvent::Notice`] with moderator + duration + reason, so the UI
    /// suppresses its own generic "X was timed out / banned" fallback (the fade
    /// itself still comes from [`ChatEvent::ClearChat`]).
    ModFeed { platform: Platform, active: bool },
    /// AutoMod (or a blocked term) held a chatter's message for review — only
    /// delivered when the logged-in user moderates the channel. Shown as a
    /// highlighted row with the held text and Allow/Deny actions; `message_id`
    /// is what the approve/deny API call takes and what a later
    /// [`ChatEvent::AutoModResolved`] refers back to. `reason` is a ready-made
    /// human string ("automod: swearing, level 4" / "blocked term").
    AutoModHeld {
        platform: Platform,
        message_id: String,
        /// The chatter whose message was held (display name).
        user: String,
        text: String,
        reason: String,
        timestamp: chrono::DateTime<chrono::Utc>,
    },
    /// A held AutoMod message was resolved (a moderator allowed/denied it, or it
    /// expired untouched). The UI updates the matching held row in place.
    AutoModResolved {
        platform: Platform,
        message_id: String,
        status: AutoModStatus,
        /// Who resolved it (display name); empty when it expired.
        moderator: String,
    },
    /// Channel identity, emitted once the connection is live (before or with the
    /// first message). Lets consumers fetch per-channel resources keyed on the
    /// platform id without that id leaking into the UI.
    Channel(ChannelMeta),
    /// Whether the logged-in user is a moderator (or broadcaster) in this
    /// channel, so the UI can offer moderation controls. `is_broadcaster` is set
    /// only when the user is the channel owner — role grants (mod/VIP) need a
    /// broadcaster token, so the UI gates those on it separately from `is_mod`.
    /// Emitted when the connector learns the user's own role (Twitch: from
    /// USERSTATE on join).
    ModStatus {
        platform: Platform,
        is_mod: bool,
        is_broadcaster: bool,
    },
    /// The emotes usable in this channel (3rd-party + native) on `platform`,
    /// emitted once the bridge has loaded them. The UI keeps them per-platform for
    /// the emote picker's Twitch/Kick tabs; they're not shown in chat. Re-emitted
    /// on a reconnect with the fresh set.
    Emotes {
        platform: Platform,
        emotes: Vec<Emote>,
    },
    /// A chatter's resolved 7TV cosmetics (name paint and/or a 7TV badge),
    /// emitted by the bridge after an async lookup keyed on their numeric id. The
    /// UI applies them to that user's existing and future messages on `platform`
    /// (like a retroactive ban fade, but additive). Skipped entirely when the 7TV
    /// cosmetics setting is off.
    Cosmetics {
        platform: Platform,
        user_id: String,
        paint: Option<NamePaint>,
        badge: Option<Badge>,
    },
    /// A stream went live or offline. Twitch emits this on a transition from the
    /// bridge's live-status poll; Kick emits it from its Pusher
    /// `StreamerIsLive`/`StopStreamBroadcast` events (and an initial seed on join).
    /// Rendered as a highlighted system row (green on live, muted on offline).
    /// `title` is the stream title when going live (empty when offline or unknown).
    /// `game` is the stream's category/game (empty when offline or unknown).
    /// `started_at` is when the stream began (for an uptime readout), `None` when
    /// offline or unavailable. `last_stream` describes the most recent *past*
    /// broadcast, set only on an offline event when known (Kick's VODs, Twitch's
    /// IVR `lastBroadcast`), for the tooltip's "last live …" line; `None` otherwise.
    /// `link` is the stream's own watch URL when that differs from the channel
    /// page (YouTube's per-stream `watch?v=` link); `None` where the channel page
    /// *is* the stream (Twitch/Kick) or when offline.
    Live {
        platform: Platform,
        live: bool,
        title: String,
        game: String,
        started_at: Option<chrono::DateTime<chrono::Utc>>,
        last_stream: Option<LastStream>,
        link: Option<String>,
    },
    /// A periodic concurrent-viewer-count update for the platform's stream,
    /// separate from [`ChatEvent::Live`] so a count refresh can't clobber the
    /// live-status metadata (title/game/last stream). `None` = unknown or
    /// offline. Emitted only when the count changes; the UI's status bar shows
    /// it next to each live platform.
    Viewers {
        platform: Platform,
        count: Option<u64>,
    },
}

/// How a held AutoMod message was resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoModStatus {
    Approved,
    Denied,
    /// Nobody acted within Twitch's review window.
    Expired,
}

/// A channel's most recent past broadcast, for the offline tab tooltip. Generic
/// (platform-agnostic) so it can ride [`ChatEvent::Live`]; a connector fills it
/// from its own source (Kick from the VODs endpoint, Twitch from IVR's
/// `lastBroadcast`).
#[derive(Clone, Debug, PartialEq)]
pub struct LastStream {
    /// When that stream began.
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// When it ended (start + duration). `None` when the source doesn't report
    /// a duration (Twitch/IVR) — the tooltip then shows only how long ago it
    /// started, not how long it ran.
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The stream's title (empty when unknown).
    pub title: String,
    /// The stream's category/game (empty when unknown).
    pub game: String,
}

/// A stream of [`ChatEvent`]s for one joined channel.
pub type ChatStream = mpsc::UnboundedReceiver<ChatEvent>;

/// The sending half a connector keeps to publish events.
pub type ChatSink = mpsc::UnboundedSender<ChatEvent>;

/// Reading and sending chat. Every platform connector implements this; the UI
/// depends only on the trait, never a concrete platform.
#[async_trait]
pub trait ChatSource: Send + Sync {
    /// Connect to a channel and return its event stream.
    async fn join(&self, channel: &str) -> anyhow::Result<ChatStream>;

    /// Send a chat message, optionally as a reply to `reply_parent_id` (the parent
    /// message's platform id) so it threads in chat. The default errors; connectors
    /// that support sending (e.g. authenticated Twitch) override it.
    async fn send(
        &self,
        _channel: &str,
        _text: &str,
        _reply_parent_id: Option<&str>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("sending is not supported on this connection")
    }
}
