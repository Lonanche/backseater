//! The per-tab chat feed view (`ChatView`): the virtualized log, input box,
//! usercard/emote popups, and reply bar for one channel connection. Owns its
//! connection via [`Controller`]; the app (`BackseaterApp`) hosts one per tab.
//! The emote picker lives in the [`picker`] child module.
//!
//! Split out of `main.rs`; the shared row types live here too since only this
//! view uses them.

mod log;
mod picker;

use std::collections::{HashMap, HashSet};

use bks_core::Message;
use bks_platform::EventKind;
use gpui::prelude::*;
use gpui::{
    div, img, px, App, Context, Entity, FollowMode, FontWeight, ListAlignment, ListState,
    MouseButton, Pixels, Point, SharedString, Window,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::dialog::DialogButtonProps;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::scroll::ScrollableElement;
use gpui_component::tooltip::Tooltip;
use gpui_component::{h_flex, v_flex, ActiveTheme, Sizable, WindowExt};

use crate::controller::Controller;
use crate::image_cache::LruImageCache;
use crate::session::Session;
use crate::tabs::{self, TabConfig};
use crate::{
    child_window, commands, controller, render, selectable, usercard, viewerlist,
    USERCARD_MESSAGES,
};
use log::LogView;
use picker::{EmoteCell, PickerRow};

/// How long an image stays cached after it was last drawn. Past this, the eviction
/// sweep frees its frames (it re-loads from its URL when next drawn). Matches
/// Chatterino's `IMAGE_POOL_IMAGE_LIFETIME` (10 min). Long on purpose: gpui's image
/// loader re-downloads on a miss (no disk cache), so a short lifetime makes emotes
/// that scroll off and back churn the network and load slowly. 10 min means normal
/// scrolling never re-fetches, while a multi-hour session still bounds memory.
const EMOTE_LIFETIME: std::time::Duration = std::time::Duration::from_secs(600);

/// How often the eviction sweep runs. Matches Chatterino's
/// `IMAGE_POOL_CLEANUP_INTERVAL` (1 min).
const EMOTE_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// How long the status bar's viewer count takes to count from the old number to
/// a freshly pushed one, and how often it repaints while doing so (~20fps on its
/// own per-view coalesced timer — plenty for a rolling number; the animated
/// emotes run on their separate shared 20ms grid in `animated_img`).
const VIEWER_ANIM_DURATION: std::time::Duration = std::time::Duration::from_millis(900);
const VIEWER_ANIM_TICK: std::time::Duration = std::time::Duration::from_millis(50);

/// How long the pointer must rest on a link before its preview tooltip appears,
/// and the grace after leaving before it's hidden (a quick leave-and-return
/// keeps it up). Mirrors the tab-chip tooltip's timings.
const LINK_PREVIEW_SHOW_DELAY: std::time::Duration = std::time::Duration::from_millis(350);
const LINK_PREVIEW_HIDE_GRACE: std::time::Duration = std::time::Duration::from_millis(200);

/// Repaint cadence while a jumped-to row's flash fades (~30fps for a smooth
/// fade), and how long the aged-out "no longer in history" note lingers.
const FLASH_TICK: std::time::Duration = std::time::Duration::from_millis(33);
const JUMP_NOTE_DURATION: std::time::Duration = std::time::Duration::from_millis(4000);

/// One platform's in-flight viewer-count animation: the shown value eases from
/// `from` toward `to` over [`VIEWER_ANIM_DURATION`]. Retargeting mid-flight
/// restarts from the currently *shown* value, so a burst of updates stays smooth.
struct ViewerAnim {
    from: u64,
    to: u64,
    started: std::time::Instant,
}

impl ViewerAnim {
    fn value_at(&self, now: std::time::Instant) -> u64 {
        let t = now.duration_since(self.started).as_secs_f32()
            / VIEWER_ANIM_DURATION.as_secs_f32();
        eased_count(self.from, self.to, t)
    }

    fn done(&self, now: std::time::Instant) -> bool {
        // `from == to` is a first/settled count that never moves — without this,
        // its 900ms window would schedule repaint ticks for a static number.
        self.from == self.to || now.duration_since(self.started) >= VIEWER_ANIM_DURATION
    }
}

/// The count shown at progress `t` (0..=1) between `from` and `to`, ease-out
/// (fast start, settling into the final digits — the Twitch-style roll).
fn eased_count(from: u64, to: u64, t: f32) -> u64 {
    if t >= 1.0 {
        return to;
    }
    let t = t.max(0.0);
    let eased = (1.0 - (1.0 - t) * (1.0 - t)) as f64;
    (from as f64 + (to as f64 - from as f64) * eased).round() as u64
}

/// Default size of the usercard child window (header + mod actions + recent
/// messages fit without scrolling); the OS resizes it freely from there.
const USERCARD_WINDOW_SIZE: gpui::Size<Pixels> = gpui::Size {
    width: px(440.),
    height: px(620.),
};
/// Smallest the usercard window can be resized to.
const USERCARD_MIN_SIZE: gpui::Size<Pixels> = gpui::Size {
    width: px(360.),
    height: px(300.),
};

/// The longest timeout each platform accepts: Helix rejects durations over
/// 1,209,600s (2 weeks); Kick's ban API takes minutes capped at 7 days. Gates
/// which preset chips show and what the custom box allows.
fn max_timeout_secs(platform: bks_core::Platform) -> u32 {
    match platform {
        bks_core::Platform::Kick => 604_800,
        _ => 1_209_600,
    }
}

/// Default size of the viewer-list child window (a narrow name column).
const VIEWERLIST_WINDOW_SIZE: gpui::Size<Pixels> = gpui::Size {
    width: px(320.),
    height: px(560.),
};
/// Smallest the viewer-list window can be resized to.
const VIEWERLIST_MIN_SIZE: gpui::Size<Pixels> = gpui::Size {
    width: px(240.),
    height: px(280.),
};

/// A row in the chat log: a real message or a connector notice. Message rows hold
/// an immutable [`Arc<Message>`] so the same message can be shared cheaply (the
/// coming shared-channel store); its removed/decorated state (struck on ban/
/// timeout/delete, 7TV cosmetics) lives in per-view side-tables ([`struck_ids`]/
/// [`struck_authors`]/[`cosmetics`]) resolved at render time, not mutated onto the
/// message.
pub(crate) enum Row {
    Message {
        msg: std::sync::Arc<Message>,
    },
    System(String),
    /// A user-facing error (login/send/moderation failure, a bad command, ...),
    /// shown as a distinct, selectable, copyable row so it can be pasted into a bug
    /// report. Carries the full error text.
    Error(String),
    /// A public channel event (sub/gift/raid), shown as a highlighted row. The
    /// `kind` lets the events panel filter which kinds it lists. `message` is the
    /// chatter's attached sub message as a full message (author + badges + body),
    /// rendered as a normal chat line under the system text; `None` when the
    /// event has none. `timestamp` is shown by the events panel.
    Event {
        platform: bks_core::Platform,
        kind: EventKind,
        text: String,
        timestamp: chrono::DateTime<chrono::Utc>,
        message: Option<Box<Message>>,
        /// A platform-assigned highlight color (Twitch announcement colors)
        /// overriding the kind's default accent; see `EventDetails::accent`.
        accent: Option<u32>,
        /// The acting user's display name (`EventDetails::actor`) when the
        /// connector supplied one, so the log can make the leading name (e.g.
        /// the person redeeming / subscribing) clickable to open their usercard.
        actor: Option<String>,
    },
    /// A stream going live or offline, shown as a highlighted notice row (green
    /// on live, muted on offline) with the platform icon.
    Live {
        platform: bks_core::Platform,
        live: bool,
        title: String,
    },
    /// A message AutoMod held for review (arrives only while the logged-in user
    /// moderates the channel, via the EventSub feed). Shows the chatter, the
    /// held text, and why, with Allow/Deny actions until `resolved` is set (a
    /// moderator acted, or the hold expired) — then a status line replaces the
    /// buttons. `message_id` keys the approve/deny call and the later update.
    AutoMod {
        message_id: String,
        user: String,
        text: String,
        reason: String,
        resolved: Option<(bks_platform::AutoModStatus, String)>,
    },
}

/// One platform's active pinned message (both Twitch and Kick allow a single
/// mod pin per channel), shown as a banner above the log until it's unpinned,
/// expires, or the user dismisses it.
#[derive(Clone)]
pub(crate) struct ActivePin {
    /// Display name of the pinning moderator (empty when unknown).
    pub(crate) pinned_by: String,
    /// The pinned chat message, rendered like a normal chat line in the banner.
    pub(crate) message: Box<Message>,
    /// When the pin expires; `None` = until unpinned / stream end.
    pub(crate) ends_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl ActivePin {
    pub(crate) fn expired(&self) -> bool {
        self.ends_at.is_some_and(|t| t <= chrono::Utc::now())
    }
}

/// A jumped-to message (from a clicked mention) briefly flashed so the eye can
/// find it after the log scrolls to reveal it. Keyed by the message's identity
/// (`platform` + `id`) so the log render can tint exactly that one row, with the
/// tint easing to transparent over [`FLASH_DURATION`] from `started_at`.
#[derive(Clone)]
pub(crate) struct FlashTarget {
    pub(crate) platform: bks_core::Platform,
    pub(crate) msg_id: String,
    pub(crate) started_at: std::time::Instant,
}

/// How long the jumped-to row's flash tint takes to fade out.
pub(crate) const FLASH_DURATION: std::time::Duration = std::time::Duration::from_millis(2500);

impl FlashTarget {
    /// Remaining flash strength (1.0 at `started_at`, 0.0 once faded), used as the
    /// tint's opacity. `None` once fully faded so the caller can drop the target.
    pub(crate) fn strength(&self) -> Option<f32> {
        let elapsed = self.started_at.elapsed();
        if elapsed >= FLASH_DURATION {
            return None;
        }
        let t = elapsed.as_secs_f32() / FLASH_DURATION.as_secs_f32();
        // Hold near full briefly, then ease out — a smoothstep on the back half.
        Some((1.0 - t) * (1.0 - t))
    }
}

/// The latest known live status for one platform of a tab, kept by the `ChatView`
/// so the tab strip can show a hover tooltip without polling a second time. Fed by
/// `ChatEvent::Live` (which only fires on a transition, so this holds the value
/// captured at the last live↔offline change). Uptime is `now - started_at`,
/// computed at tooltip-build time so it stays current.
#[derive(Clone, Debug)]
pub(crate) struct LiveInfo {
    pub(crate) live: bool,
    pub(crate) title: String,
    /// The stream's game/category, empty when offline or unknown.
    pub(crate) game: String,
    /// The live stream's own watch URL, when it differs from the channel page
    /// (YouTube's `watch?v=` link). The tooltip's header opens it while live.
    pub(crate) link: Option<String>,
    pub(crate) started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The most recent past broadcast, shown in the offline tooltip. `None` when
    /// live or unknown.
    pub(crate) last_stream: Option<bks_platform::LastStream>,
}

/// A stable identity for rows that can legitimately arrive twice — a reconnect
/// (adding a platform to the tab) refetches the connected platforms' history. A
/// message keys on its platform + id; an event on platform + timestamp + text
/// (events carry no id, but Twitch history replays them with their original
/// send time, so the pair is stable). `None` for row kinds without a natural
/// key (system/error/live notices), which are never deduplicated.
pub(crate) fn row_key(row: &Row) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match row {
        Row::Message { msg, .. } if !msg.id.is_empty() => {
            (0u8, msg.platform, &msg.id).hash(&mut h);
        }
        Row::Event {
            platform,
            text,
            timestamp,
            ..
        } => {
            (1u8, *platform, timestamp, text).hash(&mut h);
        }
        _ => return None,
    }
    Some(h.finish())
}

/// Where a backfilled history message with timestamp `ts` belongs among existing
/// `rows`: the index of the first row that is a live message or a *later*
/// historical message (so history stays timestamp-sorted and ahead of live chat),
/// or `None` to append. Non-message rows are skipped. Free-standing (not a method)
/// so the ordering can be unit-tested without a GPUI `ChatView`.
pub(crate) fn history_insert_index<'a>(
    rows: impl Iterator<Item = &'a Row>,
    ts: chrono::DateTime<chrono::Utc>,
) -> Option<usize> {
    rows.enumerate()
        .find(|(_, row)| match row {
            Row::Message { msg, .. } => !msg.historical || msg.timestamp > ts,
            _ => false,
        })
        .map(|(i, _)| i)
}

/// Live tab-completion state for the input box. Tracks the word being completed
/// and the candidate cycle so repeated Tab presses rotate through
/// matches. Reset whenever the input text diverges from the last
/// completion we wrote (i.e. the user typed something).
struct Completion {
    /// Byte offset in the text where the completed word starts.
    start: usize,
    /// The candidate completions (already including any `@`), in cycle order.
    candidates: Vec<String>,
    /// Index of the candidate currently inserted.
    index: usize,
    /// The full input text after our last insertion, so we can tell our own edit
    /// from the user typing (which restarts the cycle).
    last_text: String,
}

/// The autocomplete popup over the input box: opens when the word at the
/// cursor starts with `@` (chatter mentions — the broadcaster + most recent
/// chatters, see [`ChatView::mention_candidates`]), `:` (emotes from the
/// send target's set(s), see [`ChatView::emote_popup_candidates`]), or `/` at
/// the start of the line (slash commands for the send target's platform, see
/// [`ChatView::command_popup`]). Up/Down move the highlight, Tab/Enter/click insert,
/// Escape dismisses; recomputed on every input change, so typing a space (the
/// word no longer starts with the trigger) closes it naturally. An empty
/// candidate list renders as its `empty_text` notice rather than hiding, so
/// the user can see why nothing completes.
struct InputPopup {
    /// Byte offset of the trigger word's start in the input text.
    start: usize,
    /// Every matching candidate. May be empty (shows `empty_text`).
    items: Vec<PopupItem>,
    /// Index of the highlighted candidate.
    selected: usize,
    /// Index of the first visible row: the popup shows a
    /// [`POPUP_VISIBLE_ITEMS`]-row window onto `items` that follows the
    /// highlight (and the mouse wheel), with "more" hints past each edge.
    window_start: usize,
    /// What an empty `items` renders as — "No matches", or the explanation why
    /// commands are unavailable (Both send mode).
    empty_text: SharedString,
}

/// One candidate in the input autocomplete popup.
enum PopupItem {
    /// A chatter name (no `@`); inserted as `@Name `.
    Mention(String),
    /// An emote, its row showing the image; inserted as its bare name.
    Emote(bks_core::Emote),
    /// A slash command under one spelling (aliases are their own rows), its
    /// row showing usage + description; inserted as `/name `.
    Command(crate::commands::CommandMatch),
}

impl PopupItem {
    /// The text this candidate inserts in place of the trigger word.
    fn insert_text(&self) -> String {
        match self {
            PopupItem::Mention(name) => format!("@{name}"),
            PopupItem::Emote(e) => e.name.clone(),
            PopupItem::Command(m) => format!("/{}", m.name),
        }
    }
}

/// Rows the input popup shows at once; the rest scroll into view as the
/// selection moves past an edge (Up/Down wrap, mouse wheel too).
const POPUP_VISIBLE_ITEMS: usize = 4;

/// How many recently-sent input lines each tab keeps for Up/Down recall.
const SENT_HISTORY_MAX: usize = 5;

/// An in-progress drag of a layout divider: the two adjacent shares at grab
/// time plus the grab position, so each move applies an absolute delta (no
/// accumulation drift).
#[derive(Clone, Copy)]
enum LayoutDrag {
    /// Resizing columns `left` and `left + 1`.
    Column {
        left: usize,
        start: (f32, f32),
        x: Pixels,
    },
    /// Resizing panels `above` and `above + 1` within column `col`.
    Row {
        col: usize,
        above: usize,
        start: (f32, f32),
        y: Pixels,
    },
}

/// A usercard moderation action, dispatched to the right controller method per
/// platform by [`ChatView::usercard_moderate`].
#[derive(Clone, Copy)]
enum Mod {
    Ban,
    Timeout(u32),
    Unban,
}

/// An open emote-info popup: a small card (image + name/provider/author, and an
/// "Open on 7TV" link for 7TV emotes) anchored near where the emote was clicked.
/// Lighter than the draggable usercard — it's a transient info bubble closed by
/// clicking anywhere outside it.
struct EmotePopup {
    name: SharedString,
    url: SharedString,
    /// The provider line ("7TV Emote", "Twitch Emote", …); empty if unknown.
    provider: SharedString,
    /// "By: <author>" when known (7TV), else empty.
    author: SharedString,
    /// The 7TV emote-page URL, set only for 7TV emotes (drives the link button).
    seventv_url: Option<String>,
    /// The emote's 7TV id, when known — used to match a still-loading link popup to
    /// its fetch result so a later click supersedes an earlier one.
    emote_id: String,
    /// Set while a clicked 7TV *link* is still being resolved to an emote; the
    /// popup shows a loading state until the fetch fills the fields in.
    loading: bool,
    /// Top-left anchor (the click position), clamped on render to stay on-screen.
    anchor: Point<Pixels>,
}

impl EmotePopup {
    /// Builds a popup from a clicked emote and the pointer position. 7TV emotes
    /// get a link to their page on 7tv.app (keyed by the emote id).
    fn from_emote(emote: &bks_core::Emote, anchor: Point<Pixels>) -> Self {
        let provider = if emote.tooltip.provider.is_empty() {
            String::new()
        } else {
            format!("{} Emote", emote.tooltip.provider)
        };
        let author = emote
            .tooltip
            .author
            .as_ref()
            .map(|a| format!("By: {a}"))
            .unwrap_or_default();
        let seventv_url = (emote.tooltip.provider == "7TV")
            .then(|| format!("https://7tv.app/emotes/{}", emote.id));
        Self {
            name: SharedString::from(emote.name.clone()),
            url: SharedString::from(emote.url.clone()),
            provider: SharedString::from(provider),
            author: SharedString::from(author),
            seventv_url,
            emote_id: emote.id.clone(),
            loading: false,
            anchor,
        }
    }

    /// A placeholder popup shown immediately when a 7TV link is clicked, before
    /// the emote's data has loaded. `id` is the emote id parsed from the URL.
    fn loading(id: String, anchor: Point<Pixels>) -> Self {
        Self {
            name: SharedString::from("loading…"),
            url: SharedString::default(),
            provider: SharedString::from("7TV Emote"),
            author: SharedString::default(),
            seventv_url: Some(format!("https://7tv.app/emotes/{id}")),
            emote_id: id,
            loading: true,
            anchor,
        }
    }
}

/// An armed link-preview hover: the link URL whose preview tooltip is (about to
/// be) shown, and the pointer position to anchor it near. Set when the pointer
/// enters a previewable link and the show-delay elapses; cleared after the
/// hide-grace once the pointer leaves. See [`ChatView::arm_link_preview`].
struct LinkPreviewHover {
    url: String,
    anchor: Point<Pixels>,
    /// Whether the tooltip card is showing yet (the show-delay elapsed). Until
    /// then the fetch may already be running but nothing is drawn.
    shown: bool,
    /// Whether the pointer is currently over this link. A link renders as several
    /// pieces (wrapping), so the pointer briefly leaves one and enters the next;
    /// the hide-grace clears the preview only if this is still false when it
    /// fires, so crossing a seam doesn't flicker the tooltip away.
    hovering: bool,
}

/// The open thread panel: the id of the message whose "replying to" line was
/// clicked (the chain is rebuilt from the buffer around it each render) and the
/// click position in window coords, so the panel anchors above that line rather
/// than floating in the middle of the chat.
struct ThreadPanel {
    seed_id: String,
    anchor: Point<Pixels>,
}

/// Which just-opened thread views owe a one-shot "scroll to the newest row" on
/// the next render, so they open at the bottom (chat order) instead of the top.
/// Each flag is set when its view opens and cleared once the scroll is applied.
#[derive(Default)]
struct ScrollToNewest {
    /// The thread panel's message list.
    panel: bool,
    /// The reply bar's thread chain.
    reply_chain: bool,
}

/// Selects which [`ScrollToNewest`] flag [`ChatView::open_at_bottom`] consumes.
#[derive(Clone, Copy)]
enum ScrollTarget {
    Panel,
    ReplyChain,
}

/// Emitted by a [`ChatView`] when new *live* activity (a chat message or a
/// public event) lands in its channel, so the app can mark the owning tab
/// unread when it isn't the active one. Emitted from the log's own
/// `Appended`/`EventAppended` handling (not backfilled history), so it survives
/// channel-swap rebuilds (the app subscribes to the stable `ChatView`, not the
/// swappable inner model).
pub(crate) struct TabActivity;

impl gpui::EventEmitter<TabActivity> for ChatView {}

/// Emitted when one of this view's channels goes live (a false→true `live`
/// transition), so the app can briefly flash the owning tab's chip. Carries the
/// platform for the flash tint. Like [`TabActivity`], it rides the stable
/// `ChatView` so the app's subscription survives channel-swap rebuilds.
pub(crate) struct TabWentLive {
    pub platform: bks_core::Platform,
}

impl gpui::EventEmitter<TabWentLive> for ChatView {}

