//! The shared **channel model**: the canonical message
//! buffer + per-channel resolved state for one channel set, shared by every view
//! (tab or popped-out window) looking at it.
//!
//! There is one model per joined channel; each view
//! observes it and lays out its own scrollback. A [`ChannelModel`]
//! (a gpui `Entity`) owns the `rows` buffer, the connection ([`Controller`]), and
//! all per-channel state (ban/delete strikes, 7TV cosmetics, pins, live status,
//! picker emotes, mod state). Each `ChatView` holds the same `Entity<ChannelModel>`
//! and reconciles its **own** virtualized `ListState` from the model's granular
//! [`ChannelEvent`]s — so two views on one channel share the buffer and connection
//! but keep independent scroll, selection, ignore/mention filters.
//!
//! The [`ChannelStore`] registry (a gpui global) dedupes by [`ChannelKey`]: the
//! first view of a channel connects; the model is handed out to later views of the
//! same key. It holds only `WeakEntity`s, so when the last `ChatView` on a channel
//! drops, the model drops — ending the drain task and closing the connection — and
//! the dead registry slot self-cleans.
//!
//! **Filtering is per-view, not here**: the model stores
//! *every* message; each view drops its own ignore-list matches and computes its
//! own mention tint at render, so two tabs with different ignore/highlight settings
//! show different subsets of the one shared buffer.

use std::collections::{HashMap, HashSet, VecDeque};

use bks_core::{Message, Platform};
use chrono::{DateTime, Utc};
use bks_platform::ChatEvent;
use gpui::prelude::*;
use gpui::{App, Context, Entity, EventEmitter, Global, Task, WeakEntity};

use bks_platform::EventKind;

use crate::chatview::{history_insert_index, row_key, ActivePin, LiveInfo, Row};
use crate::controller::Controller;
use crate::session::Session;
use crate::{MAX_EVENTS, MAX_ROWS};

/// How long a channel-point reward message (Twitch IRC) waits for its pubsub
/// "X redeemed …" event (or vice-versa) before giving up and rendering alone.
/// The two arrive over separate sockets a beat apart; a couple seconds covers
/// the skew without holding an unpaired message visibly long.
const REWARD_PAIR_WINDOW: std::time::Duration = std::time::Duration::from_secs(3);

/// A public channel event (sub/gift/raid/…) held in the model's retained
/// [`events`](ChannelModel::events) buffer, which — unlike the chat ring buffer —
/// isn't trimmed when chat fills up, so the events panel keeps its history through
/// a busy chat. Mirrors the fields of [`Row::Event`].
#[derive(Clone)]
pub struct RetainedEvent {
    pub platform: Platform,
    pub kind: EventKind,
    pub text: String,
    pub timestamp: DateTime<Utc>,
    pub message: Option<Box<Message>>,
    /// Structured extras for the panel's compact rows (see [`EventDetails`]).
    pub details: bks_platform::EventDetails,
    /// For a mass gift's per-recipient event: the sequence number of the batch
    /// announcement it belongs to (see [`gift_group`]). Views collapsing gift
    /// batches hide these rows and list their recipients under the summary.
    pub group: Option<u64>,
}

/// Identifies a channel set: the (normalized) Twitch / Kick / YouTube sources a
/// tab is configured with. Two tabs with the same triple share one model. The
/// parts are lowercased/trimmed so case differences don't split the key.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ChannelKey {
    pub twitch: String,
    pub kick: String,
    pub youtube: String,
}

impl ChannelKey {
    pub fn new(twitch: &str, kick: &str, youtube: &str) -> Self {
        Self {
            twitch: bks_core::channel_login(twitch),
            kick: bks_core::channel_login(kick),
            youtube: youtube.trim().to_lowercase(),
        }
    }

    /// A key with no channels at all — an unconfigured tab, never shared.
    fn is_empty(&self) -> bool {
        self.twitch.is_empty() && self.kick.is_empty() && self.youtube.is_empty()
    }
}