/// One tab's chat feed + input. Owns its connection via [`Controller`].
pub(crate) struct ChatView {
    /// The shared channel model: the canonical row
    /// buffer + connection + per-channel state, shared with every other view on
    /// the same channel. This view observes it and reconciles its own
    /// [`list_state`] from the model's granular row-change events.
    channel: Entity<crate::channel_store::ChannelModel>,
    /// This view's key into the shared-channel registry (its channel set).
    channel_key: crate::channel_store::ChannelKey,
    /// Subscription to the model's row-change events (reconciles `list_state`).
    _channel_sub: gpui::Subscription,
    /// App-wide login, kept so [`reconnect`](Self::reconnect) can open a fresh
    /// connection with the same session.
    session: Session,
    /// Virtualized list backing the log: only on-screen rows are built per frame
    /// (a SumTree of cached item heights), kept in lockstep with the shared
    /// model's `rows` by applying each [`ChannelEvent`] as a matching `splice`.
    /// `ListAlignment::Bottom` + `FollowMode::Tail` give chat-style bottom-stick.
    list_state: ListState,
    input: Entity<InputState>,
    /// The last placeholder pushed onto the input ("Send a message to …"), so
    /// render only calls `set_placeholder` (which notifies) on a real change.
    input_placeholder: String,
    /// The shared connection handle (send/moderation), cloned from the model.
    controller: Controller,
    config: TabConfig,
    /// Chat font size in px (shared app preference, pushed in on change).
    font_size: f32,
    /// Drag-to-select state for the log, shared into each selectable text token.
    selection: selectable::Selection,
    /// Focus for the log so Ctrl/Cmd+C reaches its copy handler (taken on a
    /// mouse-down inside the log; clicking the input box hands focus back).
    focus: gpui::FocusHandle,
    /// Matches messages that mention the user (login names + custom terms); a
    /// match tints the row. Refreshed by the app on login/settings changes.
    mentions: bks_core::MentionMatcher,
    /// This tab's stable identity in the app's tab list (survives a channel-swap
    /// rebuild), tagging mentions pushed to the shared store.
    tab_id: u64,
    /// The app-wide mention feed (see [`crate::mentions`]): mention-matched live
    /// messages land there for the all-tabs panels + the global Mentions tab.
    mention_store: Entity<crate::mentions::MentionStore>,
    /// Mentions matched during the current drain burst, flushed to the shared
    /// store once the burst ends ([`push`](Self::push) has no `Context`).
    /// Live mentions matched this burst, with each message's per-term sound
    /// verdict (computed at match time, against the matcher that matched it).
    pending_mentions: Vec<(Message, bool)>,
    /// Tails + repaints the mentions panel when a mention arrives in *any* tab
    /// (the store notifies on every push; mentions are rare).
    _mention_sub: gpui::Subscription,
    /// Words/phrases (and regexes) whose messages are hidden from chat. Refreshed
    /// by the app when the ignore settings change.
    ignore: bks_core::IgnoreList,
    /// Words/phrases (and regexes) whose messages are dimmed (kept visible at low
    /// opacity) instead of hidden. Refreshed by the app when the suppress settings
    /// change.
    suppress: bks_core::SuppressList,
    /// Pins the user dismissed with the banner's ✕, keyed by `(platform,
    /// message id)` — dismissing hides *that one pin* for this session; the next
    /// pinned message shows again.
    dismissed_pins: std::collections::HashSet<(bks_core::Platform, String)>,
    /// Mass-gift summary rows (by event seq) the user expanded in the events
    /// panel to see the recipient list. Session-only, per view.
    expanded_gifts: std::collections::HashSet<u64>,
    /// Per-platform status-bar viewer-count animation: when a fresh count lands,
    /// the shown number counts up/down to it (Twitch-style) instead of snapping.
    /// Presentation-only, so it lives on the view (popouts animate their own).
    viewer_anims: HashMap<bks_core::Platform, ViewerAnim>,
    /// A viewer-anim repaint tick is already scheduled (one timer per view at a
    /// time, like the animated-emote wakeup coalescing).
    viewer_anim_tick_pending: bool,
    /// The open chatter usercard's data, if any. Opened by clicking a name in
    /// chat (see [`open_usercard`]); shown in its own OS window.
    usercard: Option<usercard::UserCard>,
    /// Custom timeout duration box on the usercard ("90s", "10m", "1h30m", …).
    /// Window-bound like all kit inputs, so it's created against the usercard
    /// window when that opens (`None` while no window is up).
    usercard_timeout_input: Option<Entity<InputState>>,
    _usercard_timeout_sub: Option<gpui::Subscription>,
    /// Inline error under the custom timeout box (bad duration / over the
    /// platform's cap); cleared on a successful apply or a new card.
    usercard_timeout_error: Option<String>,
    /// Warn-reason box on the usercard (Twitch-only — Helix requires a reason
    /// the chatter must acknowledge). Window-bound like the timeout box.
    usercard_warn_input: Option<Entity<InputState>>,
    _usercard_warn_sub: Option<gpui::Subscription>,
    /// Inline error under the warn box (empty/overlong reason); cleared on a
    /// successful apply or a new card.
    usercard_warn_error: Option<String>,
    /// The child OS window hosting the usercard, when open. Clicking another
    /// name re-points (and refocuses) the same window instead of opening more.
    usercard_window: Option<gpui::AnyWindowHandle>,
    /// The open Twitch viewer list's data, if any (see [`viewerlist`]); shown in
    /// its own OS window, opened from the input bar's 👥 button or `/chatters`.
    viewer_list: Option<viewerlist::ViewerList>,
    /// The child OS window hosting the viewer list, when open. Re-opening
    /// refreshes + refocuses it instead of opening more.
    viewer_list_window: Option<gpui::AnyWindowHandle>,
    /// Search box filtering the viewer list by name. Window-bound like all kit
    /// inputs, so it's created against the viewer-list window when that opens
    /// (`None` while no window is up); the subscription is replaced with it.
    viewer_search: Option<Entity<InputState>>,
    _viewer_search_sub: Option<gpui::Subscription>,
    /// The main window this tab renders in — the usercard window positions
    /// itself near it.
    parent_window: gpui::AnyWindowHandle,
    /// The open emote-info popup, if any. Opened by clicking an emote in chat;
    /// closed by clicking anywhere outside it.
    emote_popup: Option<EmotePopup>,
    /// The link whose hover preview tooltip is armed/showing, if any. Driven by
    /// the render layer's link `on_hover` (a show-delay before it appears, a
    /// hide-grace after the pointer leaves). Only ever set when link previews are
    /// in Tooltip mode.
    link_preview: Option<LinkPreviewHover>,
    /// Bumped on every preview arm/disarm so a delayed show/hide timer that fired
    /// for a stale hover no-ops (same guard as the chip tooltip's generation).
    link_preview_gen: u64,
    /// The preview overlay layer's painted window-space origin, measured by a
    /// canvas inside it. gpui offsets an `absolute` overlay that's a flow child
    /// of the (non-`relative`) root by the stacked height of its flow siblings
    /// (the status bar + chrome), so the anchor (window coords) and the overlay's
    /// local coords differ by this; we subtract it when placing the card. Updated
    /// each paint; the 350ms show-delay means it's already correct before the
    /// card is visible.
    link_preview_offset: std::rc::Rc<std::cell::Cell<Point<Pixels>>>,
    /// The message being replied to, if the user clicked a row's reply button. The
    /// next sent line threads under it (on the parent's platform); shown as a
    /// "replying to" bar above the input. Cleared on send or cancel.
    replying_to: Option<controller::ReplyTo>,
    /// When set, the thread panel is open showing the reply chain seeded from this
    /// message id, anchored above the "replying to" line the user clicked (window
    /// coords). Rebuilt from the live buffer each render so it grows as new replies
    /// arrive. Cleared on ✕ or a backdrop click.
    thread_panel: Option<ThreadPanel>,
    /// Caches the last reconstructed reply thread so `build_thread` (called from
    /// the reply bar + thread panel *every* render, i.e. per keystroke while
    /// replying) doesn't re-scan the whole buffer each time. Keyed by
    /// `(seed_id, rows_generation)` — rebuilt only when the seed or the buffer
    /// changes. `RefCell` because `build_thread` takes `&self` (render path).
    thread_cache: std::cell::RefCell<Option<(String, u64, crate::thread::Thread)>>,
    /// While dragging a layout divider: which one, the two adjacent shares, and
    /// the pointer position at grab time, so each move applies a delta. `None`
    /// when not resizing.
    layout_drag: Option<LayoutDrag>,
    /// The layout grid's last-prepainted bounds (written by a measuring `canvas`
    /// inside it), so divider drags can convert pointer deltas to share
    /// fractions of the real panel area.
    grid_bounds: std::rc::Rc<std::cell::Cell<gpui::Bounds<Pixels>>>,
    /// Virtualized list backing the events panel — the retained buffer holds up
    /// to `MAX_EVENTS` rows, and the old plain scroll column rebuilt (and kept
    /// animating) every one of them each frame, which grew choppier the longer
    /// a session ran. Bottom-aligned + tail-following like the log; kept in
    /// lockstep with `events_shown`.
    events_list_state: ListState,
    /// The stable sequence numbers (see `ChannelModel::events_base`) of the
    /// retained events this view's panel shows (its kind filter applied), in
    /// order. Appended/trimmed via the model's `EventAppended`/`EventsTrimmed`;
    /// rebuilt wholesale on a filter change or reconnect.
    events_shown: std::collections::VecDeque<u64>,
    /// Scroll position of the mentions panel (tailed like the events panel).
    mentions_scroll: gpui::ScrollHandle,
    /// Set when a new mention arrived; the mentions panel tails on next render.
    mentions_new: bool,
    /// Scroll position of the thread panel's message list, so it opens at the
    /// newest message with older ones scrollable above (chat order).
    thread_scroll: gpui::ScrollHandle,
    /// Scroll position of the reply bar's thread chain, same bottom-anchoring.
    reply_chain_scroll: gpui::ScrollHandle,
    /// Set when the thread panel opens (or the reply chain first appears): the
    /// next render snaps the corresponding list to its newest row. `Thread` for
    /// the panel, `Reply` for the composer's chain — cleared once applied.
    scroll_to_newest: ScrollToNewest,
    /// The logged-in user's personal Twitch emotes (sub/follower/global) —
    /// everything they can actually use, fetched lazily on the first picker
    /// open or `:`/Tab completion. Feeds the picker AND autocomplete.
    personal_emotes: Vec<bks_core::Emote>,
    /// The viewed Twitch channel's remaining native emotes (ones the user
    /// can't use — not subscribed), shown locked-style in the picker like
    /// Twitch web but deliberately NOT autocompleted (they'd send as text).
    channel_native_emotes: Vec<bks_core::Emote>,
    /// Whether the emote picker panel is open (toggled by its input-bar button).
    picker_open: bool,
    /// Which platform's emotes the picker is currently showing (its tab).
    picker_tab: bks_core::Platform,
    /// Search box filtering the emote picker by name (substring, ci).
    picker_search: Entity<InputState>,
    /// Virtualizes the picker grid: one list item per *row* (a section header or a
    /// row of emotes), so only on-screen rows are built (large 7TV sets stay light).
    /// Kept in lockstep with `picker_rows`' length via `reset` on every refilter.
    picker_list_state: ListState,
    /// The picker's display rows for the active tab: section headers (grouped by
    /// emote provider) interleaved with rows of emotes, after the
    /// search filter. Recomputed when the search text, the tab, or the sets change.
    picker_rows: Vec<PickerRow>,
    /// Persistent [`EmoteCell`] views, keyed by emote url. Each picker emote is its
    /// own cached view so an animation tick repaints only that cell, not the whole
    /// grid (see [`EmoteCell`]). Cells must persist across renders (a fresh view each
    /// frame can't be cache-reused); rebuilt to the current filtered set on refilter.
    picker_cells: HashMap<SharedString, Entity<EmoteCell>>,
    /// Active Tab-completion cycle for the input box, if any.
    completion: Option<Completion>,
    /// The input autocomplete popup, when the word at the cursor is an
    /// `@`-mention or a `:`-emote (see [`InputPopup`]).
    popup: Option<InputPopup>,
    /// The duration selected in the open pin-confirmation dialog (`None` =
    /// until the stream ends). Reset to the default on each dialog open.
    pin_duration_choice: Option<u32>,
    /// The message id of the row currently showing its mod-button strip in
    /// "On hover" mode (`None` in the other modes / when no row is hovered).
    /// Tracked view-side and applied on the next log render — a group-hover
    /// display switch inside the row panics when hover flips mid-frame.
    hover_strip_row: Option<String>,
    /// A message the user jumped to (clicked in the mentions feed) that's briefly
    /// flashed in the log to catch the eye; cleared once faded. A repaint tick is
    /// re-armed while it's set (`flash_tick_pending`), like the viewer anim.
    flash: Option<FlashTarget>,
    /// A flash-fade repaint tick is already scheduled (one timer per view).
    flash_tick_pending: bool,
    /// A transient note shown over the log when a mention jump can't land (the
    /// message aged out of the buffer), with the time it appeared so it fades on
    /// the same tick as the row flash. View-local (not a shared model notice).
    jump_note: Option<(SharedString, std::time::Instant)>,
    /// Whether the pointer is over this view's chat-log region (tracked by the
    /// log wrapper's `on_hover`; the composer/input bar is outside it).
    log_hovered: bool,
    /// Hover-pause is engaged: appended rows are withheld from [`list_state`]
    /// (the shared model keeps them; [`unpause_log`](Self::unpause_log) splices
    /// the tail back in), so a tail-following log holds still under the pointer.
    /// Only ever engaged while following the tail — a view the user scrolled up
    /// doesn't move on appends anyway.
    log_paused: bool,
    /// A [`ChannelEvent::Changed`] arrived while paused: its full re-measure
    /// (`list_state.reset`) would snap the frozen view to the live bottom, so
    /// it's deferred to the unpause.
    paused_needs_reset: bool,
    /// Set once the personal Twitch emotes have been fetched, so we only hit
    /// Helix the first time the picker opens (not on every toggle).
    emotes_fetched: bool,
    /// This tab's recently-sent input lines, newest first, capped at
    /// [`SENT_HISTORY_MAX`]. Up/Down (when no autocomplete popup is open) recalls
    /// them into the input to edit and resend, shell-style. `/commands` and chat
    /// lines both count; UI-only commands (`/chatters`) don't.
    sent_history: Vec<String>,
    /// While browsing `sent_history` with Up/Down, the index currently shown
    /// (0 = newest). `None` means we're on the live draft (not browsing).
    history_index: Option<usize>,
    /// The in-progress text stashed when history browsing starts, restored when
    /// you press Down past the newest entry (so browsing never loses your draft).
    history_draft: String,
    /// True for exactly the one Change event a programmatic history recall causes,
    /// so its `set_value` doesn't read as the user manually editing (which would
    /// end browsing). Consumed by the Change handler.
    history_setting: bool,
    /// Scoped image cache the chat log's emote/badge/icon `img()`s render through.
    /// Unlike gpui's global asset cache (which never evicts — every image ever
    /// drawn stays decoded in RAM for the process lifetime), this one tracks each
    /// image's last-drawn time *inside its `load`* (called by gpui for every image
    /// actually rendered each frame) and a timer sweep frees off-screen ones (see
    /// [`crate::image_cache`]). On-screen images are re-stamped every frame, so they
    /// are never evicted; off-screen ones re-decode transparently when scrolled
    /// back. This needs no per-image-kind bookkeeping in the UI.
    image_cache: Entity<LruImageCache>,
    /// The chat-log region as its own **cached child view** (see [`log`]): a
    /// picker-emote animation tick dirties this `ChatView` (ancestors are always
    /// dirtied), but the heavy log subtree reuses its cached prepaint/paint
    /// unless *it* was notified. Everything that changes log content must go
    /// through [`refresh_log`](Self::refresh_log).
    log_view: Entity<LogView>,
    _input_sub: gpui::Subscription,
    _picker_search_sub: gpui::Subscription,
}

impl ChatView {
    #[allow(clippy::too_many_arguments)] // A constructor threading app context.
    pub(crate) fn new(
        session: Session,
        config: TabConfig,
        font_size: f32,
        mentions: bks_core::MentionMatcher,
        ignore: bks_core::IgnoreList,
        suppress: bks_core::SuppressList,
        tab_id: u64,
        mention_store: Entity<crate::mentions::MentionStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Bottom-aligned + tail-following: new rows stick to the bottom, and the
        // follow re-engages when the user scrolls back down. `overdraw` measures a
        // little beyond the viewport so scrolling doesn't pop in.
        let list_state = ListState::new(0, ListAlignment::Bottom, px(400.));
        list_state.set_follow_mode(FollowMode::Tail);
        // The log renders through its own cached child view (see `log`), so a
        // picker animation tick doesn't rebuild the log's rows.
        let weak_self = cx.entity().downgrade();
        let log_view = cx.new(|_| LogView::new(weak_self));
        // Repaint the log on scroll: the scrolled content and the "jump to
        // latest" pill both live in the cached `LogView`, which must be dirtied
        // explicitly or its cached paint (at the old scroll offset) is reused.
        {
            let weak = log_view.downgrade();
            list_state.set_scroll_handler(move |_ev, _window, cx| {
                let _ = weak.update(cx, |_, cx| cx.notify());
            });
        }
        // Join (or attach to) the shared channel model: the canonical row buffer
        // + connection, shared with every other view on the same channel.
        let channel_key = crate::channel_store::ChannelKey::new(
            &config.twitch_channel,
            &config.kick_channel,
            &config.youtube_channel,
        );
        let channel = crate::channel_store::get_or_create(
            channel_key.clone(),
            &config.twitch_channel,
            &config.kick_channel,
            &config.youtube_channel,
            session.clone(),
            cx,
        );
        let controller = channel.read(cx).controller.clone();
        // Seed list_state to the model's current rows (an attach to an already-live
        // channel starts with a full buffer), then reconcile each change onto our
        // OWN list_state (the shared model can't touch per-view scroll state).
        list_state.reset(channel.read(cx).len());
        let _channel_sub = cx.subscribe(&channel, Self::on_channel_event);

        // The events panel is virtualized like the log; seed it with the shared
        // buffer's current events that pass this tab's kind filter.
        let events_list_state = ListState::new(0, ListAlignment::Bottom, px(200.));
        events_list_state.set_follow_mode(FollowMode::Tail);
        let events_shown =
            filtered_event_seqs(channel.read(cx), config.event_kinds, config.collapse_gift_subs);
        events_list_state.reset(events_shown.len());

        // The placeholder is kept current by `composer_placeholder` on render
        // ("Send a message to Twitch + Kick", a login hint when logged out).
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(" Send a message"));
        let _input_sub = cx.subscribe_in(&input, window, Self::on_input_event);

        let picker_search = cx.new(|cx| InputState::new(window, cx).placeholder("Search emotes…"));
        // Re-filter the grid as the user types in the search box.
        let _picker_search_sub =
            cx.subscribe_in(&picker_search, window, Self::on_picker_search_event);
        // Small overdraw on purpose: overdrawn rows are laid out + painted (just
        // clipped), so their emotes animate invisibly — a big overdraw nearly
        // doubles how many cells tick while the picker is open.
        let picker_list_state = ListState::new(0, ListAlignment::Top, px(40.));

        // Default the picker to the platform this tab actually has (Twitch when
        // present, else Kick, else YouTube), so a single-platform tab opens on its
        // own emotes.
        let picker_tab = if !config.twitch_channel.is_empty() {
            bks_core::Platform::Twitch
        } else if !config.kick_channel.is_empty() {
            bks_core::Platform::Kick
        } else {
            bks_core::Platform::YouTube
        };

        // The app-wide image cache (shared across all tabs + picker), disk-backed
        // and swept of off-screen images on a single timer (see `crate::image_cache`).
        let image_cache = LruImageCache::shared(EMOTE_LIFETIME, EMOTE_SWEEP_INTERVAL, cx);

        let _mention_sub = cx.observe(&mention_store, |this, _, cx| {
            this.mentions_new = true;
            cx.notify();
        });

        // A closed (or rebuilt) tab takes its usercard + viewer-list windows
        // with it.
        cx.on_release(|this: &mut Self, cx: &mut App| {
            if let Some(handle) = this.usercard_window.take() {
                let _ = handle.update(cx, |_, window, _| window.remove_window());
            }
            if let Some(handle) = this.viewer_list_window.take() {
                let _ = handle.update(cx, |_, window, _| window.remove_window());
            }
        })
        .detach();

        let mut this = Self {
            channel,
            channel_key,
            _channel_sub,
            session,
            list_state,
            input,
            input_placeholder: String::new(),
            controller,
            config,
            font_size,
            selection: selectable::Selection::new(),
            focus: cx.focus_handle(),
            mentions,
            tab_id,
            mention_store,
            pending_mentions: Vec::new(),
            _mention_sub,
            ignore,
            suppress,
            dismissed_pins: std::collections::HashSet::new(),
            expanded_gifts: std::collections::HashSet::new(),
            viewer_anims: HashMap::new(),
            viewer_anim_tick_pending: false,
            usercard: None,
            usercard_timeout_input: None,
            _usercard_timeout_sub: None,
            usercard_timeout_error: None,
            usercard_warn_input: None,
            _usercard_warn_sub: None,
            usercard_warn_error: None,
            usercard_window: None,
            viewer_list: None,
            viewer_list_window: None,
            viewer_search: None,
            _viewer_search_sub: None,
            parent_window: window.window_handle(),
            emote_popup: None,
            link_preview: None,
            link_preview_gen: 0,
            link_preview_offset: std::rc::Rc::new(std::cell::Cell::new(Point::default())),
            replying_to: None,
            thread_panel: None,
            thread_cache: std::cell::RefCell::new(None),
            layout_drag: None,
            grid_bounds: std::rc::Rc::new(std::cell::Cell::new(gpui::Bounds::default())),
            events_list_state,
            events_shown,
            mentions_scroll: gpui::ScrollHandle::new(),
            thread_scroll: gpui::ScrollHandle::new(),
            reply_chain_scroll: gpui::ScrollHandle::new(),
            scroll_to_newest: ScrollToNewest::default(),
            mentions_new: false,
            personal_emotes: Vec::new(),
            channel_native_emotes: Vec::new(),
            picker_open: false,
            picker_tab,
            picker_search,
            picker_list_state,
            picker_rows: Vec::new(),
            picker_cells: HashMap::new(),
            completion: None,
            popup: None,
            pin_duration_choice: Some(controller::Controller::PIN_DURATION_SECS),
            hover_strip_row: None,
            flash: None,
            flash_tick_pending: false,
            jump_note: None,
            log_hovered: false,
            log_paused: false,
            paused_needs_reset: false,
            emotes_fetched: false,
            sent_history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            history_setting: false,
            image_cache,
            log_view,
            _input_sub,
            _picker_search_sub,
        };
        // Fetch the personal Twitch emote set right away (saved logins load
        // before tabs are built) so autocomplete has cross-channel sub emotes
        // from the start, not on the first `:`/picker open.
        this.ensure_personal_emotes(cx);
        this
    }

    /// Handles a change to the shared channel model: reconcile our own
    /// `list_state` with the model's `rows` (each structural edit mirrored so the
    /// virtualized log stays in lockstep), tail the aux panels, schedule pin
    /// expiry, and repaint. This replaces the old per-view drain loop — the model
    /// owns the connection now.
    fn on_channel_event(
        &mut self,
        _model: Entity<crate::channel_store::ChannelModel>,
        event: &crate::channel_store::ChannelEvent,
        cx: &mut Context<Self>,
    ) {
        use crate::channel_store::ChannelEvent;
        match event {
            ChannelEvent::Appended { index, msg } => {
                // Engage hover-pause lazily too (not just on hover-enter): the
                // user may have scrolled back to the bottom while hovering.
                self.maybe_pause_log(cx);
                // While paused, appended rows are withheld from the list (the
                // unpause splices the missing tail back in); the view keeps
                // showing `rows[0..item_count]`, whose indices stay valid.
                if !self.log_paused {
                    self.list_state.splice(*index..*index, 1);
                }
                // A new row may be a mention (for our terms) — flag the mentions
                // panel to tail + feed the all-tabs store.
                self.note_new_row(msg.as_deref());
                if let Some(m) = msg.as_deref() {
                    self.arm_inline_preview(m, cx);
                }
                // Live activity → the app marks this tab unread if it's inactive.
                // A historical row can also arrive via `Appended` (a backfilled
                // message newer than everything currently buffered lands at the
                // end, not through `Inserted`), so gate on `historical` rather
                // than the event variant — the join backlog must never mark a tab
                // unread. A non-message row (`None`: a live notice/error) is
                // genuinely new, so it signals.
                if msg.as_deref().is_none_or(|m| !m.historical) {
                    cx.emit(TabActivity);
                }
            }
            ChannelEvent::Inserted { index, msg } => {
                // History backfill sorts into place. While paused, an insert
                // inside the frozen prefix must apply (it shifts the rows the
                // frozen indices map to); one landing past it belongs to the
                // withheld tail instead.
                if !self.log_paused || *index <= self.list_state.item_count() {
                    let ix = (*index).min(self.list_state.item_count());
                    self.list_state.splice(ix..ix, 1);
                }
                self.note_new_row(msg.as_deref());
                if let Some(m) = msg.as_deref() {
                    self.arm_inline_preview(m, cx);
                }
            }
            ChannelEvent::RemovedFront => {
                // Applied even while paused: the frozen view tracks the shared
                // buffer's front so `rows[ix]` lookups can't skew, and the
                // trimmed rows are far above a bottom-anchored viewport.
                self.list_state.splice(0..1, 0);
            }
            ChannelEvent::EventAppended { seq } => {
                // Filter-passing events append to this view's events panel.
                // Looked up by stable sequence number: `None` means a burst
                // already trimmed it past MAX_EVENTS — nothing to show.
                let passes = self.channel.read(cx).event_at(*seq).is_some_and(|ev| {
                    self.config.event_kinds.enabled(ev.kind)
                        && !(self.config.collapse_gift_subs && ev.group.is_some())
                });
                if passes {
                    let ix = self.events_shown.len();
                    self.events_shown.push_back(*seq);
                    self.events_list_state.splice(ix..ix, 1);
                    cx.emit(TabActivity);
                }
            }
            ChannelEvent::EventsTrimmed => {
                let base = self.channel.read(cx).events_base;
                while self.events_shown.front().is_some_and(|&s| s < base) {
                    self.events_shown.pop_front();
                    self.events_list_state.splice(0..1, 0);
                }
            }
            // A row's content changed height in place (an AutoMod row resolved),
            // or channel state a render reads changed (strikes/cosmetics/pins).
            // Re-measure so a changed row's new height is picked up (rare event).
            ChannelEvent::Changed => {
                if self.log_paused {
                    // The reset would sync the withheld tail in and snap to the
                    // bottom — defer it to the unpause (the repaint below still
                    // shows in-place changes like strikes).
                    self.paused_needs_reset = true;
                } else {
                    self.list_state.reset(self.channel.read(cx).len());
                }
                // Pins may have changed: drop dismissals whose pin is gone
                // (unpinned/replaced/expired) so the restore chip only ever
                // represents — and restores — pins that are still active.
                if !self.dismissed_pins.is_empty() {
                    let active: Vec<(bks_core::Platform, String)> = self
                        .channel
                        .read(cx)
                        .pins
                        .iter()
                        .map(|(p, pin)| (*p, pin.message.id.clone()))
                        .collect();
                    self.dismissed_pins.retain(|key| active.contains(key));
                }
            }
            // A viewer count moved: only the status bar (chrome outside the
            // cached log) reads it — repaint without touching the log's
            // measurements or its cached paint. This fires every ~30s per live
            // platform, so it must stay this cheap.
            ChannelEvent::ViewersChanged => {
                cx.notify();
                return;
            }
            // Same deal for the mode bar above the composer: chrome outside the
            // cached log, repaint only.
            ChannelEvent::ChatModesChanged => {
                cx.notify();
                return;
            }
            // A channel went live: the `Row::Live` push already arrived via
            // `Appended`; here we just tell the app to flash this tab's chip.
            // Nothing in this view re-measures, so it's repaint-only like the
            // count/mode events.
            ChannelEvent::WentLive { platform } => {
                cx.emit(TabWentLive {
                    platform: *platform,
                });
                cx.notify();
                return;
            }
        }
        // An emote (re)load may have changed the picker's source. The picker is
        // per-view, so each view refilters its own when open; the picker reads
        // emotes live from the model, so refiltering on any change is correct
        // (skipped entirely while the picker is closed). Pin-expiry wakeups are
        // scheduled by the model itself (it owns the pins).
        if self.picker_open {
            self.refresh_picker_filter(cx);
        }
        self.flush_pending_mentions(cx);
        self.refresh_log(cx);
        cx.notify();
    }

    /// A newly-added message row (carried on its `Appended`/`Inserted` event —
    /// by delivery time a ring trim may have shifted it, so it must not be
    /// looked up by index): if it mentions us, tail the mentions panel + feed
    /// the all-tabs store. (The events panel follows the model's
    /// `EventAppended` instead — its list tails natively.)
    fn note_new_row(&mut self, msg: Option<&Message>) {
        let Some(msg) = msg else {
            return;
        };
        if self.mentions.matches(&msg.raw_text) {
            self.mentions_new = true;
            if !msg.historical {
                let sound = self.mentions.sound_for(&msg.raw_text);
                self.pending_mentions.push((msg.clone(), sound));
            }
        }
    }

    /// Reconnects this tab to `config`'s channels — used when a platform is *added
    /// to* or *removed from* a tab. Re-points this view at the shared model for the
    /// new channel set (attaching to an existing one if another tab already has it,
    /// else connecting fresh). Dropping the old `channel`/subscription releases this
    /// view's hold on the previous model, which tears down if it was the last view.
    pub(crate) fn reconnect(&mut self, config: TabConfig, cx: &mut Context<Self>) {
        self.config = config;
        let key = crate::channel_store::ChannelKey::new(
            &self.config.twitch_channel,
            &self.config.kick_channel,
            &self.config.youtube_channel,
        );
        let channel = crate::channel_store::get_or_create(
            key.clone(),
            &self.config.twitch_channel,
            &self.config.kick_channel,
            &self.config.youtube_channel,
            self.session.clone(),
            cx,
        );
        self.controller = channel.read(cx).controller.clone();
        // A fresh model means a fresh list — any hover-pause freeze is moot.
        self.log_paused = false;
        self.paused_needs_reset = false;
        self.list_state.reset(channel.read(cx).len());
        self._channel_sub = cx.subscribe(&channel, Self::on_channel_event);
        self.channel_key = key;
        self.channel = channel;
        // The native-emote sets are per-channel (the viewed channel's locked
        // set + its follower emotes) — refetch them for the new one.
        self.refresh_personal_emotes(cx);
        self.rebuild_events_shown(cx);
        self.refresh_log(cx);
        cx.notify();
    }

    /// Re-derives the events panel's filtered rows from the shared buffer —
    /// after a kind-filter change or a reconnect to a different channel.
    fn rebuild_events_shown(&mut self, cx: &mut Context<Self>) {
        self.events_shown = filtered_event_seqs(
            self.channel.read(cx),
            self.config.event_kinds,
            self.config.collapse_gift_subs,
        );
        self.events_list_state.reset(self.events_shown.len());
    }

    /// Updates the mention terms used to tint incoming messages. New messages
    /// match against this; already-shown rows keep the flag they were pushed with.
    pub(crate) fn set_mentions(&mut self, mentions: bks_core::MentionMatcher) {
        self.mentions = mentions;
    }

    /// Updates the ignore list. The log filters every row against this at render
    /// (a match renders as a height-0 row), so a live change re-heights already-
    /// buffered rows (full ↔ 0) — the virtualized list must re-measure, or gpui
    /// reuses the stale cached heights and leaves phantom gaps where a row is now
    /// hidden. The caller still repaints the log afterward.
    pub(crate) fn set_ignore(&mut self, ignore: bks_core::IgnoreList, cx: &mut Context<Self>) {
        self.ignore = ignore;
        // Reset unless paused: a paused view must not re-measure (that snaps the
        // frozen view to the live bottom) — unpause_log resets when it resumes.
        if self.log_paused {
            self.paused_needs_reset = true;
        } else {
            self.list_state.reset(self.channel.read(cx).len());
        }
    }

    /// Updates the suppress list — matching messages render dimmed instead of
    /// hidden. Unlike ignore this is purely a render concern, so a change
    /// re-dims already-buffered rows too (the caller repaints the log).
    pub(crate) fn set_suppress(&mut self, suppress: bks_core::SuppressList) {
        self.suppress = suppress;
    }

    /// Switches the mentions panel between this tab's own mentions and the
    /// shared all-tabs feed (the settings checkbox pushes the app-side copy here).
    pub(crate) fn set_mentions_all(&mut self, all: bool, cx: &mut Context<Self>) {
        self.config.mentions_all_tabs = all;
        cx.notify();
    }

    /// Flushes mentions matched this burst into the shared store, tagged with
    /// this tab's id and each message's channel (its "#channel" jump tag).
    fn flush_pending_mentions(&mut self, cx: &mut Context<Self>) {
        if self.pending_mentions.is_empty() {
            return;
        }
        let view = cx.entity().downgrade();
        let tab_id = self.tab_id;
        let fallback = self.config.display_name();
        let msgs = std::mem::take(&mut self.pending_mentions);
        self.mention_store.update(cx, |store, cx| {
            for (msg, sound) in msgs {
                let source = if msg.channel.is_empty() {
                    fallback.clone()
                } else {
                    msg.channel.clone()
                };
                store.push(
                    crate::mentions::MentionEntry {
                        tab_id,
                        source: SharedString::from(source),
                        view: view.clone(),
                        msg: Box::new(msg),
                        sound,
                    },
                    cx,
                );
            }
        });
    }

    /// The tab's controller, so the account settings UI can act on its feed.
    pub(crate) fn controller(&self) -> &Controller {
        &self.controller
    }

    /// The latest live status for `platform`, for the tab strip's hover tooltip.
    /// `None` until the first poll for that platform lands.
    pub(crate) fn live_status(&self, platform: bks_core::Platform, cx: &App) -> Option<LiveInfo> {
        self.channel.read(cx).live_status.get(&platform).cloned()
    }

    /// The latest concurrent viewer count for `platform` (status bar + tooltip),
    /// `None` until a count lands or while offline.
    pub(crate) fn viewer_count(&self, platform: bks_core::Platform, cx: &App) -> Option<u64> {
        self.channel.read(cx).viewer_counts.get(&platform).copied()
    }

    /// Dirties the cached log child view so its rows re-render. Required after
    /// anything that changes what the log shows (new rows, fades, font size,
    /// palette, panel config) — a `.cached()` view reuses its old paint otherwise.
    pub(crate) fn refresh_log(&self, cx: &mut Context<Self>) {
        self.log_view.update(cx, |_, cx| cx.notify());
    }

    /// The log wrapper's hover tracking (the pause-on-hover feature). Entering
    /// engages the pause when eligible; leaving resumes — back to the newest
    /// message if the view was still following the tail, staying put if the
    /// user scrolled up themselves in the meantime.
    fn set_log_hovered(&mut self, hovered: bool, cx: &mut Context<Self>) {
        if self.log_hovered == hovered {
            return;
        }
        self.log_hovered = hovered;
        if hovered {
            self.maybe_pause_log(cx);
        } else if self.log_paused {
            self.unpause_log(cx);
        }
    }

    /// Engages hover-pause when the setting is on, the pointer is over the log,
    /// and the view is following the tail (a scrolled-up view doesn't move on
    /// appends, so there's nothing to pause). A no-op otherwise; called on
    /// hover-enter and again per appended row, so scrolling back to the bottom
    /// mid-hover pauses too.
    fn maybe_pause_log(&mut self, cx: &mut Context<Self>) {
        if !self.log_paused
            && self.log_hovered
            && crate::settings::pause_chat_on_hover()
            && self.list_state.is_following_tail()
        {
            self.log_paused = true;
            self.refresh_log(cx); // show the "paused" pill
            cx.notify();
        }
    }

    /// Ends hover-pause: splices the withheld tail into the list (the follow
    /// mode does the rest — still tail-following snaps to the newest row, a
    /// mid-pause manual scroll keeps its place), or runs the re-measure a
    /// `Changed` deferred.
    fn unpause_log(&mut self, cx: &mut Context<Self>) {
        self.log_paused = false;
        let len = self.channel.read(cx).len();
        if self.paused_needs_reset {
            self.paused_needs_reset = false;
            self.list_state.reset(len);
        } else {
            let shown = self.list_state.item_count();
            if len > shown {
                self.list_state.splice(shown..shown, len - shown);
            } else if len < shown {
                // Shouldn't happen (trims apply while paused) — resync anyway.
                self.list_state.reset(len);
            }
        }
        self.refresh_log(cx);
        cx.notify();
    }

    /// Applies a new chat font size (called when the preference changes). Row
    /// heights change, so the list must re-measure (`reset` keeps the count).
    pub(crate) fn set_font_size(&mut self, font_size: f32, cx: &mut Context<Self>) {
        self.font_size = font_size;
        self.remeasure(cx);
    }

    /// Re-measures every row and repaints the log — for preference changes that
    /// change row heights without changing the rows (font family/size).
    pub(crate) fn remeasure(&mut self, cx: &mut Context<Self>) {
        // The reset below syncs any hover-pause-withheld tail in, so the
        // deferred re-measure (if one was pending) just happened.
        self.paused_needs_reset = false;
        self.list_state.reset(self.channel.read(cx).len());
        self.events_list_state.reset(self.events_shown.len());
        self.refresh_log(cx);
        cx.notify();
    }

    /// Replaces this tab's panel layout live (settings checkbox toggles push the
    /// app-side copy here), re-measuring the log since its width may change.
    pub(crate) fn set_layout(&mut self, layout: tabs::Layout, cx: &mut Context<Self>) {
        self.config.layout = layout;
        self.layout_changed(cx);
    }

    /// Updates the events panel's filters (kind checklist, "events only",
    /// hide-sub-messages, gift collapsing) live.
    pub(crate) fn set_events_filter(
        &mut self,
        kinds: tabs::EventFilter,
        events_only: bool,
        hide_sub_messages: bool,
        collapse_gift_subs: bool,
        cx: &mut Context<Self>,
    ) {
        self.config.event_kinds = kinds;
        self.config.events_only = events_only;
        self.config.hide_sub_messages = hide_sub_messages;
        self.config.collapse_gift_subs = collapse_gift_subs;
        self.rebuild_events_shown(cx);
        // "Events only" adds/removes rows from the main log: re-measure.
        self.layout_changed(cx);
    }

    /// This tab's live layout (mutated here by header-arrow moves and divider
    /// drags), borrowed so the app can compare against the persisted copy each
    /// render without a per-frame clone (it clones only on an actual change).
    pub(crate) fn layout(&self) -> &tabs::Layout {
        &self.config.layout
    }

    /// After any layout mutation: the log's size may have changed, so its rows
    /// re-wrap — re-measure and repaint.
    fn layout_changed(&mut self, cx: &mut Context<Self>) {
        self.remeasure(cx);
    }

    /// Moves `kind` one step in the layout grid (a panel header's arrow button).
    fn move_panel(&mut self, kind: tabs::PanelKind, dir: tabs::MoveDir, cx: &mut Context<Self>) {
        self.config.layout.move_panel(kind, dir);
        self.layout_changed(cx);
    }

    /// Applies a column-divider drag: distributes `delta` (a fraction of the
    /// grid width) between the two columns adjacent to the divider, keeping
    /// their pair sum and clamping so neither collapses.
    fn resize_columns(
        &mut self,
        left: usize,
        start: (f32, f32),
        delta: f32,
        cx: &mut Context<Self>,
    ) {
        let cols = &mut self.config.layout.columns;
        if left + 1 >= cols.len() {
            return;
        }
        let pair = start.0 + start.1;
        let min = tabs::MIN_SHARE.min(pair / 2.0);
        let new_left = (start.0 + delta).clamp(min, pair - min);
        cols[left].share = new_left;
        cols[left + 1].share = pair - new_left;
        self.layout_changed(cx);
    }

    /// Applies a row-divider drag within column `col` (same scheme as
    /// [`resize_columns`], vertically).
    fn resize_rows(
        &mut self,
        col: usize,
        above: usize,
        start: (f32, f32),
        delta: f32,
        cx: &mut Context<Self>,
    ) {
        let Some(column) = self.config.layout.columns.get_mut(col) else {
            return;
        };
        if above + 1 >= column.panels.len() {
            return;
        }
        let pair = start.0 + start.1;
        let min = tabs::MIN_SHARE.min(pair / 2.0);
        let new_top = (start.0 + delta).clamp(min, pair - min);
        column.panels[above].share = new_top;
        column.panels[above + 1].share = pair - new_top;
        self.layout_changed(cx);
    }

    /// On Enter: hand the text to the controller (which parses `/commands` vs
    /// plain chat), then clear the box. On any text change, recompute the
    /// autocomplete popup from the word at the cursor.
    fn on_input_event(
        &mut self,
        state: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputEvent::Change = event {
            // A manual edit ends history browsing (the box is the user's draft
            // again). A recall's own `set_value` also fires Change; the flag it
            // sets tells us to keep browsing instead of resetting. Consume it so
            // the *next* Change (real typing) resets as normal.
            if std::mem::take(&mut self.history_setting) {
                // programmatic recall — keep `history_index`
            } else {
                self.history_index = None;
            }
            self.update_input_popup(cx);
        }
        if let InputEvent::PressEnter { .. } = event {
            let text = state.read(cx).value().to_string();
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                // UI commands (viewer list, usercard, pin) act on view state and
                // never reach the controller; everything else does. A pending
                // reply threads a plain line and is then cleared; a `/command`
                // never replies (the controller ignores the target).
                if !self.handle_ui_command(trimmed, cx) {
                    self.controller.handle_input(&text, self.replying_to.take());
                    self.scroll_to_newest.reply_chain = false;
                }
                self.record_sent(&text);
            }
            self.input.update(cx, |this, cx| {
                this.set_value("", window, cx);
            });
            self.completion = None;
            self.popup = None;
            self.history_index = None;
            self.history_draft.clear();
            cx.notify();
        }
    }

    /// Records a just-sent line at the front of this tab's input history (newest
    /// first), skipping an immediate duplicate of the last send and capping at
    /// [`SENT_HISTORY_MAX`].
    fn record_sent(&mut self, text: &str) {
        if self.sent_history.first().map(String::as_str) == Some(text) {
            return;
        }
        self.sent_history.insert(0, text.to_string());
        self.sent_history.truncate(SENT_HISTORY_MAX);
    }

    /// Commands that act on this view rather than the chat APIs — the viewer
    /// list, usercards, and unpin (which needs the pins map for the active
    /// pin's id). Returns whether `line` was one of them (and was consumed);
    /// everything else goes to the controller. The Both-mode gate matches the
    /// controller's: commands act on ONE chat.
    fn handle_ui_command(&mut self, line: &str, cx: &mut Context<Self>) -> bool {
        let Some(rest) = line.strip_prefix('/') else {
            return false;
        };
        let mut parts = rest.split_whitespace();
        let cmd = parts.next().unwrap_or("").to_lowercase();
        let args: Vec<&str> = parts.collect();
        let both = self.controller.send_target() == controller::SendTarget::Both;
        let both_notice = "commands don't work while sending to both platforms — \
                           switch the send target to one";
        match cmd.as_str() {
            "chatters" | "viewers" => {
                self.open_viewer_list(cx);
                true
            }
            "usercard" | "user" => {
                if both {
                    self.controller.notice(both_notice);
                } else {
                    match args.first() {
                        Some(name) => self.open_usercard_by_name(name, cx),
                        None => self.controller.notice("usage: /usercard <user>"),
                    }
                }
                true
            }
            "unpin" => {
                if both {
                    self.controller.notice(both_notice);
                    return true;
                }
                let Some(&platform) = self.target_platforms().first() else {
                    return true;
                };
                let pinned = self
                    .channel
                    .read(cx)
                    .pins
                    .get(&platform)
                    .map(|pin| pin.message.id.clone());
                match pinned {
                    Some(id) if platform == bks_core::Platform::Twitch => {
                        self.controller.unpin_twitch(id)
                    }
                    Some(_) => self
                        .controller
                        .notice(controller::Controller::KICK_UNSUPPORTED),
                    None => self
                        .controller
                        .notice(format!("no pinned message on {}", platform.label())),
                }
                true
            }
            _ => false,
        }
    }

    /// Opens the usercard for `name` on the current target platform
    /// (`/usercard <user>`).
    fn open_usercard_by_name(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(&platform) = self.target_platforms().first() else {
            return;
        };
        self.open_usercard_named(name, platform, cx);
    }

    /// Opens the usercard for `name` on `platform` (`/usercard`, mention
    /// clicks). A message the chatter sent this session gives the real
    /// login/id/color/badges; otherwise a bare card opens and the async stats
    /// fetch fills in what it can — or shows it's not a real user.
    pub(crate) fn open_usercard_named(
        &mut self,
        name: &str,
        platform: bks_core::Platform,
        cx: &mut Context<Self>,
    ) {
        let name = name.trim_start_matches('@');
        let seen = self
            .channel
            .read(cx)
            .rows
            .iter()
            .rev()
            .find_map(|row| match row {
                Row::Message { msg }
                    if msg.platform == platform
                        && (msg.author.login.eq_ignore_ascii_case(name)
                            || msg.author.display_name.eq_ignore_ascii_case(name)) =>
                {
                    Some(msg.id.clone())
                }
                _ => None,
            });
        match seen {
            Some(id) => self.open_usercard(&id, cx),
            None => {
                let card = usercard::UserCard::new(
                    name.to_lowercase(),
                    name.to_string(),
                    String::new(),
                    platform,
                    None,
                );
                self.show_usercard(card, cx);
            }
        }
    }

    /// Recalls a previous/next sent line into the input while browsing history
    /// with Up (`delta = -1`, older) / Down (`delta = +1`, newer). Returns whether
    /// it consumed the key: only when there's history and the move stays in range
    /// (Up past the oldest is ignored; Down past the newest restores the stashed
    /// draft). Stashes the live draft on entry so it isn't lost.
    fn history_recall(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.sent_history.is_empty() {
            return false;
        }
        // Current position: None (draft) is "one below newest" = -1.
        let cur = self.history_index.map(|i| i as isize).unwrap_or(-1);
        if self.history_index.is_none() {
            // Entering history browse: stash whatever's in the box as the draft.
            self.history_draft = self.input.read(cx).value().to_string();
        }
        // Up moves toward older (higher index), Down toward newer (lower).
        let next = cur - delta;
        if next < -1 || next >= self.sent_history.len() as isize {
            return false; // past the oldest (Up) — leave the input untouched.
        }
        let value = if next < 0 {
            self.history_index = None;
            self.history_draft.clone()
        } else {
            self.history_index = Some(next as usize);
            self.sent_history[next as usize].clone()
        };
        // `set_value` fires a deferred Change; the flag tells that handler this was
        // a programmatic recall so it keeps `history_index` (see `on_input_event`).
        // Only arm it when the text actually changes — a no-op `set_value` emits no
        // Change, which would otherwise leave the flag armed for the next keystroke.
        let changed = self.input.read(cx).value() != value.as_str();
        self.history_setting = changed;
        self.input.update(cx, |state, cx| {
            state.set_value(&value, window, cx);
        });
        self.popup = None;
        cx.notify();
        true
    }

    /// A Down press outside history browsing clears a typed draft (Chatterino-
    /// style: Down = discard what I was writing). Returns whether it consumed
    /// the key (there was text to clear).
    fn clear_typed_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.history_index.is_some() || self.input.read(cx).value().is_empty() {
            return false;
        }
        self.input.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        cx.notify();
        true
    }

    /// Starts a reply to the message with `msg_id`: stashes its reply identity so
    /// the next sent line threads under it, focuses the input, and shows the
    /// "replying to" bar. No-op if the row is gone.
    fn start_reply(&mut self, msg_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(msg) = self.message_by_id(msg_id, cx) else {
            return;
        };
        self.replying_to = Some(controller::ReplyTo {
            platform: msg.platform,
            message_id: msg.id.clone(),
            parent: bks_core::ReplyParent {
                author: msg.author.display_name.clone(),
                text: msg.raw_text.clone(),
                parent_id: Some(msg.id.clone()),
                thread_root_id: Some(msg.thread_id().to_string()),
            },
            parent_elements: msg.elements.clone(),
        });
        // If the composer shows the full thread chain, open it at the newest
        // message (chat order) with older ones scrollable above. A *fresh* scroll
        // handle each open starts with zero bounds, so `open_at_bottom`'s
        // laid-out check isn't fooled by a previous open's stale bounds.
        self.reply_chain_scroll = gpui::ScrollHandle::new();
        self.scroll_to_newest.reply_chain = true;
        self.input.update(cx, |this, cx| this.focus(window, cx));
        cx.notify();
    }

    /// Opens the thread panel seeded from `msg_id` (a reply's "replying to" line
    /// was clicked at `anchor`, window coords). The chain itself is rebuilt from
    /// the live buffer each render, so the panel keeps up as new replies land.
    fn open_thread_panel(&mut self, msg_id: &str, anchor: Point<Pixels>, cx: &mut Context<Self>) {
        self.thread_panel = Some(ThreadPanel {
            seed_id: msg_id.to_string(),
            anchor,
        });
        // Open at the newest message, older ones scrollable above. A *fresh*
        // scroll handle each open starts with zero bounds, so `open_at_bottom`'s
        // laid-out check isn't fooled by a previous open's stale bounds.
        self.thread_scroll = gpui::ScrollHandle::new();
        self.scroll_to_newest.panel = true;
        cx.notify();
    }

    /// Closes the thread panel.
    fn close_thread_panel(&mut self, cx: &mut Context<Self>) {
        self.thread_panel = None;
        cx.notify();
    }

    /// One-shot "open at the newest row": while the `target`'s flag is set, snap
    /// `scroll` to the bottom. `scroll` is a *fresh* handle each open (reset in
    /// `open_thread_panel`/`start_reply`), so its `bounds()` start zero and only
    /// become non-zero once the list has actually laid out — the signal this uses
    /// to know the `scroll_to_bottom` has real content to act on before clearing
    /// the flag. Until then it keeps the flag and schedules another frame. Called
    /// from render; the view rebuilds each frame while the view is open.
    fn open_at_bottom(
        &mut self,
        target: ScrollTarget,
        scroll: &gpui::ScrollHandle,
        cx: &mut Context<Self>,
    ) {
        let flag = match target {
            ScrollTarget::Panel => &mut self.scroll_to_newest.panel,
            ScrollTarget::ReplyChain => &mut self.scroll_to_newest.reply_chain,
        };
        if !*flag {
            return;
        }
        scroll.scroll_to_bottom();
        // A just-built list reports zero bounds for a frame (not painted yet), so
        // `scroll_to_bottom` has nothing to act on. Once it's been laid out
        // (non-zero height) the scroll has taken — whether the list overflows or
        // fits fully — so clear the flag; until then keep it and request another
        // frame. This avoids spinning on a short, non-overflowing thread (whose
        // `max_offset` stays 0).
        if scroll.bounds().size.height > px(0.) {
            *flag = false;
        } else {
            cx.notify();
        }
    }

    /// Right-click-to-tag: appends `@name ` to the composer and switches the send
    /// target to the chatter's platform so the tag lands in the right chat. The
    /// name is inserted with one leading space when the box already has content
    /// (so "hi" + tag = "hi @name ") and always a trailing space so the next word
    /// is separate. Tagging a second person on the same platform leaves the target
    /// unchanged (the switch is a no-op).
    ///
    /// UX note on cross-platform tagging: in a merged Twitch+Kick feed, tagging a
    /// Twitch user then a Kick user switches the target to Kick (the last-tagged
    /// platform wins). That's intentional — a tag is only meaningful on the
    /// platform the message goes to, so the target follows the most recent tag;
    /// the send-target toggle stays visible if the user wants to correct it.
    fn tag_user(
        &mut self,
        display_name: &str,
        platform: bks_core::Platform,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Switch the composer to the tagged chatter's platform (a no-op notice-
        // wise when already there or the platform isn't sendable here).
        self.controller.set_send_target(platform);
        let current = self.input.read(cx).value().to_string();
        let sep = if current.is_empty() || current.ends_with(' ') {
            ""
        } else {
            " "
        };
        let next = format!("{current}{sep}@{display_name} ");
        self.input.update(cx, |state, cx| {
            state.set_value(&next, window, cx);
            state.focus(window, cx);
        });
        // Repaint so the send-target toggle reflects a possibly-switched platform.
        cx.notify();
    }

    /// Runs a moderation button's command against message `msg_id`: substitutes
    /// `{user}` (the author's login) and `{msg-id}` in the template and
    /// dispatches the line at the *row's* platform — a button on a Kick message
    /// moderates Kick no matter where the composer is sending.
    /// The placeholders are optional for known commands: a template without any
    /// gets the row's target inserted right after the command name, per the
    /// registry's usage shape (`commands::implicit_target`) — "/timeout 600
    /// spam" acts on the message's author, a bare "/delete" on the message.
    fn run_mod_button(&mut self, msg_id: &str, command: &str, cx: &mut Context<Self>) {
        let Some(msg) = self.message_by_id(msg_id, cx) else {
            return;
        };
        let mut template = command.to_string();
        if !command.contains("{user}") && !command.contains("{msg-id}") {
            let target = command
                .strip_prefix('/')
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(commands::implicit_target);
            if let Some(target) = target {
                let placeholder = match target {
                    commands::ImplicitTarget::User => "{user}",
                    commands::ImplicitTarget::MessageId => "{msg-id}",
                };
                let mut parts = command.splitn(2, char::is_whitespace);
                let head = parts.next().unwrap_or_default();
                template = match parts.next() {
                    Some(rest) => format!("{head} {placeholder} {rest}"),
                    None => format!("{head} {placeholder}"),
                };
            }
        }
        let line = template
            .replace("{user}", &msg.author.login)
            .replace("{msg-id}", &msg.id);
        self.controller.handle_input_at(&line, msg.platform);
    }

    /// Runs a custom mod button from the usercard against `login` on `platform`.
    /// Like [`run_mod_button`](Self::run_mod_button) but there's no message —
    /// the card targets a user — so only `{user}`/`<user>`-target templates are
    /// offered (filtered by `commands::targets_user`); the login is substituted
    /// for `{user}` and injected as the implicit target when no placeholder is
    /// typed.
    fn run_usercard_mod_button(&self, command: &str, login: &str, platform: bks_core::Platform) {
        let mut template = command.to_string();
        if !command.contains("{user}") {
            let is_user_target = command
                .strip_prefix('/')
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(commands::implicit_target)
                == Some(commands::ImplicitTarget::User);
            if is_user_target {
                let mut parts = command.splitn(2, char::is_whitespace);
                let head = parts.next().unwrap_or_default();
                template = match parts.next() {
                    Some(rest) => format!("{head} {{user}} {rest}"),
                    None => format!("{head} {{user}}"),
                };
            }
        }
        let line = template.replace("{user}", login);
        self.controller.handle_input_at(&line, platform);
    }

    /// Opens the pin confirmation dialog for message `msg_id`: the message is
    /// shown as it will appear pinned, with duration chips (Twitch-web style —
    /// timed, or "Until stream ends" where the platform supports it), and only
    /// Pin actually pins it.
    fn confirm_pin(&mut self, msg_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(msg) = self.message_by_id(msg_id, cx) else {
            return;
        };
        self.pin_duration_choice = Some(controller::Controller::PIN_DURATION_SECS);
        let entity = cx.entity();
        let font_size = self.font_size;
        let id = msg.id.clone();
        let platform = msg.platform;
        let label = platform.label();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let entity = entity.clone();
            let id = id.clone();
            let body = v_flex()
                .gap_2()
                .child(pin_dialog_preview(&msg, font_size))
                .child(pin_duration_chips(&entity, platform, cx));
            alert
                .title(format!("Pin to {label} chat?"))
                .description(body)
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Pin")
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    entity.update(cx, |this, cx| {
                        let duration = this.pin_duration_choice;
                        this.pin_message(&id, duration, cx);
                    });
                    true
                })
        });
    }

    /// Opens the unpin confirmation dialog for `platform`'s active pin: shows
    /// the pinned message, and only Unpin actually removes it.
    fn confirm_unpin(
        &self,
        platform: bks_core::Platform,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pin) = self.channel.read(cx).pins.get(&platform) else {
            return;
        };
        let msg = pin.message.clone();
        let entity = cx.entity();
        let font_size = self.font_size;
        let id = msg.id.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let entity = entity.clone();
            let id = id.clone();
            alert
                .title(format!("Unpin from {} chat?", platform.label()))
                .description(pin_dialog_preview(&msg, font_size))
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Unpin")
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    entity.update(cx, |this, cx| {
                        // The pin can be replaced while the dialog is open (a
                        // new PinMessage overwrites the platform's slot): only
                        // unpin if the previewed message is still the active
                        // pin, else the confirm would remove a pin the user
                        // never saw.
                        let still_active = this
                            .channel
                            .read(cx)
                            .pins
                            .get(&platform)
                            .is_some_and(|pin| pin.message.id == id);
                        if !still_active {
                            return;
                        }
                        if platform == bks_core::Platform::Twitch {
                            this.controller.unpin_twitch(id.clone());
                        }
                    });
                    true
                })
        });
    }

    /// Pins `msg_id` for `duration` seconds (`None` = until the stream ends).
    /// Twitch-only, by message id (Helix) — [`can_pin`](Self::can_pin) keeps the
    /// 📌 button off other platforms' rows.
    fn pin_message(&self, msg_id: &str, duration: Option<u32>, cx: &App) {
        let Some(msg) = self.message_by_id(msg_id, cx) else {
            return;
        };
        if msg.platform == bks_core::Platform::Twitch {
            self.controller.pin_twitch(msg.id.clone(), duration);
        }
    }

    /// Whether the logged-in user can pin/unpin on `platform` (gates the per-row
    /// 📌 button and the banner's Unpin). Twitch-only: mod status comes from IRC
    /// USERSTATE and pins go through Helix. Kick *receiving* pins is anonymous
    /// and stays, but pinning/unpinning only exists on Kick's site API, which
    /// rejects third-party OAuth tokens ([`Controller::KICK_UNSUPPORTED`]).
    fn can_pin(&self, platform: bks_core::Platform, cx: &App) -> bool {
        platform == bks_core::Platform::Twitch && self.channel.read(cx).can_moderate(platform)
    }

    /// The live-status bar above the log: one segment per platform of this tab
    /// that is currently live — platform icon, channel name, a live dot, and the
    /// latest concurrent viewer count ("LIVE" until a count lands), plus a Total
    /// segment when 2+ platforms have counts. `None` (no bar at all) when nothing
    /// is live — the common case, which allocates nothing (like the pin banners'
    /// empty case). A fresh count doesn't snap: the shown number counts up/down
    /// to it ([`ViewerAnim`]), repainting on a coalesced [`VIEWER_ANIM_TICK`]
    /// timer until it settles.
    fn render_status_bar(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !crate::settings::show_status_bar() {
            // Drop the animations too, so re-enabling later doesn't roll up
            // from a long-stale number.
            self.viewer_anims.clear();
            return None;
        }
        let now = std::time::Instant::now();
        // Copy the per-platform facts out first — the model borrow must end
        // before the anim map below is touched.
        let model = self.channel.read(cx);
        let facts = [
            bks_core::Platform::Twitch,
            bks_core::Platform::Kick,
            bks_core::Platform::YouTube,
        ]
        .map(|platform| {
            (
                platform,
                model.live_status.get(&platform).is_some_and(|s| s.live),
                model.viewer_counts.get(&platform).copied(),
            )
        });

        // `Vec::new` doesn't allocate — nothing does until a live platform
        // actually pushes a segment.
        let mut segments: Vec<(bks_core::Platform, SharedString, Option<u64>)> = Vec::new();
        for (platform, live, target) in facts {
            let name = match platform {
                bks_core::Platform::Kick => self.config.kick_channel.trim(),
                bks_core::Platform::YouTube => self.config.youtube_channel.trim(),
                _ => self.config.twitch_channel.trim(),
            };
            if name.is_empty() || !live {
                // Not shown: drop any animation so a later count starts fresh
                // instead of rolling up from the stale value.
                self.viewer_anims.remove(&platform);
                continue;
            }
            let displayed = match target {
                // Live but no count yet: a bare "LIVE" segment.
                None => {
                    self.viewer_anims.remove(&platform);
                    None
                }
                Some(target) => Some(match self.viewer_anims.get_mut(&platform) {
                    Some(anim) => {
                        if anim.to != target {
                            // Retarget from the value currently on screen.
                            *anim = ViewerAnim {
                                from: anim.value_at(now),
                                to: target,
                                started: now,
                            };
                        }
                        anim.value_at(now)
                    }
                    // First count for this platform: show it as-is (nothing to
                    // roll from; from == to reads as already done).
                    None => {
                        self.viewer_anims.insert(
                            platform,
                            ViewerAnim {
                                from: target,
                                to: target,
                                started: now,
                            },
                        );
                        target
                    }
                }),
            };
            segments.push((platform, SharedString::from(name.to_string()), displayed));
        }
        if self.viewer_anims.values().any(|a| !a.done(now)) {
            self.schedule_viewer_anim_tick(cx);
        }
        if segments.is_empty() {
            return None;
        }
        // With two or more counted platforms, a combined total closes the bar.
        // Summing the *displayed* (animating) values keeps it rolling in sync.
        let counted = segments.iter().filter(|(_, _, c)| c.is_some()).count();
        let total =
            (counted >= 2).then(|| segments.iter().filter_map(|(_, _, c)| *c).sum::<u64>());
        Some(
            h_flex()
                .w_full()
                .px_3()
                .py_1()
                .gap_4()
                .flex_wrap()
                .items_center()
                // The chat surface tone, not the chrome tone: the tab strip sits
                // directly above, and on the same color the two fused into one
                // band. On the content tone the strip ends where the bar starts
                // (and the active tab connects into it); the hairline below
                // separates it from the pins/log.
                .bg(gpui::rgb(render::chat_bg()))
                .border_b_1()
                .border_color(cx.theme().border)
                .text_size(px(self.font_size * 0.9))
                .children(segments.into_iter().map(|(platform, channel, viewers)| {
                    let readout = match viewers {
                        Some(n) => format!("{} viewers", bks_core::format_count(n)),
                        None => "LIVE".to_string(),
                    };
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .child(crate::platform_icon(platform, 16.))
                        .child(div().font_weight(FontWeight::BOLD).child(channel))
                        // A real dot (not the ● glyph, whose size and vertical
                        // position drift with the chat font).
                        .child(
                            div()
                                .size(px(7.))
                                .rounded_full()
                                .bg(gpui::rgb(render::live_text())),
                        )
                        .child(
                            div()
                                .text_color(cx.theme().muted_foreground)
                                .child(SharedString::from(readout)),
                        )
                }))
                .children(total.map(|total| {
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .child(
                            div()
                                .font_weight(FontWeight::BOLD)
                                .child(SharedString::from("Total")),
                        )
                        .child(
                            div()
                                .text_color(cx.theme().muted_foreground)
                                .child(SharedString::from(format!(
                                    "{} viewers",
                                    bks_core::format_count(total)
                                ))),
                        )
                }))
                // The viewer-list button rides the bar's right edge (a
                // while-live, mods-only feature — it doesn't earn a spot in
                // the composer).
                .child(div().flex_1())
                .children(self.viewerlist_button(cx))
                .into_any_element(),
        )
    }

    /// The chat-mode bar directly above the composer's input row: one group per
    /// platform with any chat restriction active — platform icon + a chip per
    /// active mode ("Followers-only (10m)", "Sub-only", "Emote-only",
    /// "Slow (5s)", "Unique"). `None` (no bar, nothing allocated) when no
    /// platform restricts anything — the common case. Platform-agnostic by
    /// construction: it renders whatever `ChannelModel::chat_modes` holds, so a
    /// connector that starts emitting `ChatEvent::ChatModes` (Kick later) shows
    /// up here with zero UI changes.
    fn render_mode_bar(&self, on_top: bool, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let model = self.channel.read(cx);
        if model.chat_modes.is_empty() {
            return None;
        }
        let mut groups: Vec<(bks_core::Platform, Vec<String>)> = Vec::new();
        for platform in [
            bks_core::Platform::Twitch,
            bks_core::Platform::Kick,
            bks_core::Platform::YouTube,
        ] {
            let Some(modes) = model.chat_modes.get(&platform) else {
                continue;
            };
            let mut chips = Vec::new();
            if let Some(min) = modes.followers_only {
                chips.push(if min.is_zero() {
                    "Followers-only".to_string()
                } else {
                    format!(
                        "Followers-only ({})",
                        bks_core::format_duration(min.as_secs())
                    )
                });
            }
            if modes.subscribers_only {
                chips.push("Sub-only".to_string());
            }
            if modes.emote_only {
                chips.push("Emote-only".to_string());
            }
            if let Some(gap) = modes.slow {
                chips.push(format!("Slow ({})", bks_core::format_duration(gap.as_secs())));
            }
            if modes.unique {
                chips.push("Unique".to_string());
            }
            if !chips.is_empty() {
                groups.push((platform, chips));
            }
        }
        if groups.is_empty() {
            return None;
        }
        let chip_text = px(self.font_size * 0.85);
        let mut bar = h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_3()
            .flex_wrap()
            .items_center()
            // Same chrome tone as the input bar; a hairline separates it from the
            // log — below the bar when it sits on top, above it when at bottom.
            .bg(gpui::rgb(render::tab_bar_bg()))
            .border_color(cx.theme().border);
        bar = if on_top {
            bar.border_b_1()
        } else {
            bar.border_t_1()
        };
        Some(
            bar
                .children(groups.into_iter().map(|(platform, chips)| {
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .child(crate::platform_icon(platform, 14.))
                        .children(chips.into_iter().map(|label| {
                            div()
                                .px_1p5()
                                .py_0p5()
                                .rounded(px(4.))
                                .bg(cx.theme().muted)
                                .text_color(cx.theme().muted_foreground)
                                .text_size(chip_text)
                                .child(SharedString::from(label))
                        }))
                }))
                .into_any_element(),
        )
    }

    /// Jumps the log to a message by identity (a clicked mention row): scroll it
    /// into view (centered), then flash it briefly so the eye can find it. When
    /// the message has aged out of the ring buffer, shows a transient "no longer
    /// in chat history" note instead and leaves the log where it is. The tab is
    /// already active (the app selects it before calling this).
    pub(crate) fn jump_to_message(
        &mut self,
        platform: bks_core::Platform,
        msg_id: &str,
        cx: &mut Context<Self>,
    ) {
        let index = self.channel.read(cx).rows.iter().position(|row| {
            matches!(row, Row::Message { msg } if msg.platform == platform && msg.id == msg_id)
        });
        match index {
            Some(ix) => {
                // The log lives in `FollowMode::Tail`: while it's actively
                // following, every layout forces the scroll to the end, which
                // would immediately undo the reveal below. A negative `scroll_by`
                // disengages tail-follow (like a user wheel-up) *without* leaving
                // Tail mode — so it still re-engages when scrolled back to the
                // bottom and the "Jump to latest" pill appears. Then reveal the
                // row centered so surrounding context is visible.
                if self.list_state.is_following_tail() {
                    self.list_state.scroll_by(px(-1.));
                }
                self.list_state.scroll_to_reveal_item(ix);
                self.flash = Some(FlashTarget {
                    platform,
                    msg_id: msg_id.to_string(),
                    started_at: std::time::Instant::now(),
                });
                self.jump_note = None;
                self.schedule_flash_tick(cx);
                self.refresh_log(cx);
                cx.notify();
            }
            None => {
                self.jump_note = Some((
                    SharedString::from("That message is no longer in chat history"),
                    std::time::Instant::now(),
                ));
                self.schedule_flash_tick(cx);
                cx.notify();
            }
        }
    }

    /// The current flash strength for a message row, if it's the flashed target
    /// and still fading; `None` otherwise. Read by the log render to tint the row.
    pub(crate) fn flash_strength_for(&self, platform: bks_core::Platform, msg_id: &str) -> Option<f32> {
        let flash = self.flash.as_ref()?;
        if flash.platform != platform || flash.msg_id != msg_id {
            return None;
        }
        flash.strength()
    }

    /// The transient jump note (aged-out message), while it's still showing.
    pub(crate) fn jump_note(&self) -> Option<SharedString> {
        let (text, at) = self.jump_note.as_ref()?;
        (at.elapsed() < JUMP_NOTE_DURATION).then(|| text.clone())
    }

    /// Schedules a repaint tick while a flash fade or jump note is active —
    /// coalesced to one pending timer per view (like the viewer anim), clearing
    /// the target/note once it has fully faded.
    fn schedule_flash_tick(&mut self, cx: &mut Context<Self>) {
        if self.flash_tick_pending {
            return;
        }
        self.flash_tick_pending = true;
        cx.spawn(async move |view, cx| {
            cx.background_executor().timer(FLASH_TICK).await;
            let _ = view.update(cx, |view, cx| {
                view.flash_tick_pending = false;
                let flash_done = view.flash.as_ref().is_none_or(|f| f.strength().is_none());
                if flash_done {
                    view.flash = None;
                }
                let note_done = view
                    .jump_note
                    .as_ref()
                    .is_none_or(|(_, at)| at.elapsed() >= JUMP_NOTE_DURATION);
                if note_done {
                    view.jump_note = None;
                }
                // Re-arm while anything is still animating; otherwise let it settle.
                if !flash_done || !note_done {
                    view.schedule_flash_tick(cx);
                }
                view.refresh_log(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Schedules one repaint tick for the viewer-count animation — coalesced: at
    /// most one pending timer per view, re-armed by the next render while any
    /// animation is still running.
    fn schedule_viewer_anim_tick(&mut self, cx: &mut Context<Self>) {
        if self.viewer_anim_tick_pending {
            return;
        }
        self.viewer_anim_tick_pending = true;
        cx.spawn(async move |view, cx| {
            cx.background_executor().timer(VIEWER_ANIM_TICK).await;
            let _ = view.update(cx, |view, cx| {
                view.viewer_anim_tick_pending = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// The pinned-message overlay floating over the chat log's top edge: one
    /// card per platform with an active pin (and the platform not hidden in
    /// settings) — 📌 + "Pinned by X", the message rendered like a chat line, an
    /// Unpin button for moderators, and an ✕ that *collapses* the card. A
    /// collapsed pin isn't gone: a small 📌 chip stays at the top-right and a
    /// click brings the card(s) back.
    fn render_pin_overlay(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        // No pins is the common case (every frame) — skip the deep message clone.
        if self.channel.read(cx).pins.is_empty() {
            return None;
        }
        // Clone the shared model's pins so we don't hold a model borrow across the
        // per-card render (which needs `cx`).
        let pins: Vec<(bks_core::Platform, ActivePin)> = self
            .channel
            .read(cx)
            .pins
            .iter()
            .map(|(p, pin)| (*p, pin.clone()))
            .collect();
        let mut cards: Vec<gpui::AnyElement> = Vec::new();
        let mut collapsed = 0usize;
        for platform in [bks_core::Platform::Twitch, bks_core::Platform::Kick] {
            let Some(pin) = pins.iter().find(|(p, _)| *p == platform).map(|(_, pin)| pin)
            else {
                continue;
            };
            if pin.expired() || !crate::settings::show_pinned(platform) {
                continue;
            }
            if self
                .dismissed_pins
                .contains(&(platform, pin.message.id.clone()))
            {
                collapsed += 1;
                continue;
            }
            cards.push(self.render_pin_banner(platform, pin, cx));
        }
        if cards.is_empty() && collapsed == 0 {
            return None;
        }
        // Full-bleed banners pinned to the chat's top edge; the collapsed-pin
        // chip floats at the top-right below them.
        let mut col = v_flex().absolute().top_0().left_0().right_0().children(cards);
        if collapsed > 0 {
            col = col.child(
                h_flex().justify_end().pt_1().pr_2().child(
                    div()
                        .id("restore-pins")
                        // Block the row (and its hover reply/pin buttons)
                        // painted underneath from also receiving the click.
                        .occlude()
                        .px_2()
                        .py_0p5()
                        .rounded_full()
                        .bg(gpui::rgb(render::panel_bg()))
                        .border_1()
                        .border_color(gpui::rgb(render::panel_border()))
                        .shadow_sm()
                        .text_size(px(self.font_size * 0.8))
                        .cursor_pointer()
                        // Opaque — a hover style replaces the base bg, and the
                        // translucent chrome_hover would let the rows this chip
                        // occludes bleed through.
                        .hover(|s| s.bg(gpui::rgb(render::panel_hover())))
                        .tooltip(|window, cx| {
                            Tooltip::new("Show the pinned message").build(window, cx)
                        })
                        .flex()
                        .items_center()
                        .gap_1()
                        // svg() needs its own text color — nothing cascades.
                        .child(
                            gpui::svg()
                                .path("icons/pin.svg")
                                .size(px(self.font_size * 0.8))
                                .flex_none()
                                .text_color(cx.theme().foreground),
                        )
                        .when(collapsed > 1, |chip| {
                            chip.child(SharedString::from(collapsed.to_string()))
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.dismissed_pins.clear();
                                cx.notify();
                                cx.stop_propagation();
                            }),
                        ),
                ),
            );
        }
        Some(col.into_any_element())
    }

    fn render_pin_banner(
        &self,
        platform: bks_core::Platform,
        pin: &ActivePin,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let p = render::palette();
        // The banner's message renders with the shared message renderer (badges,
        // colored name, emotes) against a throwaway selection — banner text isn't
        // part of the log's drag-select. The author name (and `@mentions` in the
        // body) open the chatter's usercard on the pin's platform, like they do in
        // the log; the click resolves the identity by name (the pinned message may
        // no longer be in the log buffer, so we can't key on its id).
        let selection = selectable::Selection::new();
        selection.begin_frame();
        let mut ordinal = 0usize;
        let author_login = pin.message.author.login.clone();
        let name_click: render::NameClick = {
            let entity = cx.entity();
            let login = author_login.clone();
            Box::new(move |_window: &mut Window, cx: &mut App| {
                entity.update(cx, |this, cx| {
                    this.open_usercard_named(&login, platform, cx);
                    cx.notify();
                });
            })
        };
        let mention_click = mention_click_for_platform(&cx.entity(), platform);
        let handlers = render::RowHandlers {
            name_click: Some(name_click),
            mention_click: Some(mention_click),
            ..Default::default()
        };
        let message = render::render_message(
            &pin.message,
            render::RowFlags::default(),
            self.font_size,
            &selection,
            &mut ordinal,
            handlers,
        );

        let pinned_by = pin.pinned_by.clone();
        let msg_id = pin.message.id.clone();

        // A full-width banner flush to the chat's top edge, floating over the
        // log (the shadow separates it from the rows scrolling underneath).
        // Two rows: a small "📌 Pinned by X" header with the Unpin/✕ controls
        // on its right, then the message on its own full-width line — so when
        // Twitch and Kick pins stack, the messages stay column-aligned no
        // matter how long each pinner's name is.
        // `occlude` keeps clicks on it from also hitting the row below.
        let mut header = h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap_1()
                    .text_size(px(self.font_size * 0.75))
                    .text_color(gpui::rgb(p.event_text))
                    // svg() needs its own text color — nothing cascades.
                    .child(
                        gpui::svg()
                            .path("icons/pin.svg")
                            .size(px(self.font_size * 0.8))
                            .flex_none()
                            .text_color(gpui::rgb(p.event_text)),
                    )
                    .map(|row| {
                        if pinned_by.is_empty() {
                            row.child(SharedString::from("Pinned"))
                        } else {
                            // The pinning moderator's name is clickable (opens their
                            // usercard on the pin's platform), the rest is label text.
                            let entity = cx.entity();
                            let name = pinned_by.clone();
                            row.child(SharedString::from("Pinned by")).child(
                                div()
                                    .id(SharedString::from(format!("pin-by-{platform:?}")))
                                    .cursor_pointer()
                                    .hover(|s| s.underline())
                                    .child(SharedString::from(pinned_by.clone()))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        move |_, _window, cx| {
                                            entity.update(cx, |this, cx| {
                                                this.open_usercard_named(&name, platform, cx);
                                                cx.notify();
                                            });
                                        },
                                    ),
                            )
                        }
                    }),
            );

        if self.can_pin(platform, cx) {
            header = header.child(
                Button::new(SharedString::from(format!("unpin-{platform:?}")))
                    .label("Unpin")
                    .outline()
                    .xsmall()
                    .compact()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_unpin(platform, window, cx);
                    })),
            );
        }

        header = header.child(
            div()
                .id(SharedString::from(format!("dismiss-pin-{platform:?}")))
                .flex_none()
                .px_1()
                .rounded_sm()
                .cursor_pointer()
                .text_color(cx.theme().muted_foreground)
                .hover(|s| {
                    s.bg(render::chrome_hover())
                        .text_color(cx.theme().foreground)
                })
                .child(SharedString::from("✕"))
                .tooltip(|window, cx| {
                    Tooltip::new("Collapse — the pin chip brings it back").build(window, cx)
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.dismissed_pins.insert((platform, msg_id.clone()));
                        cx.notify();
                    }),
                ),
        );

        v_flex()
            .occlude()
            .w_full()
            .gap_0p5()
            .px_2()
            .py_1()
            .bg(gpui::rgb(p.event_bg))
            .border_l_2()
            .border_color(gpui::rgb(p.event_text))
            .shadow_md()
            .child(header)
            .child(
                // Cap the banner's height so a wall-of-text pin can't shove the
                // log off screen; the content still wraps inside.
                div()
                    .w_full()
                    .min_w_0()
                    .max_h(px(120.))
                    .overflow_hidden()
                    .child(message),
            )
            .into_any_element()
    }

    /// Forgets all resolved cosmetics on the shared channel (7TV-cosmetics toggle
    /// switched off) so the change shows immediately.
    pub(crate) fn clear_cosmetics(&mut self, cx: &mut Context<Self>) {
        self.channel.update(cx, |m, cx| m.clear_cosmetics(cx));
    }

    /// The held AutoMod row's Allow/Deny click: forward to Helix. The result
    /// comes back as an EventSub update, which resolves the row in place.
    pub(crate) fn automod_action(&mut self, message_id: String, allow: bool) {
        self.controller.automod_twitch(message_id, allow);
    }

    /// Opens (or replaces) the usercard for the chatter who authored message
    /// `msg_id`, then kicks off the async account-stats fetch (Twitch only). The
    /// clicked name passes only the message id; the chatter's identity is read
    /// from that message here (no need to clone it onto every row's click).
    pub(crate) fn open_usercard(&mut self, msg_id: &str, cx: &mut Context<Self>) {
        let Some(msg) = self.message_by_id(msg_id, cx) else {
            return; // Message scrolled out of the buffer before the click landed.
        };
        let mut card = usercard::UserCard::new(
            msg.author.login.clone(),
            msg.author.display_name.clone(),
            msg.author.user_id.clone(),
            msg.platform,
            msg.author.color.map(bks_core::Color::to_u32),
        );
        card.set_roles_from_badges(&msg.author.badges);
        self.show_usercard(card, cx);
    }

    /// Shows `card` in the usercard window (shared by name clicks in chat and
    /// the viewer list) and starts the async account-stats fetch for it.
    fn show_usercard(&mut self, card: usercard::UserCard, cx: &mut Context<Self>) {
        let login = card.login.clone();
        let platform = card.platform;
        self.usercard = Some(card);
        // A stale custom-timeout/warn error would misread as being about the new card.
        self.usercard_timeout_error = None;
        self.usercard_warn_error = None;

        // Show the card's OS window. Deferred to a task because opening a window
        // draws it synchronously, and that draw re-enters this entity for the
        // body — which would double-lease it from inside this listener.
        let view = cx.entity();
        cx.spawn(async move |_, cx| {
            cx.update(|cx| Self::show_usercard_window(view, cx));
        })
        .detach();

        // Fetch account stats in the background; deliver back over a smol channel
        // the view drains, then store them on the (still-open) card. Twitch loads
        // via Helix; Kick via the broker. Other platforms have no lookup yet.
        match platform {
            bks_core::Platform::Twitch => {
                let (tx, rx) = smol::channel::bounded(1);
                self.controller.fetch_twitch_usercard(login.clone(), tx);
                self.apply_usercard_stats(login, rx, usercard::Stats::Twitch, cx);
            }
            bks_core::Platform::Kick => {
                let (tx, rx) = smol::channel::bounded(1);
                self.controller.fetch_kick_usercard(login.clone(), tx);
                self.apply_usercard_stats(login, rx, usercard::Stats::Kick, cx);
            }
            _ => {}
        }
    }

    /// Waits for a usercard stats fetch and stores the (wrapped) result on the
    /// still-open card — the shared back half of both platforms' lookups in
    /// [`show_usercard`](Self::show_usercard). `wrap` is the platform's `Stats`
    /// variant constructor.
    fn apply_usercard_stats<T: 'static>(
        &self,
        login: String,
        rx: smol::channel::Receiver<anyhow::Result<T>>,
        wrap: impl FnOnce(T) -> usercard::Stats + 'static,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |weak, cx| {
            if let Ok(result) = rx.recv().await {
                let _ = weak.update(cx, |this, cx| {
                    // Ignore if the card was closed or replaced meanwhile.
                    if let Some(card) = &mut this.usercard {
                        if card.login == login {
                            card.stats = match result {
                                Ok(data) => wrap(data),
                                Err(err) => usercard::Stats::Unavailable(format!("{err:#}")),
                            };
                            cx.notify();
                        }
                    }
                });
            }
        })
        .detach();
    }

    /// Opens (or re-points + refocuses) the child OS window hosting this tab's
    /// usercard. Runs from a plain `App` context (see the spawn in
    /// [`open_usercard`]); the body renders against this view, so replacing the
    /// card re-renders the same window.
    fn show_usercard_window(view: Entity<Self>, cx: &mut App) {
        let Some(title) = view
            .read(cx)
            .usercard
            .as_ref()
            .map(|card| format!("{}'s Usercard", card.display_name))
        else {
            return;
        };
        if let Some(handle) = view.read(cx).usercard_window {
            if child_window::focus_existing(handle, Some(&title), cx) {
                // The same window now shows a different chatter: a leftover
                // half-typed duration would apply to the wrong person.
                let _ = handle.update(cx, |_, window, cx| {
                    view.update(cx, |this, cx| {
                        if let Some(input) = &this.usercard_timeout_input {
                            input.update(cx, |state, cx| state.set_value("", window, cx));
                        }
                        if let Some(input) = &this.usercard_warn_input {
                            input.update(cx, |state, cx| state.set_value("", window, cx));
                        }
                    });
                });
                return;
            }
            // The window closed under us — fall through and open a fresh one.
        }

        // Opens at the last place the user left a usercard (persisted), else
        // centered over the chat window. Bare (no built-in scroll surface): the
        // header + mod actions stay put and only the recent-messages section
        // scrolls.
        let opened = child_window::open_persisted_bare(
            "usercard",
            &title,
            USERCARD_WINDOW_SIZE,
            USERCARD_MIN_SIZE,
            view.read(cx).parent_window,
            view.clone(),
            |this, cx| this.usercard_body(cx),
            cx,
        );
        let Ok((handle, content)) = opened else {
            return;
        };
        let _ = handle.update(cx, |_, window, cx| {
            view.update(cx, |this, cx| {
                // The custom-timeout box is window-bound like all kit inputs, so
                // it's created against this window (same rule as the viewer-list
                // search); Enter applies it.
                let input = cx.new(|cx| InputState::new(window, cx).placeholder("90s, 10m, 1h30m…"));
                this._usercard_timeout_sub =
                    Some(cx.subscribe_in(&input, window, Self::on_usercard_timeout_event));
                this.usercard_timeout_input = Some(input);
                let warn =
                    cx.new(|cx| InputState::new(window, cx).placeholder("Warning reason…"));
                this._usercard_warn_sub =
                    Some(cx.subscribe_in(&warn, window, Self::on_usercard_warn_event));
                this.usercard_warn_input = Some(warn);
                this.usercard_window = Some(handle);
                // The user closing the window (OS ✕) releases its content view;
                // drop the card then — unless a newer window replaced it.
                cx.observe_release(&content, move |this, _, cx| {
                    if this.usercard_window == Some(handle) {
                        this.usercard_window = None;
                        this.usercard = None;
                        this.usercard_timeout_input = None;
                        this._usercard_timeout_sub = None;
                        this.usercard_timeout_error = None;
                        this.usercard_warn_input = None;
                        this._usercard_warn_sub = None;
                        this.usercard_warn_error = None;
                    }
                    cx.notify();
                })
                .detach();
                cx.notify();
            });
        });
    }

    /// Applies the custom-timeout box on Enter.
    fn on_usercard_timeout_event(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputEvent::PressEnter { .. } = event {
            self.apply_custom_timeout(window, cx);
        }
    }

    /// Parses the custom-timeout box and times the card's chatter out for that
    /// long, or leaves an inline error under the box (unparseable, or over the
    /// platform's cap — 2 weeks on Twitch, 7 days on Kick).
    fn apply_custom_timeout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(card), Some(input)) = (&self.usercard, self.usercard_timeout_input.clone())
        else {
            return;
        };
        let platform = card.platform;
        let login = card.login.clone();
        let text = input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        let max = max_timeout_secs(platform);
        match bks_core::parse_duration(&text) {
            None => {
                self.usercard_timeout_error =
                    Some(format!("Can't read \"{text}\" — try 90s, 10m, 1h30m, or 3d"));
            }
            Some(secs) if secs > max as u64 => {
                self.usercard_timeout_error = Some(match platform {
                    bks_core::Platform::Kick => "Kick timeouts max out at 7 days".to_string(),
                    _ => "Twitch timeouts max out at 2 weeks".to_string(),
                });
            }
            Some(secs) => {
                self.usercard_moderate(platform, Mod::Timeout(secs as u32), &login);
                self.usercard_timeout_error = None;
                input.update(cx, |state, cx| state.set_value("", window, cx));
            }
        }
        cx.notify();
    }

    /// Applies the warn-reason box on Enter.
    fn on_usercard_warn_event(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputEvent::PressEnter { .. } = event {
            self.apply_usercard_warn(window, cx);
        }
    }

    /// Warns the card's chatter with the reason box's text (Twitch-only), or
    /// leaves an inline error under the box — Helix requires a non-empty reason
    /// of at most 500 characters.
    fn apply_usercard_warn(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(card), Some(input)) = (&self.usercard, self.usercard_warn_input.clone()) else {
            return;
        };
        if card.platform != bks_core::Platform::Twitch {
            return;
        }
        let login = card.login.clone();
        let reason = input.read(cx).value().trim().to_string();
        if reason.is_empty() {
            self.usercard_warn_error =
                Some("Enter a reason — the chatter has to acknowledge it".to_string());
        } else if reason.chars().count() > 500 {
            self.usercard_warn_error = Some("Warning reasons max out at 500 characters".to_string());
        } else {
            self.controller.warn_twitch(login, reason);
            self.usercard_warn_error = None;
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
        cx.notify();
    }

    /// Opens (or refreshes + refocuses) the Twitch viewer-list window for this
    /// tab. Fetches the chatters via Helix — broadcaster/moderator only, so the
    /// window shows an explanatory error for everyone else.
    fn open_viewer_list(&mut self, cx: &mut Context<Self>) {
        if self.config.twitch_channel.is_empty() {
            return;
        }
        self.viewer_list = Some(viewerlist::ViewerList::new(
            self.config.twitch_channel.clone(),
        ));
        self.refresh_viewer_list(cx);

        // Show the list's OS window. Deferred like the usercard: opening a
        // window draws it synchronously, re-entering this entity for the body.
        let view = cx.entity();
        cx.spawn(async move |_, cx| {
            cx.update(|cx| Self::show_viewer_list_window(view, cx));
        })
        .detach();
    }

    /// (Re)fetches the viewer list into the open window's state.
    fn refresh_viewer_list(&mut self, cx: &mut Context<Self>) {
        let Some(list) = &mut self.viewer_list else {
            return;
        };
        list.state = viewerlist::State::Loading;
        cx.notify();
        let channel = list.channel.clone();
        let (tx, rx) = smol::channel::bounded(1);
        self.controller.fetch_twitch_chatters(tx);
        cx.spawn(async move |weak, cx| {
            if let Ok(result) = rx.recv().await {
                let _ = weak.update(cx, |this, cx| {
                    // Ignore if the window was closed or re-pointed meanwhile.
                    if let Some(list) = &mut this.viewer_list {
                        if list.channel == channel {
                            list.resolve(result);
                            cx.notify();
                        }
                    }
                });
            }
        })
        .detach();
    }

    /// Opens (or refocuses) the child OS window hosting this tab's viewer list.
    /// Runs from a plain `App` context (see the spawn in [`open_viewer_list`]).
    /// The search input is created against this window — kit inputs are
    /// window-bound, so one made for the main window wouldn't get focus/cursor
    /// events here (same rule as the settings inputs).
    fn show_viewer_list_window(view: Entity<Self>, cx: &mut App) {
        let Some(title) = view
            .read(cx)
            .viewer_list
            .as_ref()
            .map(|list| format!("Viewer List - {}", list.channel))
        else {
            return;
        };
        if let Some(handle) = view.read(cx).viewer_list_window {
            if child_window::focus_existing(handle, Some(&title), cx) {
                return;
            }
            // The window closed under us — fall through and open a fresh one.
        }

        let opened = child_window::open_centered(
            &title,
            VIEWERLIST_WINDOW_SIZE,
            VIEWERLIST_MIN_SIZE,
            view.read(cx).parent_window,
            view.clone(),
            |this, cx| this.viewer_list_body(cx),
            cx,
        );
        let Ok((handle, content)) = opened else {
            return;
        };
        let _ = handle.update(cx, |_, window, cx| {
            view.update(cx, |this, cx| {
                let search =
                    cx.new(|cx| InputState::new(window, cx).placeholder("Search viewers…"));
                this._viewer_search_sub =
                    Some(cx.subscribe_in(&search, window, Self::on_viewer_search_event));
                this.viewer_search = Some(search);
                this.viewer_list_window = Some(handle);
                // The user closing the window (OS ✕) releases its content view;
                // drop the list then — unless a newer window replaced it.
                cx.observe_release(&content, move |this, _, cx| {
                    if this.viewer_list_window == Some(handle) {
                        this.viewer_list_window = None;
                        this.viewer_list = None;
                        this.viewer_search = None;
                        this._viewer_search_sub = None;
                    }
                    cx.notify();
                })
                .detach();
                cx.notify();
            });
        });
    }

    /// Re-filters the viewer list as the user types in its search box.
    fn on_viewer_search_event(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputEvent::Change = event {
            cx.notify();
        }
    }

    /// The viewer-list window's content: a count + refresh header, the search
    /// box, and the (filtered, capped) name column. Clicking a name opens that
    /// chatter's usercard.
    fn viewer_list_body(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(list) = &self.viewer_list else {
            return gpui::Empty.into_any_element();
        };

        let header_text = match &list.state {
            viewerlist::State::Loading => "loading…".to_string(),
            viewerlist::State::Failed(_) => String::new(),
            viewerlist::State::Loaded(chatters) => {
                let unit = bks_core::plural(chatters.total, "chatter", "chatters");
                format!("{} {unit}", chatters.total)
            }
        };
        let header = h_flex()
            .gap_2()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from(header_text)),
            )
            .child(
                Button::new("viewerlist-refresh")
                    .label("Refresh")
                    .outline()
                    .xsmall()
                    .compact()
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_viewer_list(cx))),
            );

        let content: gpui::AnyElement = match &list.state {
            viewerlist::State::Loading => div()
                .text_size(px(13.))
                .text_color(cx.theme().muted_foreground)
                .child(SharedString::from("loading viewer list…"))
                .into_any_element(),
            viewerlist::State::Failed(err) => div()
                .text_size(px(13.))
                .text_color(gpui::rgb(0xe05d5d))
                .child(SharedString::from(err.clone()))
                .into_any_element(),
            viewerlist::State::Loaded(chatters) => {
                let query = self
                    .viewer_search
                    .as_ref()
                    .map(|s| s.read(cx).value().to_string())
                    .unwrap_or_default();
                let matched = viewerlist::filter(&chatters.chatters, &query);
                let shown = matched.len().min(viewerlist::MAX_SHOWN);
                let hidden = matched.len() - shown;
                let rows: Vec<gpui::AnyElement> = matched[..shown]
                    .iter()
                    .enumerate()
                    .map(|(ix, chatter)| {
                        let login = chatter.user_login.clone();
                        let name = if chatter.user_name.is_empty() {
                            chatter.user_login.clone()
                        } else {
                            chatter.user_name.clone()
                        };
                        let user_id = chatter.user_id.clone();
                        div()
                            .id(("viewer", ix))
                            .px_1()
                            .py_0p5()
                            .rounded_sm()
                            .text_size(px(13.))
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().secondary))
                            .child(SharedString::from(viewerlist::label(chatter)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    let card = usercard::UserCard::new(
                                        login.clone(),
                                        name.clone(),
                                        user_id.clone(),
                                        bks_core::Platform::Twitch,
                                        None,
                                    );
                                    this.show_usercard(card, cx);
                                }),
                            )
                            .into_any_element()
                    })
                    .collect();
                let mut col = v_flex()
                    .id("viewer-list-names")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows);
                if hidden > 0 {
                    col = col.child(
                        div()
                            .pt_1()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child(SharedString::from(format!(
                                "…and {hidden} more — search to narrow the list"
                            ))),
                    );
                }
                col.into_any_element()
            }
        };

        let mut body = v_flex().h_full().gap_2().child(header);
        if let Some(search) = &self.viewer_search {
            body = body.child(Input::new(search));
        }
        body.child(content).into_any_element()
    }

    /// Opens the emote popup for a clicked 7TV link: shows a loading placeholder
    /// immediately, then fetches the emote by id (7TV REST) and fills the popup in
    /// when it returns — unless the popup was closed or replaced meanwhile.
    fn open_seventv_link(&mut self, id: String, pos: Point<Pixels>, cx: &mut Context<Self>) {
        self.emote_popup = Some(EmotePopup::loading(id.clone(), pos));
        cx.spawn(async move |weak, cx| {
            let result = bks_emotes::fetch_emote(&id).await;
            let _ = weak.update(cx, |this, cx| {
                // Only apply if the loading popup for this id is still showing.
                let still_loading = this
                    .emote_popup
                    .as_ref()
                    .is_some_and(|p| p.loading && p.emote_id == id);
                if !still_loading {
                    return;
                }
                match result {
                    Ok(emote) => {
                        this.emote_popup = Some(EmotePopup::from_emote(&emote, pos));
                    }
                    Err(err) => {
                        if let Some(p) = &mut this.emote_popup {
                            p.loading = false;
                            p.name = SharedString::from(format!("couldn't load emote: {err:#}"));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Handles a link's hover enter/leave from the render layer, driving the
    /// preview tooltip. Only acts in Tooltip mode and for URLs a provider can
    /// preview; everything else is a no-op (the normal link stays as-is).
    fn on_link_preview_hover(&mut self, url: &str, entered: bool, anchor: Point<Pixels>, cx: &mut Context<Self>) {
        if crate::settings::link_preview_mode() != crate::settings::LinkPreviewMode::Tooltip {
            return;
        }
        if entered {
            if crate::preview::is_supported(url) {
                self.arm_link_preview(url.to_string(), anchor, cx);
            }
        } else {
            self.disarm_link_preview(cx);
        }
    }

    /// Arms a link preview: starts the cached fetch immediately (so data is ready
    /// by the time the card shows) and, after the show-delay, reveals the tooltip
    /// if the pointer is still on this same link.
    fn arm_link_preview(&mut self, url: String, anchor: Point<Pixels>, cx: &mut Context<Self>) {
        // Re-entering the same link (crossing a wrap seam) just marks it hovered
        // again — the fetch + show-delay are already running, so don't restart.
        if let Some(p) = self.link_preview.as_mut().filter(|p| p.url == url) {
            p.hovering = true;
            p.anchor = anchor;
            return;
        }
        self.link_preview_gen += 1;
        let gen = self.link_preview_gen;
        self.link_preview = Some(LinkPreviewHover {
            url: url.clone(),
            anchor,
            shown: false,
            hovering: true,
        });

        // Start the fetch; a completion wakes this view to repaint (render peeks
        // the cache directly, so the returned state is unused here). If already
        // resolved no wake comes, but the show-delay below still repaints.
        let (tx, rx) = smol::channel::bounded::<String>(1);
        crate::preview::lookup(&url, &self.controller.runtime(), tx);
        cx.spawn(async move |this, cx| {
            if rx.recv().await.is_ok() {
                let _ = this.update(cx, |this, cx| {
                    if this.link_preview.as_ref().is_some_and(|p| p.url == url) {
                        cx.notify();
                    }
                });
            }
        })
        .detach();

        // Reveal the card after the show-delay if this arm is still current.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LINK_PREVIEW_SHOW_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                if this.link_preview_gen == gen {
                    if let Some(p) = &mut this.link_preview {
                        p.shown = true;
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// Marks the preview un-hovered and, after the hide-grace, clears it unless
    /// the pointer returned to the link meanwhile (re-checked via `hovering` at
    /// fire time, so crossing a wrapped link's seam doesn't dismiss it).
    fn disarm_link_preview(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.link_preview.as_mut() else {
            return;
        };
        p.hovering = false;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LINK_PREVIEW_HIDE_GRACE).await;
            let _ = this.update(cx, |this, cx| {
                if this.link_preview.as_ref().is_some_and(|p| !p.hovering) {
                    this.link_preview = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Kicks off the inline preview fetch for a newly-arrived message, when inline
    /// mode is on and it has a previewable link. The card renders a fixed-height
    /// skeleton immediately (so the row doesn't jump), then fills in when the
    /// fetch lands — a repaint suffices on success (same height); a *failed* fetch
    /// collapses the card, so that re-measures the log.
    fn arm_inline_preview(&self, msg: &Message, cx: &mut Context<Self>) {
        if crate::settings::link_preview_mode() != crate::settings::LinkPreviewMode::Inline {
            return;
        }
        let Some(url) = crate::preview::first_previewable_url(msg) else {
            return;
        };
        let (tx, rx) = smol::channel::bounded::<String>(1);
        // Only spawns a fetch if the URL isn't already cached/in-flight.
        crate::preview::lookup(&url, &self.controller.runtime(), tx);
        cx.spawn(async move |this, cx| {
            if let Ok(done_url) = rx.recv().await {
                let _ = this.update(cx, |this, cx| {
                    // A failed fetch collapses the card (height change → re-measure);
                    // a ready one is the same height as the skeleton (repaint only).
                    if matches!(
                        crate::preview::peek(&done_url),
                        crate::preview::PreviewState::None
                    ) {
                        this.remeasure(cx);
                    } else {
                        this.refresh_log(cx);
                    }
                });
            }
        })
        .detach();
    }

    /// Arms inline-preview fetches for every message currently in the buffer —
    /// used when the user switches *to* Inline mode, so already-shown messages
    /// with links get their cards (later messages are armed on append). Cheap
    /// when nothing has a previewable link (URL matching only), and the cache
    /// dedupes so a link posted many times fetches once.
    pub(crate) fn arm_buffered_inline_previews(&self, cx: &mut Context<Self>) {
        let msgs: Vec<std::sync::Arc<Message>> = self
            .channel
            .read(cx)
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Message { msg } => Some(msg.clone()),
                _ => None,
            })
            .collect();
        for msg in msgs {
            self.arm_inline_preview(&msg, cx);
        }
    }

    /// This chatter's recent messages in the current feed (oldest first), capped
    /// to the most recent [`USERCARD_MESSAGES`]. Matched by lowercased login.
    /// Live messages only — the `historical` backlog fetched at join is skipped.
    /// Clones the shared `Arc<Message>`s (cheap) out of the channel model.
    fn usercard_messages(&self, login: &str, cx: &App) -> Vec<std::sync::Arc<Message>> {
        let mut msgs: Vec<std::sync::Arc<Message>> = self
            .channel
            .read(cx)
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Message { msg } if msg.author.login == login && !msg.historical => {
                    Some(msg.clone())
                }
                _ => None,
            })
            .collect();
        let skip = msgs.len().saturating_sub(USERCARD_MESSAGES);
        msgs.drain(..skip);
        msgs
    }

    /// Finds a message row by its id (for a clicked name to resolve its author).
    fn message_by_id(&self, id: &str, cx: &App) -> Option<std::sync::Arc<Message>> {
        self.channel.read(cx).rows.iter().rev().find_map(|row| match row {
            Row::Message { msg } if msg.id == id => Some(msg.clone()),
            _ => None,
        })
    }

    /// Handles a Tab press in the input: completes the word at the cursor against
    /// emote names and chatter names. The first Tab replaces the
    /// word with the best match; subsequent Tabs cycle through the rest. Typing
    /// anything between presses restarts the cycle (detected via `last_text`).
    /// Returns whether it handled the key (so the caller can stop propagation).
    fn complete_input(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let text = self.input.read(cx).value().to_string();
        let cursor = self.input.read(cx).cursor().min(text.len());

        // Continue an active cycle only if the text is exactly what we last wrote
        // (otherwise the user edited it — start fresh from the new word).
        if let Some(c) = &self.completion {
            if c.last_text == text && c.candidates.len() > 1 {
                let next = (c.index + 1) % c.candidates.len();
                return self.apply_completion(next, &text, window, cx);
            }
        }

        // Start a new cycle: find the word ending at the cursor.
        let start = word_start(&text, cursor);
        let word = &text[start..cursor];
        if word.is_empty() {
            return false;
        }
        let at_prefixed = word.starts_with('@');
        let stem = word.trim_start_matches('@');
        if stem.is_empty() {
            return false;
        }

        let candidates = self.completion_candidates(stem, at_prefixed, cx);
        if candidates.is_empty() {
            return false;
        }
        self.completion = Some(Completion {
            start,
            candidates,
            index: 0,
            last_text: String::new(),
        });
        self.apply_completion(0, &text, window, cx)
    }

    /// Replaces the word being completed (from `start` to the old cursor) with
    /// candidate `index`, appends a trailing space, and records the result so the
    /// next Tab knows the cycle is still live. The current cursor is re-derived
    /// from the completion start each time so cycling replaces the prior insertion.
    fn apply_completion(
        &mut self,
        index: usize,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(c) = self.completion.as_mut() else {
            return false;
        };
        let Some(candidate) = c.candidates.get(index).cloned() else {
            return false;
        };
        c.index = index;

        // The word being completed runs from `start` to the next space (on the
        // first apply that's the user's typed word; on a re-cycle it's the prior
        // candidate) — replace just it.
        let next = replace_word(text, c.start, &candidate);
        c.last_text = next.clone();
        self.input.update(cx, |state, cx| {
            state.set_value(&next, window, cx);
        });
        true
    }

    /// The completion candidates for `stem` (the word without a leading `@`),
    /// case-insensitive prefix match. Usernames come first when the word was
    /// `@`-prefixed (and are returned with the `@`); otherwise emotes come first,
    /// then usernames. Deduplicated, capped, in match order.
    fn completion_candidates(&self, stem: &str, at_prefixed: bool, cx: &App) -> Vec<String> {
        let stem_lc = stem.to_lowercase();
        let mut emotes: Vec<String> = Vec::new();
        // Set-based dedup + alloc-free prefix match: large 7TV channels carry
        // thousands of emotes, and the old per-candidate `to_lowercase` +
        // linear `contains` made each Tab press O(n²) with an allocation per
        // emote.
        let mut seen_emotes: HashSet<&str> = HashSet::new();
        let model = self.channel.read(cx);
        // Completion draws from every emote set the tab has — Twitch (channel +
        // personal), Kick, and YouTube — regardless of which picker tab is selected.
        let all = model
            .emotes_twitch
            .iter()
            .chain(model.emotes_kick.iter())
            .chain(model.emotes_youtube.iter())
            .chain(self.personal_emotes.iter());
        for e in all {
            if starts_with_ci(&e.name, &stem_lc) && seen_emotes.insert(e.name.as_str()) {
                emotes.push(e.name.clone());
            }
        }
        // Recent chatters (most-recent first), matched by display name or login.
        let mut names: Vec<String> = Vec::new();
        let mut seen_names: HashSet<String> = HashSet::new();
        for row in model.rows.iter().rev() {
            if let Row::Message { msg } = row {
                let display = &msg.author.display_name;
                if starts_with_ci(display, &stem_lc) && seen_names.insert(display.to_lowercase()) {
                    names.push(display.clone());
                }
            }
        }
        let names: Vec<String> = if at_prefixed {
            names.into_iter().map(|n| format!("@{n}")).collect()
        } else {
            names
        };

        const MAX_CANDIDATES: usize = 40;
        let mut out: Vec<String> = if at_prefixed {
            names.into_iter().chain(emotes).collect()
        } else {
            emotes.into_iter().chain(names).collect()
        };
        out.truncate(MAX_CANDIDATES);
        out
    }

    /// Recomputes the autocomplete popup from the input's current text + cursor
    /// (called on every input change). The popup exists exactly while the word
    /// ending at the cursor starts with `@` or `:`, so typing a space after the
    /// completed word dismisses it without special-casing.
    fn update_input_popup(&mut self, cx: &mut Context<Self>) {
        let next = self.popup_state(cx);
        if next.is_some() || self.popup.is_some() {
            self.popup = next;
            cx.notify();
        }
    }

    /// Recomputes an already-open autocomplete popup (e.g. after the async native-
    /// emote fetch lands) so it reflects the new candidate set. A no-op when no
    /// popup is showing, so it never makes one appear on its own.
    pub(super) fn refresh_emote_popup(&mut self, cx: &mut Context<Self>) {
        if self.popup.is_some() {
            self.popup = self.popup_state(cx);
            cx.notify();
        }
    }

    /// The popup state for the input's current text + cursor, or `None` when the
    /// word at the cursor is neither an `@`-mention, a `:`-emote, nor a leading
    /// `/`-command (or this tab has no platform to complete for).
    fn popup_state(&self, cx: &Context<Self>) -> Option<InputPopup> {
        let state = self.input.read(cx);
        let text = state.value().to_string();
        let cursor = state.cursor().min(text.len());
        let start = word_start(&text, cursor);
        let word = &text[start..cursor];
        if self.target_platforms().is_empty() {
            return None;
        }
        let items: Vec<PopupItem> = if let Some(stem) = word.strip_prefix('@') {
            self.mention_candidates(stem, cx)
                .into_iter()
                .map(PopupItem::Mention)
                .collect()
        } else if start == 0 && word.starts_with('/') {
            // A slash only triggers commands at the start of the line — "and/or"
            // or a mid-sentence path stays plain text.
            return Some(self.command_popup(&word[1..], cx));
        } else {
            let stem = word.strip_prefix(':')?;
            self.emote_popup_candidates(stem, cx)
                .into_iter()
                .map(PopupItem::Emote)
                .collect()
        };
        Some(InputPopup {
            start,
            items,
            selected: 0,
            window_start: 0,
            empty_text: "No matches".into(),
        })
    }

    /// The command-popup state for `stem` (the typed text after the leading
    /// `/`): the send target platform's commands from the registry
    /// ([`crate::commands`]), with mod-only commands hidden unless the user can
    /// moderate that platform's channel and broadcaster-only ones (raid, role
    /// grants) hidden unless they own it. In Both send mode commands have no
    /// unambiguous target, so the popup is a single explanatory notice instead.
    fn command_popup(&self, stem: &str, cx: &App) -> InputPopup {
        let (items, empty_text) = if self.controller.send_target() == controller::SendTarget::Both {
            (
                Vec::new(),
                "commands don't work while sending to both platforms — switch the send target",
            )
        } else {
            let items = self
                .target_platforms()
                .first()
                .map(|&platform| {
                    let model = self.channel.read(cx);
                    let can_mod = model.can_moderate(platform);
                    let owner = model.is_broadcaster(platform);
                    crate::commands::matching(platform, stem)
                        .into_iter()
                        .filter(|m| {
                            (can_mod || !m.def.mod_only) && (owner || !m.def.broadcaster_only)
                        })
                        .map(PopupItem::Command)
                        .collect()
                })
                .unwrap_or_default();
            (items, "No matching commands")
        };
        InputPopup {
            start: 0,
            items,
            selected: 0,
            window_start: 0,
            empty_text: empty_text.into(),
        }
    }

    /// The platform(s) popup candidates are drawn from: the send target
    /// narrowed to this tab's configured channels. When that intersection is
    /// empty (e.g. a Kick-only tab whose default target is Twitch), falls back
    /// to whatever chat platforms the tab has. YouTube is excluded — we can't
    /// send there at all.
    fn target_platforms(&self) -> Vec<bks_core::Platform> {
        use bks_core::Platform;
        let has_twitch = !self.config.twitch_channel.is_empty();
        let has_kick = !self.config.kick_channel.is_empty();
        let mut plats = Vec::new();
        match self.controller.send_target() {
            controller::SendTarget::Twitch if has_twitch => plats.push(Platform::Twitch),
            controller::SendTarget::Kick if has_kick => plats.push(Platform::Kick),
            controller::SendTarget::Both => {
                if has_twitch {
                    plats.push(Platform::Twitch);
                }
                if has_kick {
                    plats.push(Platform::Kick);
                }
            }
            _ => {}
        }
        if plats.is_empty() {
            if has_twitch {
                plats.push(Platform::Twitch);
            }
            if has_kick {
                plats.push(Platform::Kick);
            }
        }
        plats
    }

    /// The mention-popup candidates for `stem` (the typed word without the `@`).
    /// A bare `@` lists the broadcaster(s) first, then the most recent chatters;
    /// a non-empty stem prefix-matches every known chatter (broadcasters
    /// included) alphabetically. All matches — the popup windows them
    /// [`POPUP_VISIBLE_ITEMS`] at a time. May be empty ("No matches").
    fn mention_candidates(&self, stem: &str, cx: &App) -> Vec<String> {
        use bks_core::Platform;
        let plats = self.target_platforms();
        let model = self.channel.read(cx);

        // The targeted platforms' broadcasters: prefer the display name from a
        // message they've sent, falling back to the configured channel name.
        let mut broadcasters: Vec<String> = Vec::new();
        for p in &plats {
            let channel = match p {
                Platform::Twitch => &self.config.twitch_channel,
                Platform::Kick => &self.config.kick_channel,
                _ => continue,
            };
            if channel.is_empty() {
                continue;
            }
            let display = model
                .rows
                .iter()
                .rev()
                .find_map(|row| match row {
                    Row::Message { msg }
                        if msg.platform == *p && msg.author.login.eq_ignore_ascii_case(channel) =>
                    {
                        Some(msg.author.display_name.clone())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| channel.clone());
            if !broadcasters
                .iter()
                .any(|b| b.eq_ignore_ascii_case(&display))
            {
                broadcasters.push(display);
            }
        }

        if stem.is_empty() {
            // Bare `@`: broadcaster(s), then most recent chatters. Set-based
            // dedup — the linear `any` scan per row made this O(rows × names).
            let mut seen: HashSet<String> = broadcasters.iter().map(|b| b.to_lowercase()).collect();
            let mut out = broadcasters;
            for row in model.rows.iter().rev() {
                if let Row::Message { msg } = row {
                    let name = &msg.author.display_name;
                    if plats.contains(&msg.platform) && seen.insert(name.to_lowercase()) {
                        out.push(name.clone());
                    }
                }
            }
            return out;
        }

        let stem_lc = stem.to_lowercase();
        let mut names: Vec<String> = broadcasters
            .into_iter()
            .filter(|b| starts_with_ci(b, &stem_lc))
            .collect();
        let mut seen: HashSet<String> = names.iter().map(|n| n.to_lowercase()).collect();
        for row in model.rows.iter() {
            if let Row::Message { msg } = row {
                let name = &msg.author.display_name;
                if plats.contains(&msg.platform)
                    && starts_with_ci(name, &stem_lc)
                    && seen.insert(name.to_lowercase())
                {
                    names.push(name.clone());
                }
            }
        }
        names.sort_by_cached_key(|n| n.to_lowercase());
        names
    }

    /// The emote-popup candidates for `stem` (the typed word without the `:`):
    /// case-insensitive prefix matches from the send target's emote set(s),
    /// alphabetical — all matches, windowed [`POPUP_VISIBLE_ITEMS`] at a time.
    /// Only in genuine **Both** send mode is the set restricted to emotes present
    /// on *every* targeted platform (by exact name) — a line sent to both must
    /// render on both. Otherwise (a single-platform target, including the logged-
    /// out fallback) every emote of that platform completes, so channel-specific
    /// Twitch emotes aren't dropped just because the tab also has a Kick channel.
    /// Personal Twitch emotes count as Twitch.
    fn emote_popup_candidates(&self, stem: &str, cx: &App) -> Vec<bks_core::Emote> {
        use bks_core::Platform;
        let model = self.channel.read(cx);
        let set_for = |p: &Platform| -> Vec<&bks_core::Emote> {
            match p {
                Platform::Kick => model.emotes_kick.iter().collect(),
                _ => model
                    .emotes_twitch
                    .iter()
                    .chain(self.personal_emotes.iter())
                    .collect(),
            }
        };
        let sets: Vec<Vec<&bks_core::Emote>> =
            self.target_platforms().iter().map(set_for).collect();
        let Some((first, rest)) = sets.split_first() else {
            return Vec::new();
        };
        // Intersect across platforms only when the user is really sending to both;
        // a single-platform target (or the two-platform fallback used when logged
        // out) completes that platform's full set.
        let intersect = matches!(self.controller.send_target(), controller::SendTarget::Both);
        // Precompute the other platforms' name sets once — the old linear scan
        // of every other set per candidate made Both-mode O(n²·m) per keystroke.
        let rest_names: Vec<HashSet<&str>> = if intersect {
            rest.iter()
                .map(|s| s.iter().map(|e| e.name.as_str()).collect())
                .collect()
        } else {
            Vec::new()
        };
        let stem_lc = stem.to_lowercase();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut out: Vec<bks_core::Emote> = Vec::new();
        for e in first {
            if !starts_with_ci(&e.name, &stem_lc)
                || !seen.insert(e.name.as_str())
                || !rest_names.iter().all(|s| s.contains(e.name.as_str()))
            {
                continue;
            }
            out.push((*e).clone());
        }
        out.sort_by_cached_key(|e| e.name.to_lowercase());
        out
    }

    /// Moves the popup's highlight by `delta` (wrapping), scrolling the visible
    /// window along when the highlight steps past an edge. Returns whether it
    /// consumed the key (popup open with something to select).
    fn popup_move(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let Some(popup) = self.popup.as_mut() else {
            return false;
        };
        let len = popup.items.len();
        if len == 0 {
            return false;
        }
        popup.selected = (popup.selected as isize + delta).rem_euclid(len as isize) as usize;
        if popup.selected < popup.window_start {
            popup.window_start = popup.selected;
        } else if popup.selected >= popup.window_start + POPUP_VISIBLE_ITEMS {
            popup.window_start = popup.selected + 1 - POPUP_VISIBLE_ITEMS;
        }
        cx.notify();
        true
    }

    /// Inserts popup candidate `index` into the input: replaces the trigger
    /// word being completed with the candidate's text (`@Name ` for a mention,
    /// `Name ` for an emote — trailing space), keeping any
    /// text that followed it, and closes the popup.
    fn popup_select(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(insert) = self
            .popup
            .as_ref()
            .and_then(|m| m.items.get(index))
            .map(PopupItem::insert_text)
        else {
            return;
        };
        let start = self.popup.take().map(|m| m.start).unwrap_or(0);
        let text = self.input.read(cx).value().to_string();
        let next = replace_word(&text, start.min(text.len()), &insert);
        self.input.update(cx, |state, cx| {
            state.set_value(&next, window, cx);
            state.focus(window, cx);
        });
        self.popup = None;
        cx.notify();
    }

    /// A Tab press while the popup is open: dismisses an empty ("No matches")
    /// popup, otherwise inserts the highlighted candidate — Tab confirms like
    /// Enter; Up/Down move the highlight.
    fn popup_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.popup.as_ref().map(|m| (m.items.len(), m.selected)) {
            Some((0, _)) => {
                self.popup = None;
                cx.notify();
            }
            Some((_, ix)) => self.popup_select(ix, window, cx),
            None => {}
        }
    }

    /// The autocomplete popup, anchored just above the input bar (its parent).
    /// Rows are clickable; the highlighted one tracks Up/Down. Only
    /// [`POPUP_VISIBLE_ITEMS`] rows show at once — the window follows the
    /// highlight (and the mouse wheel), with muted "N more" hints past each
    /// edge. Emote rows show a first-frame poster thumbnail (cheap — no full
    /// animation decode while typing) next to the name.
    fn render_input_popup(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let popup = self.popup.as_ref()?;
        let more_hint = |n: usize, arrow: &str| {
            div()
                .px_2()
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .child(SharedString::from(format!("{arrow} {n} more")))
                .into_any_element()
        };
        let len = popup.items.len();
        let win_start = popup.window_start.min(len.saturating_sub(1));
        let win_end = (win_start + POPUP_VISIBLE_ITEMS).min(len);
        let mut body: Vec<gpui::AnyElement> = Vec::new();
        if popup.items.is_empty() {
            body.push(
                div()
                    .px_2()
                    .py_1()
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(popup.empty_text.clone())
                    .into_any_element(),
            );
        } else {
            if win_start > 0 {
                body.push(more_hint(win_start, "▲"));
            }
            body.extend(
                popup
                    .items
                    .iter()
                    .enumerate()
                    .take(win_end)
                    .skip(win_start)
                    .map(|(ix, item)| {
                        let selected = ix == popup.selected;
                        let content: gpui::AnyElement = match item {
                            PopupItem::Mention(name) => {
                                SharedString::from(name.clone()).into_any_element()
                            }
                            PopupItem::Emote(e) => h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    img(SharedString::from(format!(
                                        "{}{}",
                                        crate::image_cache::POSTER_PREFIX,
                                        e.url
                                    )))
                                    .image_cache(&self.image_cache)
                                    .h(px(20.))
                                    .max_w(px(40.)),
                                )
                                .child(SharedString::from(e.name.clone()))
                                .into_any_element(),
                            PopupItem::Command(m) => v_flex()
                                .child(SharedString::from(m.usage()))
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .when(!selected, |d| {
                                            d.text_color(cx.theme().muted_foreground)
                                        })
                                        .child(SharedString::from(m.def.description)),
                                )
                                .into_any_element(),
                        };
                        div()
                            .id(("popup-item", ix))
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_size(px(14.))
                            .when(selected, |d| {
                                d.bg(cx.theme().accent)
                                    .text_color(cx.theme().accent_foreground)
                            })
                            .hover(|d| d.bg(cx.theme().accent.opacity(0.7)))
                            .child(content)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    this.popup_select(ix, window, cx);
                                }),
                            )
                            .into_any_element()
                    }),
            );
            if win_end < len {
                body.push(more_hint(len - win_end, "▼"));
            }
        }
        Some(
            v_flex()
                .id("input-popup")
                .occlude()
                .absolute()
                .bottom(gpui::relative(1.))
                .left(px(8.))
                .min_w(px(200.))
                .max_w(px(320.))
                .p_1()
                .bg(cx.theme().popover)
                .text_color(cx.theme().popover_foreground)
                .border_1()
                .border_color(cx.theme().border)
                .rounded_lg()
                .shadow_lg()
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    if this.popup.take().is_some() {
                        cx.notify();
                    }
                }))
                // Wheel scrolling shifts the visible window (the highlight stays
                // where it is, like a listbox); wheel up at the top / down at the
                // bottom is a no-op.
                .on_scroll_wheel(cx.listener(|this, ev: &gpui::ScrollWheelEvent, _, cx| {
                    let Some(popup) = this.popup.as_mut() else {
                        return;
                    };
                    let max = popup.items.len().saturating_sub(POPUP_VISIBLE_ITEMS);
                    let dy = ev.delta.pixel_delta(px(20.)).y;
                    let step: isize = match dy {
                        d if d > px(0.) => -1,
                        d if d < px(0.) => 1,
                        _ => return,
                    };
                    let next = (popup.window_start as isize + step).clamp(0, max as isize) as usize;
                    if next != popup.window_start {
                        popup.window_start = next;
                        cx.notify();
                    }
                }))
                .children(body)
                .into_any_element(),
        )
    }

    /// A clickable chip showing/cycling the send target — only when logged into
    /// Kick and this tab has both a Twitch and a Kick channel (a
    /// single-platform tab has nothing to switch).
    fn send_target_toggle(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.controller.kick_logged_in()
            || !self.controller.has_kick()
            || !self.controller.has_twitch()
        {
            return None;
        }
        use bks_core::Platform;
        // The icons that represent the current target, plus a tooltip label so the
        // meaning stays discoverable now that the letters are gone.
        let (platforms, tip): (&[Platform], &str) = match self.controller.send_target() {
            controller::SendTarget::Twitch => (&[Platform::Twitch], "Sending to Twitch"),
            controller::SendTarget::Kick => (&[Platform::Kick], "Sending to Kick"),
            controller::SendTarget::Both => (
                &[Platform::Twitch, Platform::Kick],
                "Sending to Twitch + Kick",
            ),
        };
        // A subtle accent ring in the active platform's color (Both → neutral).
        let ring = match self.controller.send_target() {
            controller::SendTarget::Twitch => 0x9147ff,
            controller::SendTarget::Kick => 0x53fc18,
            controller::SendTarget::Both => 0x808080,
        };
        let icons = platforms.iter().filter_map(|p| {
            p.icon_url().map(|url| {
                img(SharedString::from(url))
                    .id(SharedString::from(p.label()))
                    .h(px(17.))
                    .w(px(17.))
            })
        });
        let tip = SharedString::from(tip);
        // Compact — it sits *inside* the input box as its prefix: a small pill
        // ringed in the active platform's color, click cycles the target.
        Some(
            h_flex()
                .id("send-target")
                .gap_1()
                .px_1p5()
                .py_0p5()
                .rounded_sm()
                .border_1()
                .border_color(gpui::rgb(ring))
                .cursor_pointer()
                .hover(|s| s.bg(render::chrome_hover()))
                .children(icons)
                .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.controller.cycle_send_target();
                        cx.notify();
                    }),
                )
                .into_any_element(),
        )
    }

    /// The status-bar button opening the Twitch viewer list — only when this tab
    /// has a Twitch channel (Kick/YouTube expose no chatters API at all). Lives
    /// on the status bar (a while-live, mods-only feature), and `/viewers` /
    /// `/chatters` still open it any time.
    fn viewerlist_button(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.config.twitch_channel.is_empty() {
            return None;
        }
        Some(
            div()
                .id("viewer-list-toggle")
                .px_1p5()
                .rounded_sm()
                .cursor_pointer()
                .text_color(cx.theme().muted_foreground)
                .hover(|s| {
                    s.bg(render::chrome_hover())
                        .text_color(cx.theme().foreground)
                })
                .child(SharedString::from("👥"))
                .tooltip(|window, cx| {
                    Tooltip::new("Viewer list (mods only — Twitch restriction)").build(window, cx)
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.open_viewer_list(cx)),
                )
                .into_any_element(),
        )
    }

    /// The draggable divider between layout columns `left` and `left + 1`.
    /// Dragging redistributes their width shares (see [`resize_columns`]).
    fn column_divider(&self, left: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .id(("col-divider", left))
            .w(px(5.))
            .flex_none()
            .h_full()
            .cursor_ew_resize()
            .bg(cx.theme().border)
            .hover(|s| s.bg(cx.theme().muted_foreground))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &gpui::MouseDownEvent, _, cx| {
                    let cols = &this.config.layout.columns;
                    let (Some(l), Some(r)) = (cols.get(left), cols.get(left + 1)) else {
                        return;
                    };
                    this.layout_drag = Some(LayoutDrag::Column {
                        left,
                        start: (l.share, r.share),
                        x: ev.position.x,
                    });
                    cx.notify();
                }),
            )
            .on_drag(PanelDivider, |_, _, _, cx| cx.new(|_| PanelDivider))
            .on_drag_move(cx.listener(
                move |this, ev: &gpui::DragMoveEvent<PanelDivider>, _, cx| {
                    let Some(LayoutDrag::Column { left: l, start, x }) = this.layout_drag else {
                        return;
                    };
                    if l != left {
                        return;
                    }
                    let width = f32::from(this.grid_bounds.get().size.width).max(1.0);
                    let delta = f32::from(ev.event.position.x - x) / width;
                    this.resize_columns(left, start, delta, cx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.layout_drag = None),
            )
            .into_any_element()
    }

    /// The draggable divider between panels `above` and `above + 1` in layout
    /// column `col` (see [`resize_rows`]).
    fn row_divider(&self, col: usize, above: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .id(("row-divider", col * 100 + above))
            .h(px(5.))
            .flex_none()
            .w_full()
            .cursor_ns_resize()
            .bg(cx.theme().border)
            .hover(|s| s.bg(cx.theme().muted_foreground))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &gpui::MouseDownEvent, _, cx| {
                    let Some(column) = this.config.layout.columns.get(col) else {
                        return;
                    };
                    let (Some(t), Some(b)) =
                        (column.panels.get(above), column.panels.get(above + 1))
                    else {
                        return;
                    };
                    this.layout_drag = Some(LayoutDrag::Row {
                        col,
                        above,
                        start: (t.share, b.share),
                        y: ev.position.y,
                    });
                    cx.notify();
                }),
            )
            .on_drag(PanelDivider, |_, _, _, cx| cx.new(|_| PanelDivider))
            .on_drag_move(cx.listener(
                move |this, ev: &gpui::DragMoveEvent<PanelDivider>, _, cx| {
                    let Some(LayoutDrag::Row {
                        col: c,
                        above: a,
                        start,
                        y,
                    }) = this.layout_drag
                    else {
                        return;
                    };
                    if c != col || a != above {
                        return;
                    }
                    let height = f32::from(this.grid_bounds.get().size.height).max(1.0);
                    let delta = f32::from(ev.event.position.y - y) / height;
                    this.resize_rows(col, above, start, delta, cx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.layout_drag = None),
            )
            .into_any_element()
    }

    fn render_events_panel(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let font_size = self.font_size;
        let hide_msgs = self.config.hide_sub_messages;
        let collapse = self.config.collapse_gift_subs;
        // Virtualized like the log: only on-screen event rows are built per
        // frame. The retained buffer holds up to MAX_EVENTS rows, and building
        // them all each frame (the old plain scroll column) re-laid-out — and
        // kept animating — every off-screen row too, which grew choppier the
        // longer a session ran. Rows come from `events_shown` (this view's
        // filter, applied when each event arrived, not per frame).
        let view = cx.entity();
        let events_list = gpui::list(
            self.events_list_state.clone(),
            move |ix, _window, cx: &mut gpui::App| {
                let this = view.read(cx);
                let model = this.channel.read(cx);
                let Some(&seq) = this.events_shown.get(ix) else {
                    return div().into_any_element();
                };
                let Some(ev) = model.event_at(seq) else {
                    return div().into_any_element();
                };
                // A mass-gift summary can reveal its recipients: the ones sent
                // inline on the announcement (Kick) plus the collapsed
                // per-recipient rows grouped under it (Twitch). Only collapse
                // mode hides those rows, so only it lists them here. The full
                // recipient list is only *materialized* when the row is actually
                // expanded — the collapsed common case only needs to know whether
                // any recipient exists (a cheap `any`), not clone them all, so a
                // gift-heavy panel doesn't rescan the whole buffer every frame.
                let is_summary = ev.details.gift_count.is_some();
                let has_grouped = |group: u64| {
                    model
                        .events
                        .iter()
                        .any(|e| e.group == Some(group) && e.details.recipient.is_some())
                };
                let expandable = is_summary
                    && (!ev.details.recipients.is_empty() || (collapse && has_grouped(seq)));
                let expanded = expandable && this.expanded_gifts.contains(&seq);
                let names = expanded.then(|| {
                    let mut names = ev.details.recipients.clone();
                    if collapse {
                        names.extend(
                            model
                                .events
                                .iter()
                                .filter(|e| e.group == Some(seq))
                                .filter_map(|e| e.details.recipient.clone()),
                        );
                    }
                    names
                });
                let row = render::render_event_compact(
                    render::PanelEvent {
                        platform: ev.platform,
                        kind: ev.kind,
                        text: &ev.text,
                        timestamp: ev.timestamp,
                        details: &ev.details,
                        message: if hide_msgs { None } else { ev.message.as_deref() },
                        expandable,
                        expanded_names: names,
                        mention_click: Some(mention_click_for_platform(&view, ev.platform)),
                    },
                    font_size,
                );
                let wrapper = div().w_full().min_w_0().px(px(6.0));
                if expandable {
                    let view = view.clone();
                    wrapper
                        .id(("event-row", ix))
                        .cursor_pointer()
                        .hover(|s| s.bg(render::row_hover()))
                        .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| {
                            view.update(cx, |this, cx| {
                                if !this.expanded_gifts.remove(&seq) {
                                    this.expanded_gifts.insert(seq);
                                }
                                // The row's height changed: re-measure just it.
                                this.events_list_state.splice(ix..ix + 1, 1);
                                cx.notify();
                            });
                        })
                        .child(row)
                        .into_any_element()
                } else {
                    wrapper.child(row).into_any_element()
                }
            },
        )
        .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
        .size_full();

        // The header stays pinned; only the events list below it scrolls. Same
        // shape as `panel_scroll_list`, but the overlay scrollbar drives the
        // `ListState` instead of a `ScrollHandle`; tailing is the list's native
        // `FollowMode::Tail`.
        let body = div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .size_full()
                    .text_size(px(font_size))
                    .child(events_list),
            )
            // Mounted only while scrolled off the bottom, like the chat log's —
            // tail-follow offset changes otherwise keep the bar visible.
            .when(!self.events_list_state.is_following_tail(), |d| {
                d.vertical_scrollbar(&self.events_list_state)
            })
            .into_any_element();
        self.aux_panel("events-panel", "Events", tabs::PanelKind::Events, body, cx)
    }

    /// The scrollable rows column of an aux panel: a `relative` wrapper holding
    /// the scrolling column plus an absolute scrollbar over the same handle, so
    /// dragging the thumb scrolls the panel. The horizontal inset around the
    /// rows is exactly the visible gap on each side (event pills render `flush`,
    /// no negative-margin bleed — see `render_event`). No scrollbar gutter
    /// (unlike the log, whose right-edge hover buttons must clear the thumb):
    /// the panel rarely overflows, and when it does the thumb overlays the row
    /// edge like a standard overlay scrollbar.
    fn panel_scroll_list(
        &self,
        id: &'static str,
        scroll: &gpui::ScrollHandle,
        rows: Vec<gpui::AnyElement>,
    ) -> gpui::AnyElement {
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .id(id)
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(scroll)
                    .text_size(px(self.font_size))
                    .child(v_flex().gap_1().px(px(6.0)).children(rows)),
            )
            .vertical_scrollbar(scroll)
            .into_any_element()
    }

    /// The shared chrome of an aux panel (events/mentions): a full-size column
    /// on the chat log's (lighter) background — so the panels read as one
    /// surface with it — with the pinned [`panel_header`](Self::panel_header)
    /// above `body`. Horizontal insets live on the rows container (see
    /// [`panel_scroll_list`](Self::panel_scroll_list)) so the scrollbar sits
    /// flush at the panel edge; only the header needs its own left padding.
    fn aux_panel(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        kind: tabs::PanelKind,
        body: gpui::AnyElement,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        v_flex()
            .id(id)
            .size_full()
            .min_h_0()
            .py_2()
            .gap_1()
            .bg(gpui::rgb(render::chat_bg()))
            .child(self.panel_header(label, kind, cx))
            .child(body)
            .into_any_element()
    }

    /// One arrow button of a panel header: moves `kind` one step through the
    /// layout grid. Impossible moves are no-ops in [`tabs::Layout::move_panel`].
    fn move_button(
        &self,
        kind: tabs::PanelKind,
        dir: tabs::MoveDir,
        glyph: &'static str,
        tip: &'static str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        self.header_chip(format!("move-{kind:?}-{dir:?}"), glyph, cx)
            .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.move_panel(kind, dir, cx)),
            )
            .into_any_element()
    }

    /// The shared styling of a panel header's small clickable glyphs (the
    /// move arrows and the remove button).
    fn header_chip(
        &self,
        id: impl Into<SharedString>,
        glyph: &'static str,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id.into())
            .px_1()
            .rounded_sm()
            .cursor_pointer()
            .text_size(px(self.font_size * 0.72))
            .text_color(cx.theme().muted_foreground)
            .hover(|s| s.bg(cx.theme().secondary))
            .child(SharedString::from(glyph))
    }

    /// A panel's pinned header: the muted label plus ◀ ▲ ▼ ▶ buttons that move
    /// the panel through the layout grid (its position persists per tab).
    fn panel_header(
        &self,
        label: impl Into<SharedString>,
        kind: tabs::PanelKind,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        use tabs::MoveDir;
        h_flex()
            .pl_2()
            .pr_1()
            .justify_between()
            .items_center()
            .child(
                div()
                    .text_size(px(self.font_size * 0.72))
                    .text_color(cx.theme().muted_foreground)
                    .child(label.into()),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(self.move_button(kind, MoveDir::Left, "◀", "Move left", cx))
                    .child(self.move_button(kind, MoveDir::Up, "▲", "Move up", cx))
                    .child(self.move_button(kind, MoveDir::Down, "▼", "Move down", cx))
                    .child(self.move_button(kind, MoveDir::Right, "▶", "Move right", cx))
                    // Aux panels can be removed right here (same as unchecking
                    // them in tab settings, where they're re-enabled). Chat can't.
                    .when(kind != tabs::PanelKind::Chat, |h| {
                        h.child(
                            self.header_chip(format!("remove-{kind:?}"), "🗑", cx)
                                .tooltip(|window, cx| {
                                    Tooltip::new("Remove panel (re-enable in tab settings)")
                                        .build(window, cx)
                                })
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, _, cx| {
                                        this.config.layout.set_enabled(kind, false);
                                        this.layout_changed(cx);
                                    }),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }

    /// The mentions panel: messages that mention the user,
    /// rendered like normal chat rows (name click → usercard) in its own
    /// tailing scroll column. Per the tab's "all tabs" setting it shows either
    /// this tab's own mentions (from the log) or the shared all-tabs feed (each
    /// row under a "#channel" tag, click → jump to the source tab).
    fn render_mentions_panel(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let font_size = self.font_size;
        let all_tabs = self.config.mentions_all_tabs;
        let rows: Vec<gpui::AnyElement> = if all_tabs {
            crate::mentions::feed_rows(&self.mention_store, font_size, cx)
        } else {
            let entity = cx.entity();
            // The panel isn't part of the log's drag-select; give its rows their
            // own throwaway selection context (same as the usercard's message list).
            let selection = selectable::Selection::new();
            selection.begin_frame();
            let mut ordinal = 0usize;
            // Gather + render this view's mentions (its own terms) straight out of
            // the shared buffer. `decorate` only clones a message when its author
            // actually has cosmetics (Cow) — the old collect-to-owned pass deep-
            // cloned every mention-matching message on every render of this view.
            let model = self.channel.read(cx);
            model
                .rows
                .iter()
                .filter_map(|row| {
                    // Chat rows and a sub/resub event's attached chatter message
                    // both count as mention sources.
                    let msg: &Message = match row {
                        Row::Message { msg } => msg,
                        Row::Event {
                            message: Some(msg), ..
                        } => msg,
                        _ => return None,
                    };
                    if !self.mentions.matches(&msg.raw_text) {
                        return None;
                    }
                    let struck = model.is_struck(msg);
                    let decorated = log::decorate(msg, model);
                    Some(
                        render::render_message(
                            &decorated,
                            render::RowFlags {
                                struck,
                                mentioned: true,
                                hide_timestamp: !crate::settings::show_timestamps_mentions(),
                                ..Default::default()
                            },
                            font_size,
                            &selection,
                            &mut ordinal,
                            render::RowHandlers {
                                name_click: Some(name_click_for(&entity, msg)),
                                mention_click: Some(mention_click_for(&entity, msg)),
                                ..Default::default()
                            },
                        )
                        .into_any_element(),
                    )
                })
                .collect()
        };
        tail_panel(&mut self.mentions_new, &self.mentions_scroll);

        let body = if rows.is_empty() {
            div()
                .px_2()
                .text_size(px(font_size * 0.85))
                .text_color(cx.theme().muted_foreground)
                .child(SharedString::from("No mentions yet."))
                .into_any_element()
        } else {
            self.panel_scroll_list("mentions-panel-list", &self.mentions_scroll, rows)
        };
        let label = if all_tabs {
            "Mentions — all tabs"
        } else {
            "Mentions"
        };
        self.aux_panel("mentions-panel", label, tabs::PanelKind::Mentions, body, cx)
    }

    /// Builds the tab's panel grid from `config.layout`: columns left-to-right
    /// (draggable dividers between them), each stacking its panels top-to-bottom
    /// (draggable dividers there too). Columns/panels size by `share` via
    /// `flex_grow` on a zero basis, so the fractions hold regardless of content.
    /// A zero-size `canvas` records the grid's bounds for the dividers' px→share
    /// conversion.
    fn render_layout_grid(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut layout = self.config.layout.clone();
        if !layout.contains(tabs::PanelKind::Chat) {
            // Defensive: a config that skipped migration still renders a chat.
            layout.sanitize();
        }

        // The chat log renders through its own **cached child view** (see the
        // [`log`] module): a picker-cell animation tick dirties this view (gpui
        // dirties all ancestors of a notified view), and without the cache that
        // re-render would rebuild every visible log row at the animation rate.
        // The cached node carries the flex participation (`log_view_style`); log
        // content changes dirty it via [`refresh_log`](Self::refresh_log).
        // `Option` because chat appears exactly once (sanitize guarantees it).
        let mut chat_log =
            Some(gpui::AnyView::from(self.log_view.clone()).cached(log::log_view_style()));
        let single_panel = layout.columns.len() == 1 && layout.columns[0].panels.len() == 1;

        let bounds_cell = self.grid_bounds.clone();
        let mut row = h_flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .w_full()
            // `h_flex` defaults to `items_center`, which would vertically center
            // (and visually shrink) the columns — stretch them to full height.
            .items_stretch()
            .relative()
            .child(
                gpui::canvas(move |b, _, _| bounds_cell.set(b), |_, _, _, _| ())
                    .absolute()
                    .size_full(),
            );

        for (ci, column) in layout.columns.iter().enumerate() {
            if ci > 0 {
                row = row.child(self.column_divider(ci - 1, cx));
            }
            let mut col = v_flex().h_full().min_w_0().min_h_0();
            set_share(col.style(), column.share);
            for (ri, panel) in column.panels.iter().enumerate() {
                if ri > 0 {
                    col = col.child(self.row_divider(ci, ri - 1, cx));
                }
                let content: gpui::AnyElement = match panel.kind {
                    tabs::PanelKind::Chat => {
                        let log = chat_log
                            .take()
                            .map(IntoElement::into_any_element)
                            .unwrap_or_else(|| div().into_any_element());
                        // Pinned messages float over the log's top edge (inside
                        // the chat, Twitch-style), so the wrapper is `relative`.
                        // Its hover drives pause-on-hover: the region is the log
                        // only — the composer below is deliberately outside it.
                        let log = div()
                            .id("chat-log-region")
                            .relative()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                                this.set_log_hovered(*hovered, cx);
                            }))
                            .child(log)
                            .children(self.render_pin_overlay(cx))
                            .into_any_element();
                        // The composer lives under the log *inside* this cell,
                        // so with side panels open the input spans the chat
                        // column, not the whole window.
                        let mut col = v_flex().size_full().min_h_0();
                        if !single_panel {
                            // A header like the other panels' (move buttons).
                            col = col
                                .bg(gpui::rgb(render::chat_bg()))
                                .pt_2()
                                .child(self.panel_header("Chat", tabs::PanelKind::Chat, cx))
                                .child(div().h_1());
                        }
                        // Chat restrictions at the top of the panel (below the
                        // header, above the pinned banner that floats over the
                        // log) when the user opted in; otherwise they sit above
                        // the input, inside the composer.
                        if crate::settings::chat_modes_placement()
                            == crate::settings::ChatModesPlacement::Top
                        {
                            col = col.children(self.render_mode_bar(true, cx));
                        }
                        col.child(log)
                            .child(self.render_composer(cx))
                            .into_any_element()
                    }
                    tabs::PanelKind::Events => self.render_events_panel(cx),
                    tabs::PanelKind::Mentions => self.render_mentions_panel(cx),
                };
                let mut cell = div()
                    .w_full()
                    .min_w_0()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(content);
                set_share(cell.style(), panel.share);
                col = col.child(cell);
            }
            row = row.child(col);
        }
        row.into_any_element()
    }

    /// The hover link-preview tooltip: a small card with the video thumbnail,
    /// title, channel, and view count, anchored near the hovered link. `None`
    /// unless a preview is armed and its show-delay elapsed; a still-loading
    /// fetch shows a compact "Loading…" card so the hover feels responsive.
    /// Passive (no click backdrop, non-occluding) — a tooltip dismissed by
    /// moving off the link.
    fn render_link_preview(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let hover = self.link_preview.as_ref()?;
        if !hover.shown {
            return None;
        }
        #[allow(clippy::type_complexity)]
        let (title, author, stats, byline, thumbnail): (
            SharedString,
            SharedString,
            SharedString,
            SharedString,
            Option<SharedString>,
        ) = match crate::preview::peek(&hover.url) {
            crate::preview::PreviewState::Ready(p) => (
                SharedString::from(p.title.clone()),
                SharedString::from(p.author.clone()),
                SharedString::from(p.stats.clone().unwrap_or_default()),
                SharedString::from(p.byline.clone().unwrap_or_default()),
                // Streamer mode can hide the thumbnail (it can reveal what a link
                // points at on stream); the rest of the card still shows.
                if crate::settings::hide_preview_thumbnails() {
                    None
                } else {
                    p.thumbnail_url.clone().map(SharedString::from)
                },
            ),
            crate::preview::PreviewState::Loading => (
                SharedString::from("Loading preview…"),
                SharedString::default(),
                SharedString::default(),
                SharedString::default(),
                None,
            ),
            // The fetch failed (or turned out unsupported) — show nothing.
            crate::preview::PreviewState::None => return None,
        };

        let viewport = window.viewport_size();
        const CARD_W: f32 = 240.;
        const GAP: f32 = 10.;
        let thumb_h = if thumbnail.is_some() { CARD_W * 9. / 16. } else { 0. };
        // The muted meta pieces: "channel · views" and the clip's "Clipped by X".
        let channel_views = [author, stats]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        // Decide whether the byline fits on the *same* line as channel/views: if so
        // it joins with a "·" separator; if not it goes on its own line with no
        // separator (nothing to separate). Static flex can't detect a wrap, so we
        // estimate the one-line width (~5.5px per char at 11px) against the card's
        // inner width. `meta_two_lines` then drives both the layout and the height.
        let one_line = if channel_views.is_empty() {
            byline.to_string()
        } else if byline.is_empty() {
            channel_views.clone()
        } else {
            format!("{channel_views} · {byline}")
        };
        let has_meta = !one_line.is_empty();
        const META_INNER_W: f32 = CARD_W - 12.; // minus p_1p5 left+right
        let meta_two_lines =
            !channel_views.is_empty() && !byline.is_empty() && one_line.chars().count() as f32 * 5.5 > META_INNER_W;
        // The card is top-anchored (grows downward), so the above-the-link gap
        // depends on estimating its height. Budget two title lines; the meta is one
        // or two lines by the decision above. An over-estimate only lifts the card
        // slightly higher — harmless — while an under-estimate would let the real
        // bottom overlap the link.
        let title_h = 40.; // up to two title lines
        let meta_h = if !has_meta {
            0.
        } else if meta_two_lines {
            32.
        } else {
            16.
        };
        let children = 1 + u32::from(thumbnail.is_some()) + u32::from(has_meta);
        let card_h = 12. // p_1p5 top+bottom
            + 4. * children.saturating_sub(1) as f32 // gap_1 between children
            + thumb_h
            + title_h
            + meta_h;

        let vw = f32::from(viewport.width);
        let vh = f32::from(viewport.height);
        let anchor_x = f32::from(hover.anchor.x);
        let anchor_y = f32::from(hover.anchor.y);
        // Center the card horizontally on the pointer, clamped to the viewport.
        let x = (anchor_x - CARD_W / 2.).min(vw - CARD_W - 8.).max(8.);
        // The pointer sits mid-glyph, so the link occupies roughly half a chat
        // line above and below `anchor_y`. Prefer the card above the link (its
        // bottom GAP px above the link's top); flip below only when it won't fit.
        let half_line = self.font_size * 0.9;
        let link_top = anchor_y - half_line;
        let link_bottom = anchor_y + half_line;
        let top = if link_top - GAP - card_h >= 8. {
            link_top - GAP - card_h
        } else {
            (link_bottom + GAP).min(vh - card_h - 8.).max(8.)
        };
        // `top`/`x` are window coords (like `anchor`), but the overlay layer's
        // local origin sits below the window top — gpui offsets an `absolute`
        // overlay that's a flow child of the non-`relative` root by the chrome
        // stacked above it. Subtract that measured offset (see
        // `link_preview_offset`) so the card lands where `anchor` says.
        let offset = self.link_preview_offset.get();
        let local_top = top - f32::from(offset.y);
        let local_x = x - f32::from(offset.x);

        let mut card = v_flex()
            .absolute()
            .left(px(local_x))
            .top(px(local_top))
            .w(px(CARD_W))
            .p_1p5()
            .gap_1()
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .rounded_lg()
            .shadow_lg()
            .text_color(cx.theme().popover_foreground);
        // The thumbnail is a static image (not animated) → plain img().
        if let Some(thumb) = thumbnail {
            card = card.child(
                img(thumb)
                    .w_full()
                    .h(px(thumb_h))
                    .rounded_md()
                    .object_fit(gpui::ObjectFit::Cover),
            );
        }
        card = card.child(
            div()
                .font_weight(FontWeight::MEDIUM)
                .text_size(px(13.))
                .line_height(px(16.))
                .child(title),
        );
        if has_meta {
            let muted = cx.theme().muted_foreground;
            let meta_line = |text: SharedString| {
                div().text_size(px(11.)).text_color(muted).child(text)
            };
            if meta_two_lines {
                // Doesn't fit on one line → "channel · views" then "Clipped by X"
                // on its own line, with no separator between them.
                card = card.child(meta_line(SharedString::from(channel_views)));
                card = card.child(meta_line(byline));
            } else {
                // Fits on one line → a single "channel · views · Clipped by X".
                card = card.child(meta_line(SharedString::from(one_line)));
            }
        }

        Some(gpui::deferred(div().absolute().inset_0().child(card)).into_any_element())
    }

    /// A zero-cost element that continuously records the overlay layer's
    /// window-space origin into [`link_preview_offset`]. Rendered *unconditionally*
    /// (see [`render`](Self::render)) so the offset is always warm — otherwise the
    /// first preview would paint one frame at the wrong spot (offset still `(0,0)`)
    /// before the measure landed. Matches the `inset_0` layer the card renders in.
    fn link_preview_probe(&self) -> gpui::AnyElement {
        let offset_cell = self.link_preview_offset.clone();
        div()
            .absolute()
            .inset_0()
            .child(
                gpui::canvas(move |b, _, _| offset_cell.set(b.origin), |_, _, _, _| ())
                    .absolute()
                    .size_full(),
            )
            .into_any_element()
    }

    /// The emote-info popup overlay when one is open, else `None`. A transparent
    /// full-window backdrop (behind the card) closes it on any outside click; the
    /// card itself is `occlude`d so clicking it doesn't close it.
    fn render_emote_popup(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let popup = self.emote_popup.as_ref()?;
        let viewport = window.viewport_size();
        // Clamp the anchor so the (fixed-ish) card stays on-screen.
        const CARD_W: f32 = 260.;
        const CARD_H: f32 = 200.;
        // Gap between the clicked emote and the card so the emote stays visible.
        const GAP: f32 = 20.;
        let vw = f32::from(viewport.width);
        let vh = f32::from(viewport.height);
        let anchor_x = f32::from(popup.anchor.x);
        let anchor_y = f32::from(popup.anchor.y);
        let x = anchor_x.min(vw - CARD_W).max(8.);
        // Prefer opening below the click; if there isn't room (the emote sits low
        // in the log), flip the card above it so it isn't clipped or covering the
        // emote we clicked. Clamp to the viewport either way.
        let y = if anchor_y + GAP + CARD_H <= vh - 8. {
            anchor_y + GAP
        } else {
            (anchor_y - GAP - CARD_H).max(8.)
        };

        let mut card = v_flex()
            .occlude()
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(CARD_W))
            .p_3()
            .gap_2()
            .items_center()
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .rounded_lg()
            .shadow_lg()
            .text_color(cx.theme().popover_foreground)
            // The image is absent while a clicked link is still resolving.
            .when(!popup.url.is_empty(), |c| {
                c.child(crate::animated_img::animated_img(
                    "emote-popup-img",
                    popup.url.clone(),
                    px(64.),
                ))
            })
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .text_size(px(15.))
                    .child(popup.name.clone()),
            );
        if !popup.provider.is_empty() {
            card = card.child(
                div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child(popup.provider.clone()),
            );
        }
        if !popup.author.is_empty() {
            card = card.child(
                div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child(popup.author.clone()),
            );
        }
        if let Some(url) = popup.seventv_url.clone() {
            card = card.child(
                Button::new("emote-popup-7tv")
                    .label("Open on 7TV ↗")
                    .outline()
                    .small()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.open_url(&url);
                        this.emote_popup = None;
                        cx.notify();
                    })),
            );
        }

        // The backdrop fills the window behind the card; a click anywhere on it
        // (i.e. outside the occluded card) closes the popup.
        Some(
            div()
                .absolute()
                .inset_0()
                .child(card)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.emote_popup = None;
                        cx.notify();
                    }),
                )
                .into_any_element(),
        )
    }

    /// The "Replying to" bar above the input when a reply is pending. When the
    /// message being replied to is part of a multi-message thread that's still in
    /// the buffer, the bar shows the whole chain (scrollable, oldest first) so the
    /// user sees the conversation they're joining; the message they're replying to
    /// is tinted. Otherwise it's a single "Replying to name: preview" line. An ✕
    /// cancels. `None` when not replying.
    fn render_reply_bar(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        // Copy the reply's fields so the rest of the method can take `&mut self`
        // (the chain scroll needs it) without holding a borrow of `replying_to`.
        let reply = self.replying_to.as_ref()?;
        let message_id = reply.message_id.clone();
        let parent_author = reply.parent.author.clone();
        let parent_elements = reply.parent_elements.clone();
        let reply_platform = reply.platform;
        // A stable per-reply id seed so the preview's emote images animate.
        let seed = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            message_id.hash(&mut h);
            h.finish()
        };

        let font_size = self.font_size;
        let accent = gpui::rgb(render::palette().reply);
        let label_size = font_size * 0.82;

        // A small circular ✕ that lifts on hover — a cleaner dismiss than a bare
        // glyph, matching the app's chrome buttons.
        let cancel = div()
            .id("cancel-reply")
            .flex_none()
            .size(px(font_size + 6.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .cursor_pointer()
            .text_color(cx.theme().muted_foreground)
            .text_size(px(font_size * 0.85))
            .hover(|s| {
                s.bg(render::chrome_hover())
                    .text_color(cx.theme().foreground)
            })
            .child(SharedString::from("✕"))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.replying_to = None;
                    this.scroll_to_newest.reply_chain = false;
                    cx.notify();
                }),
            );

        // The small reply icon + "Replying to <name>" caption, shared by both the
        // single-line bar and the thread header. `name` is emphasized in the reply
        // accent; the lead-in is muted and smaller. When a name is shown it's
        // clickable (opens the chatter's usercard on the reply's platform).
        let entity = cx.entity();
        let caption = |lead: &str, name: Option<SharedString>, fg: gpui::Hsla| {
            h_flex()
                .flex_none()
                .items_center()
                .gap_1p5()
                .child(
                    gpui::svg()
                        .path("icons/reply.svg")
                        .size(px(font_size * 0.9))
                        .flex_none()
                        .text_color(accent),
                )
                .child(
                    div()
                        .text_size(px(label_size))
                        .text_color(fg)
                        .child(SharedString::from(lead.to_string())),
                )
                .when_some(name, |row, name| {
                    let entity = entity.clone();
                    let target = name.to_string();
                    row.child(
                        div()
                            .id("reply-bar-name")
                            .text_size(px(label_size))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(accent)
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .child(name)
                            .on_mouse_down(
                                MouseButton::Left,
                                move |_, _window: &mut Window, cx: &mut App| {
                                    let name = target.clone();
                                    entity.update(cx, |this, cx| {
                                        this.open_usercard_named(&name, reply_platform, cx);
                                        cx.notify();
                                    });
                                },
                            ),
                    )
                })
        };

        // The modern reply-bar shell: a rounded, elevated pill inset from the
        // composer edges, with a reply-accent bar down its left edge — reads as a
        // distinct, dismissible affordance rather than a flat full-bleed strip.
        let shell = |body: gpui::AnyElement| {
            v_flex()
                .w_full()
                .mx_2()
                .mt_1()
                .rounded_lg()
                .bg(gpui::rgb(render::panel_bg()))
                .border_l_2()
                .border_color(accent)
                .overflow_hidden()
                .child(body)
        };

        // Reconstruct the thread the target belongs to; show the whole chain when
        // it's a real conversation (>1 message still buffered).
        let thread = self.build_thread(&message_id, cx);
        let show_chain = thread.as_ref().is_some_and(|t| t.is_multi());

        if let (true, Some(thread)) = (show_chain, thread) {
            let muted = cx.theme().muted_foreground;
            let entity = cx.entity();
            let lines: Vec<gpui::AnyElement> = thread
                .messages
                .iter()
                .map(|m| {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    m.id.hash(&mut h);
                    render::render_thread_line(
                        m,
                        font_size,
                        h.finish(),
                        m.id == message_id,
                        Some(name_click_for(&entity, m)),
                    )
                    .into_any_element()
                })
                .collect();
            // Open the chain at the newest message, older ones scrollable above.
            let scroll = self.reply_chain_scroll.clone();
            self.open_at_bottom(ScrollTarget::ReplyChain, &scroll, cx);
            let count = thread.len();
            let body = v_flex()
                .w_full()
                .child(
                    h_flex()
                        .w_full()
                        .px_2()
                        .py_1()
                        .items_center()
                        .justify_between()
                        .child(caption("Replying in thread", None, muted).child(
                            div()
                                .text_size(px(label_size))
                                .text_color(muted)
                                .child(SharedString::from(format!("· {count}"))),
                        ))
                        .child(cancel),
                )
                .child(
                    v_flex()
                        .id("reply-thread-chain")
                        .w_full()
                        .px_2()
                        .pb_1p5()
                        .gap_0p5()
                        // Cap the chain's height so a long thread scrolls rather
                        // than shoving the composer off-screen.
                        .max_h(px(140.))
                        .overflow_y_scroll()
                        .track_scroll(&scroll)
                        .children(lines),
                )
                .into_any_element();
            return Some(shell(body).into_any_element());
        }

        let muted = cx.theme().muted_foreground;
        let body = h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .child(caption("Replying to", Some(SharedString::from(parent_author)), muted))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_size(px(label_size))
                    .text_color(muted)
                    .child(render::render_reply_preview(&parent_elements, font_size, seed)),
            )
            .child(cancel)
            .into_any_element();
        Some(shell(body).into_any_element())
    }

    /// Reconstructs the thread seeded from `seed_id` off the live buffer, memoized
    /// by `(seed_id, rows_generation)` so the reply bar + thread panel (which call
    /// this every render — per keystroke while replying) don't re-scan the whole
    /// buffer unless the seed or the rows actually changed. `None` if the seed row
    /// is gone.
    fn build_thread(&self, seed_id: &str, cx: &App) -> Option<crate::thread::Thread> {
        let model = self.channel.read(cx);
        let generation = model.rows_generation();
        // Serve from cache when the seed + buffer are unchanged.
        if let Some((cached_seed, cached_gen, thread)) = self.thread_cache.borrow().as_ref() {
            if cached_seed == seed_id && *cached_gen == generation {
                return Some(thread.clone());
            }
        }
        let messages = model.rows.iter().filter_map(|row| match row {
            Row::Message { msg } => Some(msg),
            _ => None,
        });
        let thread = crate::thread::reconstruct(messages, seed_id);
        // Cache the result (only a successful reconstruction — a missing seed is
        // transient and cheap to retry, and caching `None` would need its own key).
        if let Some(t) = &thread {
            *self.thread_cache.borrow_mut() = Some((seed_id.to_string(), generation, t.clone()));
        }
        thread
    }

    /// The thread panel: a floating card listing the whole reply chain the clicked
    /// message belongs to, oldest at top, each row's name/`@mentions` opening the
    /// usercard. It's anchored just **above the "replying to" line the user
    /// clicked** (flipping below only when it won't fit), not centered — a
    /// full-window transparent layer catches an outside click to dismiss. The
    /// header has an ✕ and a "Reply to thread" button that replies to the message
    /// the panel was opened on (the seed). `None` when the panel is closed or its
    /// seed scrolled out of the buffer.
    fn render_thread_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let (seed_id, anchor) = self
            .thread_panel
            .as_ref()
            .map(|t| (t.seed_id.clone(), t.anchor))?;
        let thread = self.build_thread(&seed_id, cx)?;
        let entity = cx.entity();

        // Each message renders against a throwaway selection (the panel isn't part
        // of the log's drag-select); names + mentions open usercards like the log.
        let selection = selectable::Selection::new();
        selection.begin_frame();
        let mut ordinal = 0usize;
        let font_size = self.font_size;

        // Read the shared model once to snapshot which of this thread's messages
        // are struck (ban/delete), then drop the borrow — the render loop below
        // doesn't touch the model, so it can't re-borrow it per row.
        let struck_ids: std::collections::HashSet<&str> = {
            let model = self.channel.read(cx);
            thread
                .messages
                .iter()
                .filter(|m| model.is_struck(m))
                .map(|m| m.id.as_str())
                .collect()
        };
        let rows: Vec<gpui::AnyElement> = thread
            .messages
            .iter()
            .map(|msg| {
                let is_seed = msg.id == seed_id;
                let handlers = render::RowHandlers {
                    name_click: Some(name_click_for(&entity, msg)),
                    mention_click: Some(mention_click_for(&entity, msg)),
                    ..Default::default()
                };
                let struck = struck_ids.contains(msg.id.as_str());
                let body = render::render_message(
                    msg,
                    render::RowFlags {
                        struck,
                        ..Default::default()
                    },
                    font_size,
                    &selection,
                    &mut ordinal,
                    handlers,
                );
                let mut row = div().w_full().px_2().py_0p5().child(body);
                if is_seed {
                    // The message the user clicked from: a subtle tint + accent bar
                    // so it's findable in a long thread (tint only, not the text).
                    row = row
                        .bg(cx.theme().secondary)
                        .rounded_md()
                        .border_l_2()
                        .border_color(cx.theme().primary);
                }
                row.into_any_element()
            })
            .collect();

        let count = thread.len();
        // "Reply to thread" replies to the message the panel was opened on (the
        // one the user clicked), not the newest — the resulting thread is the same
        // either way (every member shares the root), and replying to the message
        // in view matches intent. Falls back to the newest if the seed somehow
        // isn't in the rebuilt chain.
        let reply_target = thread
            .messages
            .iter()
            .find(|m| m.id == seed_id)
            .or_else(|| thread.messages.last())
            .map(|m| m.id.clone());

        let header = h_flex()
            .w_full()
            .px_3()
            .py_2()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(cx.theme().foreground)
                    .child(SharedString::from(format!("Thread ({count})"))),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .when_some(reply_target, |row, target| {
                        row.child(
                            div()
                                .id("thread-reply")
                                .px_2()
                                .py_0p5()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(cx.theme().secondary)
                                .text_color(cx.theme().secondary_foreground)
                                .text_size(px(font_size - 1.))
                                .child(SharedString::from("↩ Reply to thread"))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        this.close_thread_panel(cx);
                                        this.start_reply(&target, window, cx);
                                    }),
                                ),
                        )
                    })
                    .child(
                        div()
                            .id("thread-close")
                            .px_1()
                            .cursor_pointer()
                            .text_color(cx.theme().muted_foreground)
                            .child(SharedString::from("✕"))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.close_thread_panel(cx)),
                            ),
                    ),
            );

        // Fixed card box so its placement above the click can be computed. The
        // list inside scrolls when the thread is taller than the body budget.
        const CARD_W: f32 = 440.;
        const CARD_H: f32 = 360.;
        const GAP: f32 = 8.;
        let card = v_flex()
            .id("thread-panel")
            .w(px(CARD_W))
            .h(px(CARD_H))
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .rounded_lg()
            .shadow_lg()
            .text_color(cx.theme().popover_foreground)
            .text_size(px(font_size))
            .child(header)
            .child(
                v_flex()
                    .id("thread-panel-list")
                    .flex_1()
                    .min_h_0()
                    .py_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.thread_scroll)
                    .children(rows),
            );

        // Open at the newest message: snap to the bottom on the first render(s)
        // after opening, until the list has laid out and the scroll takes (a
        // just-built list reports max_offset 0 for a frame).
        let scroll = self.thread_scroll.clone();
        self.open_at_bottom(ScrollTarget::Panel, &scroll, cx);

        // Place the card above the clicked line (its bottom GAP px above the
        // click), flipping below only when it won't fit; clamp to the viewport.
        // Same window→overlay-local conversion as the link-preview card: this
        // renders in the same `inset_0` layer, so it reuses `link_preview_offset`
        // (kept warm by the always-on probe).
        let viewport = window.viewport_size();
        let vw = f32::from(viewport.width);
        let vh = f32::from(viewport.height);
        let ax = f32::from(anchor.x);
        let ay = f32::from(anchor.y);
        // Center horizontally on the click, clamped into the viewport.
        let x = (ax - CARD_W / 2.).min(vw - CARD_W - 8.).max(8.);
        // The click sits mid-line; the line is ~one chat line tall.
        let half_line = self.font_size * 0.9;
        let line_top = ay - half_line;
        let line_bottom = ay + half_line;
        let top = if line_top - GAP - CARD_H >= 8. {
            line_top - GAP - CARD_H
        } else {
            (line_bottom + GAP).min(vh - CARD_H - 8.).max(8.)
        };
        let offset = self.link_preview_offset.get();
        let local_x = x - f32::from(offset.x);
        let local_top = top - f32::from(offset.y);

        // A full-window transparent layer catches an outside click to dismiss; the
        // card sits on top (its own clicks occluded so they don't dismiss).
        let layer = div()
            .id("thread-backdrop")
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.close_thread_panel(cx)),
            )
            .child(
                div()
                    .absolute()
                    .left(px(local_x))
                    .top(px(local_top))
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_, _, _| {})
                    .child(card),
            );

        Some(gpui::deferred(layer).into_any_element())
    }
}