/// A granular change to the model's `rows`, emitted so each observing view can
/// apply the *same* structural edit to its own `ListState` (which must stay in
/// lockstep with `rows` for the virtualized log).
#[derive(Clone)]
pub enum ChannelEvent {
    /// A row was appended at the end (index = its position at emit time; valid
    /// for `ListState` splicing since views apply events in emit order). For a
    /// message row the message rides along — subscribers run only after the
    /// whole update burst, when a ring trim may already have shifted (or
    /// dropped) the row, so they must not look it up by index.
    Appended {
        index: usize,
        msg: Option<std::sync::Arc<Message>>,
        /// The row replays the join backlog (a historical message *or* event) —
        /// it must never flash the tab. Carried on the event because a
        /// historical event without an attached message has `msg: None`,
        /// indistinguishable from a live notice row.
        historical: bool,
        /// The model's [`rows_generation`](ChannelModel::rows_generation) right
        /// after this mutation. ⚠️ Structural events carry it because gpui
        /// queues `cx.emit` and delivers the queue to subscribers that
        /// register *between emit and flush* — a view that seeds its
        /// `ListState` from the current rows and then subscribes receives
        /// replays of the very events its seed already covered, inflating the
        /// list with phantom trailing items (2× items over rows at startup —
        /// the invisible-day-divider bug). Views record the generation they
        /// seeded at and skip anything at or below it.
        generation: u64,
    },
    /// A row was inserted at `index` (history backfill placement — insertion
    /// only ever happens for historical rows, so unlike
    /// [`Appended`](Self::Appended) it carries no `historical` flag). Same
    /// stale-index caveat as `Appended`, same replay guard via `generation`.
    Inserted {
        index: usize,
        msg: Option<std::sync::Arc<Message>>,
        /// See [`Appended`](Self::Appended)`::generation`.
        generation: u64,
    },
    /// The front row was dropped (ring-buffer trim past `MAX_ROWS`).
    RemovedFront {
        /// See [`Appended`](Self::Appended)`::generation`.
        generation: u64,
    },
    /// A retained event was appended to the [`events`](ChannelModel::events)
    /// buffer, identified by its stable sequence number (see
    /// [`events_base`](ChannelModel::events_base)) — carried because subscribers
    /// run after the whole update burst, when `events.back()` may already be a
    /// later event. Views append it to their events panel if it passes their
    /// kind filter.
    EventAppended { seq: u64 },
    /// The events buffer trimmed old entries past `MAX_EVENTS` (`events_base`
    /// advanced). Views drop shown sequence numbers below the new base.
    EventsTrimmed,
    /// A non-structural change (a row's content updated in place, e.g. an AutoMod
    /// row resolved, or side-table state changed) — views just repaint.
    Changed,
    /// A viewer count changed. Split from [`Changed`](Self::Changed) because it
    /// fires steadily (every ~30s per live platform) and only the status bar /
    /// tooltip read it — views answer with a bare repaint, NOT the log re-measure
    /// `Changed` triggers.
    ViewersChanged,
    /// A platform's chat-restriction modes changed (follower-only, emote-only,
    /// slow, ...). Like [`ViewersChanged`](Self::ViewersChanged): only the mode
    /// bar above the composer reads it, so views answer with a bare repaint,
    /// never the log re-measure `Changed` triggers.
    ChatModesChanged,
    /// A platform's stream just transitioned to live (the `live` flag flipped
    /// false→true). Fired alongside the `Row::Live` push so the tab strip can
    /// flash the chip; a mere in-place `Live` update (a late start time, an
    /// offline seed) does not fire it.
    WentLive { platform: bks_core::Platform },
}

/// One channel's shared model: the message buffer + connection + per-channel state.
pub struct ChannelModel {
    /// The canonical row buffer, shared by every view. Holds *all* messages
    /// (unfiltered); views apply their own ignore/mention at render.
    pub rows: VecDeque<Row>,
    /// Retained public events (sub/gift/raid/…) for the events panel, kept
    /// separately from `rows` so a busy chat trimming the ring buffer can't drop
    /// them. Each `ChatEvent::Event` is pushed here *and* into `rows` (for the
    /// inline log line); this buffer is trimmed only at [`MAX_EVENTS`]. The events
    /// panel reads from here, not from the filtered `rows`.
    pub events: VecDeque<RetainedEvent>,
    /// How many events have been trimmed off the front of `events` over the
    /// model's life. An event's stable **sequence number** is `events_base +
    /// its current index`, so views can reference events across trims (their
    /// virtualized events panels store sequence numbers, not indices).
    pub events_base: u64,
    /// Mass-gift batches still expecting per-recipient events, keyed by
    /// `(platform, gifter login)` → (the announcement's seq, gifts remaining).
    /// See [`gift_group`].
    pending_gifts: HashMap<(Platform, String), (u64, u32)>,
    /// Keys of rows present, so a reconnect's refetched history can't duplicate a
    /// row already shown (deduped once here, not per view).
    row_keys: HashSet<u64>,
    /// Bumped on every structural row mutation (push/insert/pop). Lets a view
    /// cheaply detect whether `rows` changed since it last derived something from
    /// it — the thread-reply UI caches its reconstructed chain keyed on this so it
    /// isn't rebuilt per keystroke. Not tied to struck/side-table edits (those
    /// don't change row membership).
    rows_generation: u64,
    /// Single messages struck by a deletion, keyed platform → set of msg ids.
    /// Nested (not a `(Platform, String)` tuple key) so the per-row/per-frame
    /// [`is_struck`](Self::is_struck) lookup probes the inner set with a borrowed
    /// `&str` — no key `String` allocation on the hot render path.
    struck_ids: HashMap<Platform, HashSet<String>>,
    /// Ban/timeout fades keyed platform → (lowercased login → cutoff time): only
    /// that author's messages *at or before* the cutoff are struck. The cutoff is
    /// the clear's server-side timestamp (falling back to "now"), so once the
    /// timeout lapses — or a ban replayed from the join backlog was lifted before
    /// we joined — the user's newer messages render normally (the fade doesn't
    /// leak onto future chat). Nested for the same borrowed-`&str` lookup as
    /// `struck_ids`.
    struck_authors: HashMap<Platform, HashMap<String, DateTime<Utc>>>,
    /// Resolved 7TV cosmetics keyed platform → (user_id → cosmetics), applied at
    /// render time. Nested so the per-row lookup borrows `user_id` as `&str`
    /// (cosmetics resolve for most active chatters, so this ran per visible row
    /// per frame with a `to_string` on the old tuple key).
    cosmetics: HashMap<Platform, HashMap<String, bks_emotes::Cosmetics>>,
    /// Active pinned message per platform (shown as a banner).
    pub pins: HashMap<Platform, ActivePin>,
    /// Latest live status per platform (for the tab strip's hover tooltip).
    pub live_status: HashMap<Platform, LiveInfo>,
    /// Latest concurrent viewer count per platform (for the status bar), fed by
    /// the periodic `ChatEvent::Viewers` updates; absent = unknown or offline.
    pub viewer_counts: HashMap<Platform, u64>,
    /// Active chat-restriction modes per platform (for the mode bar above the
    /// composer); a platform with nothing restricted has no entry.
    pub chat_modes: HashMap<Platform, bks_platform::ChatModes>,
    /// Picker emote sets per platform (channel 7TV + native).
    pub emotes_twitch: Vec<bks_core::Emote>,
    pub emotes_kick: Vec<bks_core::Emote>,
    pub emotes_youtube: Vec<bks_core::Emote>,
    /// Whether the logged-in user moderates the Twitch channel (gates usercard
    /// mod actions).
    pub twitch_mod: bool,
    /// Whether the logged-in user owns the Twitch channel. Only the broadcaster
    /// can grant/revoke mod + VIP, so those buttons gate on this, not `twitch_mod`.
    pub twitch_broadcaster: bool,
    /// Whether the logged-in user moderates the Kick channel. Resolved by the
    /// controller (own-usercard lookup at connect/login, refreshed from our own
    /// messages' badges) and delivered as `ModStatus`, like Twitch's USERSTATE.
    pub kick_mod: bool,
    /// Platforms whose rich moderator feed (Twitch EventSub) is live — suppresses
    /// the generic ban/timeout notice fallback.
    pub rich_mod_feed: HashSet<Platform>,
    /// Reward messages (Twitch IRC PRIVMSGs carrying a `custom-reward-id`) held
    /// back until their pubsub "X redeemed …" event arrives, so the message
    /// renders *under* that header as one row. Keyed by lowercased redeemer login
    /// → a FIFO queue, so one user redeeming twice in the window keeps both
    /// messages (a plain map would clobber the first). A held message with no
    /// event within the window is flushed as a normal chat row.
    pending_reward_msgs: HashMap<String, VecDeque<Box<Message>>>,
    /// Redeemers whose "X redeemed …" event was just rendered without its
    /// message (the rare case where pubsub beat IRC). Their reward message,
    /// arriving within the window, renders as a normal row *below* the already-
    /// shown event (correct order) instead of being held. Keyed by lowercased
    /// redeemer name → the instant the event rendered.
    recent_reward_events: HashMap<String, std::time::Instant>,
    /// The connection handle (send/moderation), shared by every view.
    pub controller: Controller,
    /// Drains the connection's event stream into this model. Dropping the model
    /// (last view closed) drops this task, closing the connection.
    _drain: Task<()>,
}