impl ChatView {
    /// The usercard window's content: the account header, the mod-action rows,
    /// and the chatter's recent messages.
    fn usercard_body(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(card) = &self.usercard else {
            return gpui::Empty.into_any_element();
        };
        // Streamer mode hides the avatar behind a placeholder; clicking it
        // reveals this card's avatar (state lives here, on the host).
        let reveal = cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
            if let Some(card) = &mut this.usercard {
                card.avatar_revealed = true;
                cx.notify();
            }
        });
        let header = card.header(reveal, cx);
        let actions = self.usercard_actions(cx);
        let messages = self.usercard_message_list(cx);
        // The window is bare (no built-in scroll surface): header + actions stay
        // fixed and the recent-messages section fills and scrolls the rest.
        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .child(header)
            .child(actions)
            .child(messages)
            .into_any_element()
    }

    /// The moderation panel: a compact "Timeout" chip row plus Ban/Unban (and, on
    /// Twitch, a Warn reason box and Mod/VIP grant toggles). Built to fit the card's default width — small
    /// chips that wrap rather than a single overflowing row. Shown only when the
    /// logged-in user can moderate the card's platform: Twitch needs `twitch_mod`;
    /// Kick needs real mod status too (resolved from the logged-in account's own
    /// usercard; its API has ban/timeout/unban, no role grants).
    fn usercard_actions(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(card) = &self.usercard else {
            return div().into_any_element();
        };
        let platform = card.platform;
        let can_moderate = self.channel.read(cx).can_moderate(platform);
        if !can_moderate {
            return div().into_any_element();
        }
        // The target broadcaster can't be banned/timed out or granted a role at
        // all; a target moderator can't be banned/timed out until they're
        // unmodded. Hide the buttons that would always fail rather than show them
        // dead. Role grants (mod/VIP) also need a *broadcaster* token, so only the
        // channel owner sees them — a plain moderator can't add/remove mod or VIP.
        let is_broadcaster = card.is_broadcaster;
        let show_ban_timeout = !is_broadcaster && !card.is_moderator;
        let show_roles = !is_broadcaster && self.channel.read(cx).twitch_broadcaster;
        let login = card.login.clone();

        // The user's own custom mod buttons (Settings → Mod Buttons), filtered
        // to this card's platform (scope Both/None or a matching platform, and
        // supported on it) and to those that act on a *user* — the card has no
        // message, so "/delete"/`{msg-id}` buttons are skipped. Labeled with the
        // button's name; each runs its template against this login. These show
        // even for a mod/broadcaster target (a bot shoutout isn't a ban).
        let custom_buttons: Vec<gpui::AnyElement> = crate::settings::mod_buttons()
            .iter()
            .filter(|b| b.platform.is_none_or(|p| p == platform))
            .filter(|b| commands::supported_on(&b.command, platform))
            .filter(|b| commands::targets_user(&b.command))
            .enumerate()
            .map(|(i, b)| {
                let label = if b.name.is_empty() {
                    b.command.clone()
                } else {
                    b.name.clone()
                };
                let command = b.command.clone();
                let to_login = login.clone();
                Button::new(SharedString::from(format!("usercard-custom-{i}")))
                    .label(SharedString::from(label))
                    .outline()
                    .xsmall()
                    .compact()
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.run_usercard_mod_button(&command, &to_login, platform);
                    }))
                    .into_any_element()
            })
            .collect();
        let custom_buttons_row = (!custom_buttons.is_empty())
            .then(|| h_flex().w_full().flex_wrap().gap_1().children(custom_buttons));

        if !show_ban_timeout && !show_roles && custom_buttons_row.is_none() {
            return div().into_any_element();
        }

        // (label, seconds) timeout presets — Chatterino's spread, through the
        // full 2-week Twitch cap; presets over the platform's cap are dropped
        // (Kick tops out at 7 days).
        const PRESETS: &[(&str, u32)] = &[
            ("1s", 1),
            ("1m", 60),
            ("10m", 600),
            ("30m", 1800),
            ("1h", 3600),
            ("4h", 14400),
            ("1d", 86400),
            ("3d", 259_200),
            ("1w", 604_800),
            ("2w", 1_209_600),
        ];
        let max = max_timeout_secs(platform);

        let timeout_chips = h_flex()
            .w_full()
            .flex_wrap()
            .gap_1()
            .children(
                PRESETS
                    .iter()
                    .filter(|(_, secs)| *secs <= max)
                    .map(|(label, secs)| {
                        let secs = *secs;
                        let to_login = login.clone();
                        Button::new(SharedString::from(format!("usercard-to-{label}")))
                            .label(*label)
                            .outline()
                            .xsmall()
                            .compact()
                            .on_click(cx.listener(move |this, _, _, _| {
                                this.usercard_moderate(platform, Mod::Timeout(secs), &to_login);
                            }))
                    }),
            );

        // The custom-duration row: a small parse-anything box ("90s", "1h30m",
        // "3d") applied by Enter or its button. The input only exists while the
        // usercard window is open (it's bound to it).
        let custom_row = self.usercard_timeout_input.as_ref().map(|input| {
            h_flex()
                .w_full()
                .gap_1()
                .items_center()
                .child(div().flex_1().child(Input::new(input).small()))
                .child(
                    Button::new("usercard-to-custom")
                        .label("Timeout")
                        .outline()
                        .small()
                        .compact()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.apply_custom_timeout(window, cx);
                        })),
                )
        });
        let custom_error = self.usercard_timeout_error.as_ref().map(|err| {
            div()
                .text_size(px(12.))
                .text_color(cx.theme().danger)
                .child(SharedString::from(err.clone()))
        });

        // Warn (Twitch-only — Kick has no warn API): a reason box + button;
        // Helix requires the reason, and the chatter must acknowledge the
        // warning before they can chat again. Applied by Enter or the button.
        let warn_row = (platform == bks_core::Platform::Twitch)
            .then_some(self.usercard_warn_input.as_ref())
            .flatten()
            .map(|input| {
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .child(div().flex_1().child(Input::new(input).small()))
                    .child(
                        Button::new("usercard-warn")
                            .label("Warn")
                            .outline()
                            .small()
                            .compact()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.apply_usercard_warn(window, cx);
                            })),
                    )
            });
        let warn_error = self.usercard_warn_error.as_ref().map(|err| {
            div()
                .text_size(px(12.))
                .text_color(cx.theme().danger)
                .child(SharedString::from(err.clone()))
        });

        // Ban + Unban, always present for both platforms.
        let ban_login = login.clone();
        let ban = Button::new("usercard-ban")
            .label("Ban")
            .danger()
            .xsmall()
            .compact()
            .on_click(cx.listener(move |this, _, _, _| {
                this.usercard_moderate(platform, Mod::Ban, &ban_login);
            }));
        let unban_login = login.clone();
        let unban = Button::new("usercard-unban")
            .label("Unban")
            .outline()
            .xsmall()
            .compact()
            .on_click(cx.listener(move |this, _, _, _| {
                this.usercard_moderate(platform, Mod::Unban, &unban_login);
            }));

        // Role grants are Twitch-only (Kick's public API can't add/remove mod/VIP).
        // Both directions are shown as separate buttons (Mod/Unmod, VIP/Unvip) so
        // the action never depends on (possibly stale) detected role state.
        let roles = (show_roles && platform == bks_core::Platform::Twitch).then(|| {
            let role_btn = |id: &'static str,
                            label: &'static str,
                            role: controller::Role,
                            grant: bool,
                            login: SharedString| {
                Button::new(id)
                    .label(label)
                    .outline()
                    .xsmall()
                    .compact()
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.controller
                            .set_role_twitch(role, grant, login.to_string());
                    }))
            };
            let login = SharedString::from(login.clone());
            h_flex()
                .gap_1()
                .child(role_btn(
                    "usercard-mod",
                    "Mod",
                    controller::Role::Moderator,
                    true,
                    login.clone(),
                ))
                .child(role_btn(
                    "usercard-unmod",
                    "Unmod",
                    controller::Role::Moderator,
                    false,
                    login.clone(),
                ))
                .child(role_btn(
                    "usercard-vip",
                    "VIP",
                    controller::Role::Vip,
                    true,
                    login.clone(),
                ))
                .child(role_btn(
                    "usercard-unvip",
                    "Unvip",
                    controller::Role::Vip,
                    false,
                    login.clone(),
                ))
        });

        // A compact, sectioned panel: the timeout chips + custom-duration row,
        // a Ban/Unban row, (Twitch broadcaster only) a "Role" row, and the
        // user's custom mod buttons.
        let section_label = |text: &'static str| {
            div()
                .text_size(px(11.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(cx.theme().muted_foreground)
                .child(SharedString::from(text))
        };
        v_flex()
            .w_full()
            .gap_2()
            .p_3()
            .rounded_md()
            .bg(cx.theme().secondary)
            .when(show_ban_timeout, |col| {
                col.child(
                    v_flex()
                        .w_full()
                        .gap_1()
                        .child(section_label("Timeout"))
                        .child(timeout_chips)
                        .when_some(custom_row, |col, row| col.child(row))
                        .when_some(custom_error, |col, err| col.child(err)),
                )
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap_1()
                        .child(ban)
                        .child(unban),
                )
                .when_some(warn_row, |col, row| {
                    col.child(
                        v_flex()
                            .w_full()
                            .gap_1()
                            .child(section_label("Warn"))
                            .child(row)
                            .when_some(warn_error, |col, err| col.child(err)),
                    )
                })
            })
            .when_some(roles, |col, role_row| {
                col.child(
                    v_flex()
                        .w_full()
                        .gap_1()
                        .child(section_label("Role"))
                        .child(role_row.w_full().flex_wrap()),
                )
            })
            .when_some(custom_buttons_row, |col, row| {
                col.child(
                    v_flex()
                        .w_full()
                        .gap_1()
                        .child(section_label("Custom"))
                        .child(row),
                )
            })
            .into_any_element()
    }

    /// Routes a usercard moderation action to the controller for the card's
    /// platform (Twitch or Kick). Centralizes the per-platform dispatch so the
    /// chip handlers don't each branch on the platform.
    fn usercard_moderate(&self, platform: bks_core::Platform, action: Mod, login: &str) {
        let c = &self.controller;
        let login = login.to_string();
        match (platform, action) {
            (bks_core::Platform::Twitch, Mod::Ban) => c.ban_twitch(login),
            (bks_core::Platform::Twitch, Mod::Timeout(s)) => c.timeout_twitch(login, s),
            (bks_core::Platform::Twitch, Mod::Unban) => c.unban_twitch(login),
            (bks_core::Platform::Kick, Mod::Ban) => c.ban_kick(login),
            (bks_core::Platform::Kick, Mod::Timeout(s)) => c.timeout_kick(login, s),
            (bks_core::Platform::Kick, Mod::Unban) => c.unban_kick(login),
            _ => {}
        }
    }

    /// The card's past-message list: the chatter's recent messages in this feed,
    /// rendered the same way as the main log (no mod buttons, no selection).
    fn usercard_message_list(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(card) = &self.usercard else {
            return div().into_any_element();
        };
        let login = card.login.clone();
        let msgs = self.usercard_messages(&login, cx);

        let content = if msgs.is_empty() {
            div()
                .text_size(px(13.))
                .text_color(cx.theme().muted_foreground)
                .child(SharedString::from("No recent messages in this channel."))
                .into_any_element()
        } else {
            // A throwaway selection + ordinal: the card's messages aren't part of
            // the log's drag-select, so they get their own (unused) selection
            // context.
            let selection = selectable::Selection::new();
            selection.begin_frame();
            let mut ordinal = 0usize;
            let rows: Vec<gpui::AnyElement> = msgs
                .iter()
                .map(|msg| {
                    render::render_message(
                        msg,
                        render::RowFlags::default(),
                        self.font_size,
                        &selection,
                        &mut ordinal,
                        render::RowHandlers::default(),
                    )
                    .into_any_element()
                })
                .collect();
            div()
                .id("usercard-messages")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .child(v_flex().gap_1().children(rows))
                .into_any_element()
        };

        // Fills whatever height the window leaves under the header + actions;
        // only this section scrolls.
        v_flex()
            .flex_1()
            .min_h(px(0.))
            .gap_1()
            .pt_2()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .text_size(px(11.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from(format!(
                        "Recent messages ({})",
                        msgs.len()
                    ))),
            )
            .child(content)
            .into_any_element()
    }
}

impl Render for ChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A tab with no channel set: prompt to configure it instead of an empty log.
        if !self.config.has_channel() {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .bg(cx.theme().background)
                .text_color(cx.theme().muted_foreground)
                .child(SharedString::from("This tab has no channel set."))
                .child(SharedString::from(
                    "Right-click the tab → Settings to choose a Twitch or Kick channel.",
                ));
        }

        // Keep the input's placeholder naming where a message will go ("Send a
        // message to Twitch + Kick"), guarded so the (notifying) setter only
        // runs on a real change (send-target cycle, login flip).
        let placeholder = self.composer_placeholder();
        if placeholder != self.input_placeholder {
            self.input_placeholder = placeholder.clone();
            self.input
                .update(cx, |state, cx| state.set_placeholder(placeholder, window, cx));
        }

        let emote_popup_overlay = self.render_emote_popup(window, cx);
        let link_preview_overlay = self.render_link_preview(window, cx);
        let thread_panel_overlay = self.render_thread_panel(window, cx);

        let layout_grid = self.render_layout_grid(cx);

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            // Live viewer-count status bar above the panels; pinned messages
            // float *inside* the chat log (see `render_pin_overlay`) and the
            // composer sits inside the chat panel (see `render_composer`).
            .children(self.render_status_bar(cx))
            .child(layout_grid)
            .children(emote_popup_overlay)
            // Always-on probe keeps the preview offset warm so the first tooltip
            // paints in the right place (no one-frame flash); the card overlay
            // only renders when a preview is showing.
            .child(self.link_preview_probe())
            .children(link_preview_overlay)
            .children(thread_panel_overlay)
    }
}