impl EventEmitter<ChannelEvent> for ChannelModel {}

impl ChannelModel {
    /// The current row count (each view sizes its `ListState` to this).
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// The retained event with stable sequence number `seq`, or `None` if it
    /// has been trimmed.
    pub fn event_at(&self, seq: u64) -> Option<&RetainedEvent> {
        let ix = seq.checked_sub(self.events_base)?;
        self.events.get(ix as usize)
    }

    /// Whether the logged-in user can moderate (pin/ban/timeout) on `platform`.
    /// Both flags hold real mod status: Twitch from IRC USERSTATE, Kick from the
    /// controller's own-usercard lookup (+ our own messages' badges) — see the
    /// field docs. The one place this predicate lives, so adding a platform
    /// touches one arm.
    pub fn can_moderate(&self, platform: Platform) -> bool {
        match platform {
            Platform::Twitch => self.twitch_mod,
            Platform::Kick => self.kick_mod,
            _ => false,
        }
    }

    /// The platforms of this channel the logged-in user can moderate, in a
    /// fixed order. The mod-button strip reserves one slot per button
    /// applicable to *any* of them (ghost slots where a button doesn't apply
    /// to a row's own platform), so rows of different platforms in a merged
    /// feed keep their message text horizontally aligned — see
    /// `render::ModStrip`.
    pub fn mod_platforms(&self) -> Vec<Platform> {
        [Platform::Twitch, Platform::Kick]
            .into_iter()
            .filter(|p| match p {
                Platform::Twitch => self.controller.has_twitch(),
                Platform::Kick => self.controller.has_kick(),
                _ => false,
            })
            .filter(|p| self.can_moderate(*p))
            .collect()
    }

    /// Whether the logged-in user *owns* `platform`'s channel — a stricter tier
    /// than [`can_moderate`](Self::can_moderate), gating broadcaster-only
    /// actions (`/raid`, `/mod`, `/vip`). Twitch = the USERSTATE broadcaster
    /// badge; Kick = the login matching the channel slug.
    pub fn is_broadcaster(&self, platform: Platform) -> bool {
        match platform {
            Platform::Twitch => self.twitch_broadcaster,
            Platform::Kick => self.controller.kick_is_broadcaster(),
            _ => false,
        }
    }

    pub fn is_struck(&self, msg: &Message) -> bool {
        // Runs per visible row per frame; the nested maps let us probe with a
        // borrowed `&str`, so a struck channel costs no key allocation here.
        if self.struck_ids.is_empty() && self.struck_authors.is_empty() {
            return false;
        }
        if self
            .struck_ids
            .get(&msg.platform)
            .is_some_and(|ids| ids.contains(msg.id.as_str()))
        {
            return true;
        }
        // Keys are stored lowercased. Producer logins already arrive lowercase
        // (Twitch IRC logins by protocol, Kick's builder lowercases), so the
        // common probe borrows the login as-is — this runs for every visible
        // row per frame while anyone on the platform is struck, so the
        // `to_lowercase` allocation is kept off that path and only fires for a
        // login that actually carries uppercase.
        let Some(authors) = self.struck_authors.get(&msg.platform) else {
            return false;
        };
        let login = &msg.author.login;
        let cutoff = if login.chars().any(char::is_uppercase) {
            authors.get(&login.to_lowercase())
        } else {
            authors.get(login.as_str())
        };
        cutoff.is_some_and(|&cutoff| msg.timestamp <= cutoff)
    }

    /// The cosmetics resolved for a chatter, if any (applied at render time).
    pub fn cosmetics_for(&self, platform: Platform, user_id: &str) -> Option<&bks_emotes::Cosmetics> {
        // Per visible row per frame; the nested map borrows `user_id` as `&str`
        // so this allocates nothing even once cosmetics are resolved.
        if self.cosmetics.is_empty() || user_id.is_empty() {
            return None;
        }
        self.cosmetics.get(&platform)?.get(user_id)
    }