impl ChatView {
    /// The input's placeholder text: names where a plain message goes, from the
    /// tab's channels + logins + send target. Not logged in anywhere → a login
    /// hint instead. The leading space keeps the muted text clear of the caret,
    /// which the kit blinks exactly on the first glyph's left edge.
    fn composer_placeholder(&self) -> String {
        let has_twitch = !self.config.twitch_channel.trim().is_empty();
        let has_kick = !self.config.kick_channel.trim().is_empty();
        let twitch = has_twitch && self.controller.twitch_logged_in();
        let kick = has_kick && self.controller.kick_logged_in();
        let text = match (twitch, kick) {
            (true, true) => match self.controller.send_target() {
                controller::SendTarget::Twitch => "Send a message to Twitch",
                controller::SendTarget::Kick => "Send a message to Kick",
                controller::SendTarget::Both => "Send a message to Twitch + Kick",
            },
            (true, false) => "Send a message to Twitch",
            (false, true) => "Send a message to Kick",
            (false, false) => "Log in from Settings (⚙) → Account to chat",
        };
        format!(" {text}")
    }

    /// The composer under the chat log: the emote picker (when open), the
    /// "replying to" bar, and the input row. Rendered inside the chat panel's
    /// cell so it spans the chat column — with side panels open the input stays
    /// under the chat, not stretched across the whole window. The send-target
    /// toggle and the emote-picker button live *inside* the input box
    /// (prefix/suffix), so there's no button row to misalign.
    fn render_composer(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        // Tab completion. A single-line `Input` binds Tab to its
        // `IndentInline` action, which for a non-indentable input
        // just *propagates* (no-op) — so the keystroke falls through
        // to Root's Tab → focus_next, moving focus away. We catch
        // that same `IndentInline` action on this ancestor (it runs
        // after the input's no-op) and run completion instead,
        // stopping propagation so focus doesn't move. Raw key
        // listeners can't be used here: gpui dispatches action
        // bindings before raw KeyDown listeners, and the input
        // consumes the keystroke as an action first.
        //
        // The autocomplete popup's keys need `capture_action`
        // instead: unlike Tab, the input *consumes* Up/Down/
        // Enter/Escape (single-line MoveUp/MoveDown return
        // without propagating; Enter emits PressEnter = send),
        // so a bubble-phase listener here never sees them.
        // Capture runs before the input's handler; when the
        // popup is closed each handler does nothing and the key
        // flows through normally.
        // The kit pads the input 12px each side; tighten the left so the
        // send-target pill hugs the box's edge (the right side is re-asserted
        // by the kit when a suffix is set — the picker button carries a
        // negative margin instead).
        let mut input = Input::new(&self.input)
            .pl(px(6.))
            .suffix(self.picker_button(cx));
        if let Some(toggle) = self.send_target_toggle(cx) {
            input = input.prefix(toggle);
        }
        v_flex()
            .w_full()
            .flex_none()
            // The emote picker, when open, sits between the feed and the input.
            .when(self.picker_open, |col| {
                col.child(self.render_emote_picker(cx))
            })
            // Active chat restrictions (follower-only, slow, ...), when any —
            // only when placed above the input (not at the top, or hidden).
            .when(
                crate::settings::chat_modes_placement()
                    == crate::settings::ChatModesPlacement::Bottom,
                |col| col.children(self.render_mode_bar(false, cx)),
            )
            // The "replying to" bar, when a reply is pending, sits just above input.
            .children(self.render_reply_bar(cx))
            .child(
                h_flex()
                    .w_full()
                    .relative()
                    .px_2()
                    .py_1p5()
                    .items_center()
                    // The input bar sits on the chrome tone (one elevation step
                    // above the log), separated by a hairline instead of a
                    // recessed slab.
                    .bg(gpui::rgb(render::tab_bar_bg()))
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .flex_1()
                            .on_action(cx.listener(
                                |this, _: &gpui_component::input::IndentInline, window, cx| {
                                    if this.popup.is_some() {
                                        this.popup_tab(window, cx);
                                        cx.stop_propagation();
                                    } else if this.complete_input(window, cx) {
                                        cx.stop_propagation();
                                    }
                                },
                            ))
                            .capture_action(cx.listener(
                                |this, _: &gpui_component::input::MoveUp, window, cx| {
                                    // Popup selection first; else recall older history.
                                    if this.popup_move(-1, cx)
                                        || this.history_recall(-1, window, cx)
                                    {
                                        cx.stop_propagation();
                                    }
                                },
                            ))
                            .capture_action(cx.listener(
                                |this, _: &gpui_component::input::MoveDown, window, cx| {
                                    if this.popup_move(1, cx)
                                        || this.history_recall(1, window, cx)
                                        || this.clear_typed_draft(window, cx)
                                    {
                                        cx.stop_propagation();
                                    }
                                },
                            ))
                            .capture_action(cx.listener(
                                |this, _: &gpui_component::input::Enter, window, cx| {
                                    let Some(popup) = this.popup.as_ref() else {
                                        return;
                                    };
                                    if popup.items.is_empty() {
                                        // "No matches": close and let the Enter
                                        // through to send the line as typed.
                                        this.popup = None;
                                        cx.notify();
                                        return;
                                    }
                                    let ix = popup.selected;
                                    // Already fully typed (e.g. "/slow" with
                                    // "/slow" highlighted): inserting again
                                    // would just demand a second Enter — close
                                    // the popup and let this one send.
                                    if let Some(item) = popup.items.get(ix) {
                                        let state = this.input.read(cx);
                                        let text = state.value().to_string();
                                        let cursor = state.cursor().min(text.len());
                                        let word = &text[word_start(&text, cursor)..cursor];
                                        if item.insert_text().eq_ignore_ascii_case(word) {
                                            this.popup = None;
                                            cx.notify();
                                            return;
                                        }
                                    }
                                    this.popup_select(ix, window, cx);
                                    cx.stop_propagation();
                                },
                            ))
                            .capture_action(cx.listener(
                                |this, _: &gpui_component::input::Escape, _, cx| {
                                    if this.popup.take().is_some() {
                                        cx.stop_propagation();
                                        cx.notify();
                                    }
                                },
                            ))
                            .child(input),
                    )
                    .children(self.render_input_popup(cx)),
            )
            .into_any_element()
    }
}

/// Byte offset where the word ending at `cursor` starts: just past the last
/// whitespace before the cursor, or 0. The one place the "word at the cursor"
/// scan lives (Tab completion, the autocomplete popup, and the `:`-stem check
/// all need it). Steps past the whitespace by its real UTF-8 width — `rfind + 1`
/// landed mid-character on multi-byte whitespace (a pasted no-break space), and
/// the callers' slice at that offset paniced.
fn word_start(text: &str, cursor: usize) -> usize {
    text[..cursor]
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        .map(|(i, ch)| i + ch.len_utf8())
        .unwrap_or(0)
}

/// Replaces the word starting at `start` (up to the next whitespace) with
/// `insert` plus a trailing space, keeping any text that
/// followed it — the shared splice of Tab completion and the popup's insert.
fn replace_word(text: &str, start: usize, insert: &str) -> String {
    let word_end = text[start..]
        .find(char::is_whitespace)
        .map(|i| start + i)
        .unwrap_or(text.len());
    let suffix = text[word_end..].trim_start_matches(' ');
    format!("{}{insert} {suffix}", &text[..start])
}

/// Whether `name` starts with `stem_lc` (an *already-lowercased* stem),
/// case-insensitively, without allocating a lowercased copy of `name` — this
/// runs per emote/chatter per keystroke while autocompleting, where the old
/// `name.to_lowercase().starts_with(..)` allocated for every candidate.
/// Both sides fold the Greek final sigma to the medial form: `str::to_lowercase`
/// (which produced the stem) is context-sensitive and yields 'ς' at word end,
/// while the per-char fold here always yields 'σ' — without the fold a name
/// ending in 'Σ' never matched its own typed stem.
fn starts_with_ci(name: &str, stem_lc: &str) -> bool {
    let fold = |c: char| if c == 'ς' { 'σ' } else { c };
    let mut name_chars = name.chars().flat_map(char::to_lowercase).map(fold);
    stem_lc.chars().map(fold).all(|c| name_chars.next() == Some(c))
}