    /// Forgets all resolved cosmetics (7TV-cosmetics toggle turned off).
    pub fn clear_cosmetics(&mut self, cx: &mut Context<Self>) {
        self.cosmetics.clear();
        cx.emit(ChannelEvent::Changed);
        cx.notify();
    }

    // --- Buffer mutations (each emits a ChannelEvent so views reconcile) ---

    fn row_push_back(&mut self, row: Row, cx: &mut Context<Self>) {
        if !self.note_row_key(&row) {
            return;
        }
        let ix = self.rows.len();
        let msg = row_message(&row);
        let historical = row_historical(&row);
        self.rows.push_back(row);
        self.rows_generation += 1;
        cx.emit(ChannelEvent::Appended {
            index: ix,
            msg,
            historical,
            generation: self.rows_generation,
        });
    }

    fn row_insert(&mut self, ix: usize, row: Row, cx: &mut Context<Self>) {
        if !self.note_row_key(&row) {
            return;
        }
        let msg = row_message(&row);
        self.rows.insert(ix, row);
        self.rows_generation += 1;
        cx.emit(ChannelEvent::Inserted {
            index: ix,
            msg,
            generation: self.rows_generation,
        });
    }

    fn row_pop_front(&mut self, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.pop_front() {
            if let Some(key) = row_key(&row) {
                self.row_keys.remove(&key);
            }
            self.rows_generation += 1;
            cx.emit(ChannelEvent::RemovedFront {
                generation: self.rows_generation,
            });
        }
    }

    /// A counter bumped on every structural `rows` mutation; a view can cache a
    /// value derived from `rows` and rebuild it only when this changes.
    pub fn rows_generation(&self) -> u64 {
        self.rows_generation
    }

    /// The sequence number the *next* retained event will get (`events_base` +
    /// current length) — the seed value for a view's `EventAppended` replay
    /// watermark (see [`ChannelEvent::Appended`]'s `generation` doc).
    pub fn next_event_seq(&self) -> u64 {
        self.events_base + self.events.len() as u64
    }

    /// Where a historical row with timestamp `ts` belongs in `rows` — like
    /// [`history_insert_index`], with an O(1) fast path for the dominant case:
    /// the join backlog arrives oldest-first, so each row lands *after* the
    /// current last row. When that last row is historical and not newer, no
    /// earlier anchor can exist either (historical rows always insert ahead of
    /// live messages/events, so a historical tail row means no live anchors
    /// anywhere), skipping the front scan that made an 800-line backlog O(n²).
    fn historical_insert_ix(&self, ts: DateTime<Utc>) -> Option<usize> {
        match self.rows.back() {
            Some(Row::Message { msg }) if msg.historical && msg.timestamp <= ts => None,
            Some(Row::Event {
                historical: true,
                timestamp,
                ..
            }) if *timestamp <= ts => None,
            _ => history_insert_index(self.rows.iter(), ts),
        }
    }

    /// Records `row`'s key; `false` means an identical row is already present.
    fn note_row_key(&mut self, row: &Row) -> bool {
        match row_key(row) {
            Some(key) => self.row_keys.insert(key),
            None => true,
        }
    }

    /// Inserts a message row, keeping backfilled history timestamp-sorted (so
    /// Twitch + Kick history interleave chronologically and stay ahead of live).
    /// Routes a channel-point reward message (Twitch `custom-reward-id`). If its
    /// "X redeemed …" event just rendered (pubsub beat IRC), show it now as a
    /// normal row — it lands directly below that event. Otherwise hold it, keyed
    /// by the redeemer, until the event arrives and attaches it; a matching event
    /// within [`REWARD_PAIR_WINDOW`] merges the two, and a flush after that window
    /// renders any still-unpaired message as a plain row (a reward that emits no
    /// community-points event).
    fn handle_reward_message(&mut self, msg: Box<Message>, cx: &mut Context<Self>) {
        let key = msg.author.display_name.to_lowercase();
        let recent = self
            .recent_reward_events
            .get(&key)
            .is_some_and(|at| at.elapsed() < REWARD_PAIR_WINDOW);
        if recent {
            self.recent_reward_events.remove(&key);
            self.insert_message(msg.into(), cx);
            return;
        }
        self.pending_reward_msgs
            .entry(key.clone())
            .or_default()
            .push_back(msg);
        cx.spawn(async move |model, cx| {
            cx.background_executor().timer(REWARD_PAIR_WINDOW).await;
            let _ = model.update(cx, |model, cx| {
                // Flush the oldest still-held message for this key (this timer's).
                // A matching event may have already taken it, leaving nothing.
                if let Some(msg) = model.take_pending_reward_msg(&key, None) {
                    model.insert_message(msg.into(), cx);
                }
            });
        })
        .detach();
    }

    /// Takes a reward message held for `key`, preferring the one whose text
    /// matches `want_text` (the event's `user_input`) so a user redeeming more
    /// than once at a time pairs each event with the right message; falls back to
    /// the oldest (FIFO) when there's no text match — Twitch may normalize the
    /// input, and identical redemptions are indistinguishable anyway. Removes the
    /// queue entry once empty so the map doesn't retain empty deques.
    fn take_pending_reward_msg(
        &mut self,
        key: &str,
        want_text: Option<&str>,
    ) -> Option<Box<Message>> {
        let queue = self.pending_reward_msgs.get_mut(key)?;
        let msg = want_text
            .and_then(|want| queue.iter().position(|m| m.raw_text == want))
            .and_then(|i| queue.remove(i))
            .or_else(|| queue.pop_front());
        if queue.is_empty() {
            self.pending_reward_msgs.remove(key);
        }
        msg
    }