/// The stable sequence numbers of the model's retained events that pass
/// `filter` — seeds a view's events panel (see [`ChatView::events_shown`]).
fn filtered_event_seqs(
    model: &crate::channel_store::ChannelModel,
    filter: tabs::EventFilter,
    collapse_gifts: bool,
) -> std::collections::VecDeque<u64> {
    model
        .events
        .iter()
        .enumerate()
        .filter(|(_, ev)| filter.enabled(ev.kind))
        // Collapsing hides a mass gift's per-recipient rows; their names show
        // under the batch's summary row instead.
        .filter(|(_, ev)| !(collapse_gifts && ev.group.is_some()))
        .map(|(i, _)| model.events_base + i as u64)
        .collect()
}

/// Tails an aux panel like the chat log: if a new row arrived (`new_flag`,
/// consumed here) and the panel was already scrolled to the bottom (or isn't
/// yet overflowing), snap to the newest row; otherwise leave the user's scroll
/// position alone so they can read history. `offset.y` grows more negative as
/// you scroll down, reaching `-max_offset.y` at the bottom (a small epsilon
/// absorbs rounding).
pub(crate) fn tail_panel(new_flag: &mut bool, scroll: &gpui::ScrollHandle) {
    if std::mem::take(new_flag) {
        let offset = scroll.offset().y;
        let max = scroll.max_offset().y;
        if offset <= -max + px(2.) {
            scroll.scroll_to_bottom();
        }
    }
}

/// Sizes a layout column/panel by its fractional share: `flex_grow` on a zero
/// basis distributes the free space proportionally, so shares keep meaning even
/// with the fixed-px dividers between the cells.
fn set_share(style: &mut gpui::StyleRefinement, share: f32) {
    style.flex_grow = Some(share.max(0.01));
    style.flex_shrink = Some(1.0);
    style.flex_basis = Some(px(0.).into());
}

/// Drag handle for the layout dividers; renders nothing (the visible divider
/// is the element it's attached to). Its distinct type lets the drag handlers filter.
#[derive(Clone)]
struct PanelDivider;

impl Render for PanelDivider {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// Builds the name-click callback for one message: clicking the author opens that
/// chatter's usercard on the owning [`ChatView`]. Captures only the view handle
/// and the message id — the chatter's identity is resolved from that message when
/// the click fires, so a row's click target carries one cheap id, not a copy of
/// every field.
fn name_click_for(entity: &Entity<ChatView>, msg: &Message) -> render::NameClick {
    let entity = entity.clone();
    let msg_id = SharedString::from(msg.id.clone());
    Box::new(move |_window: &mut Window, cx: &mut App| {
        entity.update(cx, |this, cx| {
            this.open_usercard(&msg_id, cx);
            cx.notify();
        });
    })
}

/// Builds the right-click-to-tag callback for one message: inserts `@name ` into
/// the composer and switches the send target to the chatter's platform. Captures
/// the author's display name + the row's platform (not the send target — tagging
/// a Kick chatter always targets Kick).
fn name_right_click_for(entity: &Entity<ChatView>, msg: &Message) -> render::NameRightClick {
    let entity = entity.clone();
    let display_name = msg.author.display_name.clone();
    let platform = msg.platform;
    Box::new(move |window: &mut Window, cx: &mut App| {
        entity.update(cx, |this, cx| {
            this.tag_user(&display_name, platform, window, cx);
        });
    })
}

/// Builds the mention-click callback for one message: clicking an `@name` in the
/// body opens that user's usercard on the row's platform (not the send target —
/// a mention in a Kick message opens a Kick card). Captures only the view handle
/// and the platform; the name arrives from the clicked token.
fn mention_click_for(entity: &Entity<ChatView>, msg: &Message) -> render::MentionClick {
    mention_click_for_platform(entity, msg.platform)
}

/// The mention-click callback keyed on a platform alone (no source message):
/// used by event rows, where the clickable name is the acting user / an
/// `@mention` in the event text and there's no chat message to key on. Opens the
/// clicked name's usercard on the event's platform.
fn mention_click_for_platform(
    entity: &Entity<ChatView>,
    platform: bks_core::Platform,
) -> render::MentionClick {
    let entity = entity.clone();
    std::rc::Rc::new(move |login: &str, _window: &mut Window, cx: &mut App| {
        entity.update(cx, |this, cx| {
            this.open_usercard_named(login, platform, cx);
            cx.notify();
        });
    })
}

/// Builds the reply-button callback for one message: starts a reply to it on the
/// owning [`ChatView`]. Captures only the view handle + message id; the reply
/// identity is resolved from the still-present row when clicked.
fn reply_click_for(entity: &Entity<ChatView>, msg: &Message) -> render::ReplyClick {
    let entity = entity.clone();
    let msg_id = msg.id.clone();
    std::rc::Rc::new(move |window: &mut Window, cx: &mut App| {
        entity.update(cx, |this, cx| {
            this.start_reply(&msg_id, window, cx);
        });
    })
}

/// Builds the "replying to" line's click callback for one reply message: opens
/// the thread panel seeded from it. Captures the view handle + message id; the
/// chain is rebuilt from the live buffer when the panel renders.
fn thread_click_for(entity: &Entity<ChatView>, msg: &Message) -> render::ThreadClick {
    let entity = entity.clone();
    let msg_id = msg.id.clone();
    std::rc::Rc::new(
        move |anchor: Point<Pixels>, _window: &mut Window, cx: &mut App| {
            entity.update(cx, |this, cx| {
                this.open_thread_panel(&msg_id, anchor, cx);
            });
        },
    )
}

/// Builds the mod-button-strip callback for one message (only called for rows
/// whose platform the user can moderate): receives the clicked button's command
/// template and runs it through [`ChatView::run_mod_button`], which resolves
/// the author from the still-present row and dispatches at the row's platform.
fn mod_click_for(entity: &Entity<ChatView>, msg: &Message) -> render::ModClick {
    let entity = entity.clone();
    let msg_id = msg.id.clone();
    std::rc::Rc::new(move |command: &str, _window: &mut Window, cx: &mut App| {
        let command = command.to_string();
        entity.update(cx, |this, cx| {
            this.run_mod_button(&msg_id, &command, cx);
        });
    })
}

/// Builds the pin-button callback for one message (only called for rows whose
/// platform the user can moderate — see [`ChatView::can_pin`]): pins it via the
/// owning [`ChatView`], which resolves the message from the still-present row.
fn pin_click_for(entity: &Entity<ChatView>, msg: &Message) -> render::PinClick {
    let entity = entity.clone();
    let msg_id = msg.id.clone();
    std::rc::Rc::new(move |window: &mut Window, cx: &mut App| {
        entity.update(cx, |this, cx| {
            this.confirm_pin(&msg_id, window, cx);
        });
    })
}

/// The message preview inside the pin/unpin confirmation dialog: the message
/// rendered like a chat line (badges, colored name, emotes inline) in the same
/// tinted, accent-barred box it will occupy as a banner.
/// The pin dialog's duration options: timed (Twitch's API takes 30s–30m) or
/// "until the stream ends" (Helix with no duration param). Kick's pin endpoint
/// always wants a number, so the open-ended option is Twitch-only.
const PIN_DURATIONS: &[(&str, Option<u32>)] = &[
    ("30s", Some(30)),
    ("1m", Some(60)),
    ("5m", Some(300)),
    ("10m", Some(600)),
    ("20m", Some(1200)),
    ("30m", Some(1800)),
    ("Until stream ends", None),
];

/// The pin dialog's clickable duration-chip row. A free fn: the dialog builder
/// runs against the window root with a plain `App`, so it reads the current
/// selection off the view entity and writes clicks back through it (the
/// notify re-runs the dialog builder, updating the highlight).
fn pin_duration_chips(
    entity: &Entity<ChatView>,
    platform: bks_core::Platform,
    cx: &App,
) -> impl IntoElement {
    let selected = entity.read(cx).pin_duration_choice;
    h_flex()
        .gap_1()
        .flex_wrap()
        .items_center()
        .child(
            div()
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                .child(SharedString::from("Pin for:")),
        )
        .children(
            PIN_DURATIONS
                .iter()
                .filter(|(_, secs)| secs.is_some() || platform == bks_core::Platform::Twitch)
                .map(|&(label, secs)| {
                    let entity = entity.clone();
                    div()
                        .id(SharedString::from(label))
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .border_1()
                        .border_color(cx.theme().border)
                        .text_size(px(12.))
                        .cursor_pointer()
                        .when(selected == secs, |d| {
                            d.bg(cx.theme().accent)
                                .text_color(cx.theme().accent_foreground)
                        })
                        .hover(|d| d.bg(cx.theme().accent.opacity(0.7)))
                        .child(SharedString::from(label))
                        .on_click(move |_, _, cx| {
                            entity.update(cx, |this, cx| {
                                this.pin_duration_choice = secs;
                                cx.notify();
                            });
                        })
                }),
        )
}

fn pin_dialog_preview(msg: &Message, font_size: f32) -> gpui::AnyElement {
    let selection = selectable::Selection::new();
    selection.begin_frame();
    let mut ordinal = 0usize;
    let rendered = render::render_message(
        msg,
        render::RowFlags::default(),
        font_size,
        &selection,
        &mut ordinal,
        render::RowHandlers::default(),
    );
    let p = render::palette();
    div()
        .w_full()
        .min_w_0()
        // Breathing room under the dialog title — the kit packs the description
        // right against it, while its footer gap is generous, which read as
        // lopsided.
        .mt_3()
        .max_h(px(160.))
        .overflow_hidden()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(gpui::rgb(p.event_bg))
        .border_l_2()
        .border_color(gpui::rgb(p.event_text))
        .child(rendered)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bks_core::{Author, Message, Platform};
    use chrono::{TimeZone, Utc};
    use std::collections::VecDeque;

    fn msg_row(secs: i64, historical: bool) -> Row {
        let ts = Utc.timestamp_opt(secs, 0).unwrap();
        Row::Message {
            msg: std::sync::Arc::new(Message {
                id: format!("m{secs}"),
                platform: Platform::Twitch,
                channel: "c".into(),
                timestamp: ts,
                author: Author {
                    login: "u".into(),
                    display_name: "U".into(),
                    color: None,
                    badges: Vec::new(),
                    user_id: String::new(),
                    paint: None,
                },
                elements: Vec::new(),
                raw_text: String::new(),
                reply: None,
                first_message: false,
                highlighted: false,
                historical,
                reward_id: None,
            }),
        }
    }

    /// Simulates `insert_message`'s placement using the same free function, to
    /// assert two history bursts interleave by timestamp and stay ahead of live.
    fn insert(rows: &mut VecDeque<Row>, secs: i64, historical: bool) {
        let ts = Utc.timestamp_opt(secs, 0).unwrap();
        if !historical {
            rows.push_back(msg_row(secs, false));
            return;
        }
        match history_insert_index(rows.iter(), ts) {
            Some(i) => rows.insert(i, msg_row(secs, true)),
            None => rows.push_back(msg_row(secs, true)),
        }
    }

    fn timestamps(rows: &VecDeque<Row>) -> Vec<i64> {
        rows.iter()
            .map(|r| match r {
                Row::Message { msg, .. } => msg.timestamp.timestamp(),
                _ => -1,
            })
            .collect()
    }

    #[test]
    fn two_history_bursts_interleave_by_timestamp() {
        let mut rows = VecDeque::new();
        // Twitch burst (oldest-first), then Kick burst (oldest-first).
        for s in [10, 30, 50] {
            insert(&mut rows, s, true);
        }
        for s in [20, 40, 60] {
            insert(&mut rows, s, true);
        }
        assert_eq!(timestamps(&rows), vec![10, 20, 30, 40, 50, 60]);
    }

    #[test]
    fn history_stays_ahead_of_live() {
        let mut rows = VecDeque::new();
        insert(&mut rows, 100, false); // a live message arrived first
        insert(&mut rows, 50, true); // late history backfill
        insert(&mut rows, 30, true);
        // History sorts among itself and sits before the live message.
        assert_eq!(timestamps(&rows), vec![30, 50, 100]);
    }

    #[test]
    fn flash_strength_starts_full_and_fades_to_none() {
        // Fresh: near full strength.
        let fresh = FlashTarget {
            platform: Platform::Twitch,
            msg_id: "m".into(),
            started_at: std::time::Instant::now(),
        };
        let s = fresh.strength().expect("just started, still fading");
        assert!(s > 0.9, "a fresh flash should be near full, got {s}");

        // Past the fade window: gone (so the caller drops it).
        let old = FlashTarget {
            platform: Platform::Twitch,
            msg_id: "m".into(),
            started_at: std::time::Instant::now() - FLASH_DURATION - std::time::Duration::from_secs(1),
        };
        assert!(old.strength().is_none(), "a faded flash should report None");
    }

    #[test]
    fn word_start_finds_the_word_at_the_cursor() {
        assert_eq!(word_start("hello world", 11), 6);
        assert_eq!(word_start("hello world", 5), 0);
        assert_eq!(word_start("", 0), 0);
        assert_eq!(word_start("word", 4), 0);
        assert_eq!(word_start("a  b", 4), 3);
        // Multi-byte whitespace (a pasted no-break space): the offset must land
        // on a char boundary — `rfind + 1` sat mid-char and paniced the callers.
        let text = "foo\u{00A0}bar";
        let start = word_start(text, text.len());
        assert!(text.is_char_boundary(start));
        assert_eq!(&text[start..], "bar");
    }

    #[test]
    fn replace_word_splices_with_trailing_space() {
        // Replaces the word at `start`, keeps the suffix, one space between.
        assert_eq!(replace_word("hi ka tail", 3, "Kappa"), "hi Kappa tail");
        // At the end of the text: candidate + trailing space.
        assert_eq!(replace_word("hi ka", 3, "Kappa"), "hi Kappa ");
        // Existing spaces after the word collapse to the single inserted one.
        assert_eq!(replace_word("ka   x", 0, "Kappa"), "Kappa x");
    }

    #[test]
    fn starts_with_ci_matches_case_insensitively() {
        assert!(starts_with_ci("KappaPride", "kappa"));
        assert!(starts_with_ci("kappa", "KAPPA".to_lowercase().as_str()));
        assert!(starts_with_ci("Kappa", "")); // empty stem matches everything
        assert!(!starts_with_ci("Kap", "kappa")); // stem longer than name
        assert!(!starts_with_ci("PogChamp", "kappa"));
        // Non-ASCII lowercasing still matches.
        assert!(starts_with_ci("Ördög", "ördög"));
        // Greek final sigma: str::to_lowercase turns a trailing Σ into ς, the
        // per-char fold into σ — both must match either stem form.
        assert_eq!("ΝΙΚΟΣ".to_lowercase(), "νικος"); // trailing final sigma ς
        assert!(starts_with_ci("ΝΙΚΟΣ", &"ΝΙΚΟΣ".to_lowercase()));
        assert!(starts_with_ci("νικοσ", "νικος"));
        assert!(starts_with_ci("νικος", "νικοσ"));
    }

    #[test]
    fn live_messages_append_in_order() {
        let mut rows = VecDeque::new();
        insert(&mut rows, 1, false);
        insert(&mut rows, 2, false);
        insert(&mut rows, 3, false);
        assert_eq!(timestamps(&rows), vec![1, 2, 3]);
    }

    #[test]
    fn eased_count_interpolates_both_directions() {
        assert_eq!(eased_count(100, 200, 0.0), 100);
        assert_eq!(eased_count(100, 200, 1.0), 200);
        assert_eq!(eased_count(200, 100, 1.0), 100);
        let mid_up = eased_count(100, 200, 0.5);
        assert!(mid_up > 100 && mid_up < 200);
        let mid_down = eased_count(200, 100, 0.5);
        assert!(mid_down > 100 && mid_down < 200);
        // Out-of-range progress clamps to the endpoints.
        assert_eq!(eased_count(100, 200, 1.5), 200);
        assert_eq!(eased_count(100, 200, -0.5), 100);
        // A same-value "animation" holds steady.
        assert_eq!(eased_count(42, 42, 0.3), 42);
    }

    #[test]
    fn settled_viewer_anim_is_done_immediately() {
        let now = std::time::Instant::now();
        // A first/settled count (from == to) must not hold repaint ticks open.
        let settled = ViewerAnim {
            from: 42,
            to: 42,
            started: now,
        };
        assert!(settled.done(now));
        let rolling = ViewerAnim {
            from: 100,
            to: 200,
            started: now,
        };
        assert!(!rolling.done(now));
        assert!(rolling.done(now + VIEWER_ANIM_DURATION));
    }

}