    fn insert_message(&mut self, msg: std::sync::Arc<Message>, cx: &mut Context<Self>) {
        if !msg.historical {
            self.row_push_back(Row::Message { msg }, cx);
            return;
        }
        match self.historical_insert_ix(msg.timestamp) {
            Some(i) => self.row_insert(i, Row::Message { msg }, cx),
            None => self.row_push_back(Row::Message { msg }, cx),
        }
    }

    /// A live (non-historical) public event: pairs a channel-point reward with
    /// its IRC message, retains the event for the events panel, and appends the
    /// inline log row. (The historical branch in [`push`](Self::push) bypasses
    /// all of this — a backlog replay only gets a sorted log row.)
    #[allow(clippy::too_many_arguments)]
    fn push_live_event(
        &mut self,
        platform: Platform,
        kind: EventKind,
        text: String,
        timestamp: DateTime<Utc>,
        mut message: Option<Box<Message>>,
        details: bks_platform::EventDetails,
        cx: &mut Context<Self>,
    ) {
        // A channel-point reward's message arrives separately over IRC. If
        // it's already waiting, attach it so it renders under this header;
        // otherwise remember that the event showed first, so the message
        // (arriving shortly) renders as a normal row below it.
        if kind == EventKind::Reward {
            if let Some(name) = details.actor.as_deref() {
                let key = name.to_lowercase();
                let want = details.redeem_input.as_deref();
                if let Some(reward_msg) = self.take_pending_reward_msg(&key, want) {
                    message = Some(reward_msg);
                } else {
                    // Drop entries whose message never arrived so the map
                    // can't grow unbounded (a text reward should always
                    // emit a message, but be defensive).
                    self.recent_reward_events
                        .retain(|_, at| at.elapsed() < REWARD_PAIR_WINDOW);
                    self.recent_reward_events
                        .insert(key, std::time::Instant::now());
                }
            }
        }
        // Retain the event in its own buffer (survives chat-ring trimming)
        // for the events panel, then also push the inline log row.
        let group = gift_group(
            &mut self.pending_gifts,
            &details,
            platform,
            self.events_base + self.events.len() as u64,
        );
        let accent = details.accent;
        let actor = details.actor.clone();
        self.events.push_back(RetainedEvent {
            platform,
            kind,
            text: text.clone(),
            timestamp,
            message: message.clone(),
            details,
            group,
        });
        cx.emit(ChannelEvent::EventAppended {
            seq: self.events_base + self.events.len() as u64 - 1,
        });
        let mut trimmed = false;
        while self.events.len() > MAX_EVENTS {
            self.events.pop_front();
            self.events_base += 1;
            trimmed = true;
        }
        if trimmed {
            cx.emit(ChannelEvent::EventsTrimmed);
        }
        self.row_push_back(
            Row::Event {
                platform,
                kind,
                text,
                timestamp,
                message,
                accent,
                actor,
                historical: false,
            },
            cx,
        );
    }

    /// Fades the target's chat *up to the ban moment* (a ban/timeout strikes their
    /// past messages). The cutoff — the clear's server-side timestamp, falling
    /// back to `now` when the platform doesn't send one — is what stops the fade
    /// leaking onto future chat: once the timeout lapses (or a replayed-from-
    /// history ban was lifted before we joined) the user's newer messages carry a
    /// later timestamp than the cutoff, so [`is_struck`] renders them normally.
    fn mark_banned(
        &mut self,
        platform: Platform,
        login: &str,
        timestamp: Option<DateTime<Utc>>,
        cx: &mut Context<Self>,
    ) {
        let cutoff = timestamp.unwrap_or_else(Utc::now);
        self.struck_authors
            .entry(platform)
            .or_default()
            .insert(login.to_lowercase(), cutoff);
        cx.emit(ChannelEvent::Changed);
    }

    fn mark_deleted(&mut self, platform: Platform, message_id: &str, cx: &mut Context<Self>) {
        self.struck_ids
            .entry(platform)
            .or_default()
            .insert(message_id.to_string());
        cx.emit(ChannelEvent::Changed);
    }

    /// Resolves a held AutoMod row in place (a status line replaces its buttons).
    fn resolve_automod(
        &mut self,
        message_id: &str,
        status: bks_platform::AutoModStatus,
        moderator: String,
        cx: &mut Context<Self>,
    ) {
        for row in &mut self.rows {
            if let Row::AutoMod {
                message_id: id,
                resolved,
                ..
            } = row
            {
                if id == message_id {
                    *resolved = Some((status, moderator));
                    cx.emit(ChannelEvent::Changed);
                    break;
                }
            }
        }
    }

    /// Schedules a wakeup at the given platform's current timed pin's expiry, which
    /// drops it from the banner (re-checked against the then-current pin, so a
    /// replaced/unpinned pin's stale wakeup is a no-op). Renders also skip an
    /// expired pin, so this only matters for a quiet chat where nothing repaints.
    fn schedule_pin_expiry(&self, platform: Platform, cx: &mut Context<Self>) {
        let Some(pin) = self.pins.get(&platform) else {
            return;
        };
        let Some(ends_at) = pin.ends_at else { return };
        let msg_id = pin.message.id.clone();
        let wait = (ends_at - chrono::Utc::now())
            .to_std()
            .unwrap_or_default()
            // A little past the deadline so the expiry check can't race it.
            + std::time::Duration::from_millis(50);
        cx.spawn(async move |model, cx| {
            cx.background_executor().timer(wait).await;
            let _ = model.update(cx, |model, cx| {
                let expired = model
                    .pins
                    .get(&platform)
                    .is_some_and(|p| p.message.id == msg_id && p.expired());
                if expired {
                    model.pins.remove(&platform);
                    cx.emit(ChannelEvent::Changed);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Applies one connector event to the shared model. This is the *model half*
    /// of the old `ChatView::push`: everything that is channel state (rows, strikes,
    /// cosmetics, pins, live, emotes, mod status). Per-view concerns (ignore,
    /// mention tint, event-kind filter, panel tailing) are NOT here — views do them.
    pub fn push(&mut self, event: ChatEvent, cx: &mut Context<Self>) {
        match event {
            ChatEvent::Message(msg) => {
                // Global ignore drops the message before it ever enters the shared
                // buffer (so it's gone from every view). Per-tab ignore is separate
                // and hides at render (the message stays for other tabs). A message
                // ignored globally can't be un-ignored per-tab, which is intended.
                if crate::settings::global_ignored(&msg) {
                    return;
                }
                // A channel-point reward message (Twitch `custom-reward-id`) is
                // paired with its "X redeemed …" event so it renders under that
                // header. Hold it here unless the event already showed for this
                // redeemer (then it renders as a normal row *below* the event).
                if msg.reward_id.is_some() {
                    self.handle_reward_message(msg, cx);
                    return;
                }
                self.insert_message(msg.into(), cx);
            }
            ChatEvent::System(text) => tracing::info!("{text}"),
            ChatEvent::Notice(text) => self.row_push_back(Row::System(text), cx),
            ChatEvent::Error(text) => self.row_push_back(Row::Error(text), cx),
            ChatEvent::Event {
                platform,
                kind,
                text,
                timestamp,
                message,
                details,
            } => {
                // An exact replay of a buffered event (a reconnect gap-fill
                // overlapping rows still shown) is skipped up front: letting it
                // reach the retained-events push and only then having
                // `row_push_back`'s dedupe drop the log row would double the
                // events-panel entry.
                if self
                    .row_keys
                    .contains(&crate::chatview::event_row_key(platform, &timestamp, &text))
                {
                    return;
                }
                // A backlog replay: timestamp-sorted into the log like a
                // historical message (faded at render), but deliberately NOT
                // retained for the events panel — it predates the session, and
                // the panel would misread it as live activity. Gift grouping
                // and reward pairing are live/panel concerns, skipped with it.
                if details.historical {
                    let row = Row::Event {
                        platform,
                        kind,
                        text,
                        timestamp,
                        message,
                        accent: details.accent,
                        actor: details.actor,
                        historical: true,
                    };
                    match self.historical_insert_ix(timestamp) {
                        Some(i) => self.row_insert(i, row, cx),
                        None => self.row_push_back(row, cx),
                    }
                } else {
                    self.push_live_event(platform, kind, text, timestamp, message, details, cx);
                }
            }
            ChatEvent::ClearChat {
                platform,
                user,
                historical,
                timestamp,
            } => {
                if let Some(u) = &user {
                    self.mark_banned(platform, u, timestamp, cx);
                }
                // The generic "X was timed out / banned" notice is posted only when
                // no richer source will (`rich_mod_feed`: Twitch's EventSub feed once
                // live, Kick always — seeded at construction). A historical clear is silent.
                if !historical && !self.rich_mod_feed.contains(&platform) {
                    let note = match &user {
                        Some(u) => format!("{u} was timed out / banned"),
                        None => "chat was cleared".to_string(),
                    };
                    self.row_push_back(Row::System(note), cx);
                }
            }
            ChatEvent::ModFeed { platform, active } => {
                if active {
                    self.rich_mod_feed.insert(platform);
                } else {
                    self.rich_mod_feed.remove(&platform);
                }
            }
            ChatEvent::AutoModHeld {
                message_id,
                user,
                text,
                reason,
                ..
            } => self.row_push_back(
                Row::AutoMod {
                    message_id,
                    user,
                    text,
                    reason,
                    resolved: None,
                },
                cx,
            ),
            ChatEvent::AutoModResolved {
                message_id,
                status,
                moderator,
                ..
            } => self.resolve_automod(&message_id, status, moderator, cx),
            ChatEvent::ModStatus {
                platform,
                is_mod,
                is_broadcaster,
            } => {
                // Only a real flip re-measures the log (`Changed` resets every
                // view's list) — Kick re-asserts this from each of our own
                // messages, and Twitch on every reconnect.
                let changed = match platform {
                    Platform::Twitch => {
                        let changed =
                            self.twitch_mod != is_mod || self.twitch_broadcaster != is_broadcaster;
                        self.twitch_mod = is_mod;
                        self.twitch_broadcaster = is_broadcaster;
                        changed
                    }
                    Platform::Kick => {
                        let changed = self.kick_mod != is_mod;
                        self.kick_mod = is_mod;
                        changed
                    }
                    _ => false,
                };
                if changed {
                    cx.emit(ChannelEvent::Changed);
                }
            }
            ChatEvent::DeleteMessage {
                platform,
                message_id,
            } => self.mark_deleted(platform, &message_id, cx),
            ChatEvent::PinMessage {
                platform,
                pinned_by,
                message,
                ends_at,
            } => {
                self.pins.insert(
                    platform,
                    ActivePin {
                        pinned_by,
                        message,
                        ends_at,
                    },
                );
                cx.emit(ChannelEvent::Changed);
                // A timed pin needs a wakeup to clear the banner when it expires,
                // even if chat is otherwise quiet. The model owns the pins, so it
                // schedules this itself (not per-view).
                if ends_at.is_some() {
                    self.schedule_pin_expiry(platform, cx);
                }
            }
            ChatEvent::UnpinMessage { platform } => {
                self.pins.remove(&platform);
                cx.emit(ChannelEvent::Changed);
            }
            ChatEvent::Live {
                platform,
                live,
                title,
                game,
                started_at,
                last_stream,
                link,
            } => {
                // Post a live/offline *notice row* only when the flag actually
                // flips (an in-place update — a late start time, or Kick's offline
                // seed on join — updates state silently).
                let prev_live = self.live_status.get(&platform).map(|p| p.live);
                let flag_changed = match prev_live {
                    Some(prev) => prev != live,
                    None => live,
                };
                self.live_status.insert(
                    platform,
                    LiveInfo {
                        live,
                        title: title.clone(),
                        game,
                        started_at,
                        last_stream,
                        link,
                    },
                );
                // A stale viewer count makes no sense once offline (the polls
                // also emit `Viewers(None)`, but push-based transitions don't).
                if !live {
                    self.viewer_counts.remove(&platform);
                }
                if flag_changed {
                    self.row_push_back(
                        Row::Live {
                            platform,
                            live,
                            title,
                        },
                        cx,
                    );
                    // Going live (not offline) fans out for the tab-strip flash.
                    if live {
                        cx.emit(ChannelEvent::WentLive { platform });
                    }
                } else {
                    cx.emit(ChannelEvent::Changed);
                }
            }
            ChatEvent::Viewers { platform, count } => {
                // Deduped: Twitch's Hermes push re-sends an unchanged number
                // every ~30s, which must not fan out repaints. A count for a
                // platform known to be offline is a late frame racing the
                // offline transition — dropped so it can't resurrect a stale
                // entry the offline `Live` just cleared.
                let changed = match count {
                    Some(n) => {
                        let live = self
                            .live_status
                            .get(&platform)
                            .is_none_or(|s| s.live);
                        live && self.viewer_counts.insert(platform, n) != Some(n)
                    }
                    None => self.viewer_counts.remove(&platform).is_some(),
                };
                if changed {
                    cx.emit(ChannelEvent::ViewersChanged);
                }
            }
            ChatEvent::ChatModes { platform, modes } => {
                // Deduped: a reconnect's first ROOMSTATE re-emits the snapshot
                // unconditionally (so a mode toggled while disconnected can't
                // go stale), which must not fan out repaints when nothing moved.
                let changed = if modes.any() {
                    self.chat_modes.insert(platform, modes) != Some(modes)
                } else {
                    self.chat_modes.remove(&platform).is_some()
                };
                if changed {
                    cx.emit(ChannelEvent::ChatModesChanged);
                }
            }
            ChatEvent::Emotes { platform, emotes } => {
                match platform {
                    Platform::Kick => self.emotes_kick = emotes,
                    Platform::YouTube => self.emotes_youtube = emotes,
                    _ => self.emotes_twitch = emotes,
                }
                // Views refilter their open picker on this Changed (picker reads
                // emotes live from the model).
                cx.emit(ChannelEvent::Changed);
            }
            ChatEvent::Cosmetics {
                platform,
                user_id,
                paint,
                badge,
            } => {
                self.cosmetics
                    .entry(platform)
                    .or_default()
                    .insert(user_id, bks_emotes::Cosmetics { paint, badge });
                cx.emit(ChannelEvent::Changed);
            }
            ChatEvent::Channel(_) => {}
        }
        // Ring-buffer trim (once, in the model).
        while self.rows.len() > MAX_ROWS {
            self.row_pop_front(cx);
        }
        cx.notify();
    }
}

/// The message of a new row, cloned to ride its `Appended`/`Inserted` event
/// (an `Arc` bump for chat rows): a plain chat message, or a sub/resub event's
/// attached chatter message — it can mention someone like any chat line.
fn row_message(row: &Row) -> Option<std::sync::Arc<Message>> {
    match row {
        Row::Message { msg } => Some(msg.clone()),
        Row::Event {
            message: Some(msg), ..
        } => Some(std::sync::Arc::new((**msg).clone())),
        _ => None,
    }
}

/// Whether a new row replays the join backlog (rides the row's
/// `Appended`/`Inserted` event — see [`ChannelEvent::Appended`]).
fn row_historical(row: &Row) -> bool {
    match row {
        Row::Message { msg } => msg.historical,
        Row::Event { historical, .. } => *historical,
        _ => false,
    }
}

/// The process-wide registry of live channel models, keyed by [`ChannelKey`].
/// Holds only weak refs so a model drops when its last view closes.
#[derive(Default)]
struct ChannelStore {
    channels: HashMap<ChannelKey, WeakEntity<ChannelModel>>,
}

impl Global for ChannelStore {}

/// Returns the shared [`ChannelModel`] for `key`, connecting on first use. Later
/// views of the same key get the same entity (shared buffer + connection). An
/// empty key (unconfigured tab) always gets a fresh, unregistered model.
pub fn get_or_create(
    key: ChannelKey,
    config_twitch: &str,
    config_kick: &str,
    config_youtube: &str,
    session: Session,
    cx: &mut App,
) -> Entity<ChannelModel> {
    if !cx.has_global::<ChannelStore>() {
        cx.set_global(ChannelStore::default());
    }
    // Reuse a live model for this key.
    if !key.is_empty() {
        if let Some(weak) = cx.global::<ChannelStore>().channels.get(&key) {
            if let Some(model) = weak.upgrade() {
                return model;
            }
        }
    }

    let model = build_model(config_twitch, config_kick, config_youtube, session, cx);
    if !key.is_empty() {
        cx.global_mut::<ChannelStore>()
            .channels
            .insert(key, model.downgrade());
    }
    model
}

/// Builds a fresh model: opens the connection and spawns the drain task that feeds
/// the model. The drain lives on the model entity, so dropping the model ends it.
fn build_model(
    twitch: &str,
    kick: &str,
    youtube: &str,
    session: Session,
    cx: &mut App,
) -> Entity<ChannelModel> {
    let (rx, controller) = crate::bridge::connect(session, twitch, kick, youtube);
    cx.new(|cx| {
        let drain = cx.spawn(async move |weak: WeakEntity<ChannelModel>, cx| {
            while let Ok(event) = rx.recv().await {
                let ok = weak.update(cx, |model, cx| {
                    model.push(event, cx);
                    // Coalesce a burst: apply every queued event before yielding, so
                    // a busy channel repaints once per burst (via the notifies above).
                    while let Ok(event) = rx.try_recv() {
                        model.push(event, cx);
                    }
                });
                if ok.is_err() {
                    break; // model dropped (last view closed)
                }
            }
        });
        ChannelModel {
            rows: VecDeque::new(),
            events: VecDeque::new(),
            events_base: 0,
            pending_gifts: HashMap::new(),
            row_keys: HashSet::new(),
            rows_generation: 0,
            struck_ids: HashMap::new(),
            struck_authors: HashMap::new(),
            cosmetics: HashMap::new(),
            pins: HashMap::new(),
            live_status: HashMap::new(),
            viewer_counts: HashMap::new(),
            chat_modes: HashMap::new(),
            emotes_twitch: Vec::new(),
            emotes_kick: Vec::new(),
            emotes_youtube: Vec::new(),
            twitch_mod: false,
            twitch_broadcaster: false,
            kick_mod: false,
            // Kick's connector always posts its own rich ban/timeout/unban notices
            // (`kick/connector.rs`), so it's a "rich mod feed" from the start —
            // seeded here so the generic-notice suppression below needn't name it.
            rich_mod_feed: HashSet::from([Platform::Kick]),
            pending_reward_msgs: HashMap::new(),
            recent_reward_events: HashMap::new(),
            controller,
            _drain: drain,
        }
    })
}

/// Assigns a mass gift's per-recipient event to its batch announcement.
///
/// Twitch sends a community gift as one "X is gifting 50 subs" USERNOTICE
/// followed by 50 individual "gifted a sub to Y" USERNOTICEs, with no reliable
/// id tying them together at the IRC surface — so the store counts instead
/// (Chatterino does the same): a batch announcement (`gift_count > 1`)
/// registers `(its seq, count)` under its gifter, and each following
/// per-recipient gift from that gifter consumes one slot and returns the
/// announcement's seq as its group. Single gifts (no pending batch) stay
/// ungrouped; a batch that never completes (missed frames) leaves a stale
/// pending entry that the gifter's next announcement replaces — harmless.
fn gift_group(
    pending: &mut HashMap<(Platform, String), (u64, u32)>,
    details: &bks_platform::EventDetails,
    platform: Platform,
    next_seq: u64,
) -> Option<u64> {
    let gifter = details.gifter.clone()?;
    let key = (platform, gifter);
    if let Some(count) = details.gift_count {
        // Even a count-1 batch registers: its one per-recipient event must
        // group under the announcement or both would show.
        if count > 0 {
            pending.insert(key, (next_seq, count));
        }
        return None;
    }
    if details.recipient.is_some() {
        if let Some((seq, remaining)) = pending.get_mut(&key) {
            let group = Some(*seq);
            *remaining -= 1;
            if *remaining == 0 {
                pending.remove(&key);
            }
            return group;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use bks_platform::EventDetails;

    fn summary(gifter: &str, count: u32) -> EventDetails {
        EventDetails {
            gift_count: Some(count),
            gifter: Some(gifter.to_string()),
            ..Default::default()
        }
    }

    fn child(gifter: &str, recipient: &str) -> EventDetails {
        EventDetails {
            gifter: Some(gifter.to_string()),
            recipient: Some(recipient.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn gift_children_group_under_their_announcement() {
        let mut pending = HashMap::new();
        let p = Platform::Twitch;
        assert_eq!(gift_group(&mut pending, &summary("rich", 2), p, 10), None);
        assert_eq!(
            gift_group(&mut pending, &child("rich", "a"), p, 11),
            Some(10)
        );
        assert_eq!(
            gift_group(&mut pending, &child("rich", "b"), p, 12),
            Some(10)
        );
        // Batch exhausted: a later single gift from the same gifter is its own row.
        assert_eq!(gift_group(&mut pending, &child("rich", "c"), p, 13), None);
    }

    #[test]
    fn unrelated_gifters_and_platforms_do_not_group() {
        let mut pending = HashMap::new();
        assert_eq!(
            gift_group(&mut pending, &summary("rich", 5), Platform::Twitch, 1),
            None
        );
        // Different gifter: no group.
        assert_eq!(
            gift_group(&mut pending, &child("other", "a"), Platform::Twitch, 2),
            None
        );
        // Same gifter, different platform: no group.
        assert_eq!(
            gift_group(&mut pending, &child("rich", "a"), Platform::Kick, 3),
            None
        );
        // Events without gift data never touch the map.
        assert_eq!(
            gift_group(
                &mut pending,
                &EventDetails::default(),
                Platform::Twitch,
                4
            ),
            None
        );
    }

    #[test]
    fn count_one_batch_still_groups_its_single_child() {
        let mut pending = HashMap::new();
        let p = Platform::Twitch;
        // Twitch sends "is gifting 1 sub" + one per-recipient event; the child
        // must fold under the announcement or both rows would show.
        assert_eq!(gift_group(&mut pending, &summary("rich", 1), p, 1), None);
        assert_eq!(gift_group(&mut pending, &child("rich", "a"), p, 2), Some(1));
        assert!(pending.is_empty());
    }
}
