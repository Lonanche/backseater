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
    child_window, controller, render, selectable, usercard, viewerlist, USERCARD_MESSAGES,
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
/// chatters, see [`ChatView::mention_candidates`]) or `:` (emotes from the
/// send target's set(s), see [`ChatView::emote_popup_candidates`]). Up/Down/Tab
/// cycle, Enter/click inserts, Escape dismisses; recomputed on every input
/// change, so typing a space (the word no longer starts with the trigger)
/// closes it naturally. An empty candidate list renders as a "No matches"
/// notice rather than hiding, so the user can see why nothing completes.
struct InputPopup {
    /// Byte offset of the trigger word's start in the input text.
    start: usize,
    /// Every matching candidate. May be empty ("No matches").
    items: Vec<PopupItem>,
    /// Index of the highlighted candidate.
    selected: usize,
    /// Index of the first visible row: the popup shows a
    /// [`POPUP_VISIBLE_ITEMS`]-row window onto `items` that follows the
    /// highlight (and the mouse wheel), with "more" hints past each edge.
    window_start: usize,
}

/// One candidate in the input autocomplete popup.
enum PopupItem {
    /// A chatter name (no `@`); inserted as `@Name `.
    Mention(String),
    /// An emote, its row showing the image; inserted as its bare name.
    Emote(bks_core::Emote),
}

impl PopupItem {
    /// The text this candidate inserts in place of the trigger word.
    fn insert_text(&self) -> String {
        match self {
            PopupItem::Mention(name) => format!("@{name}"),
            PopupItem::Emote(e) => e.name.clone(),
        }
    }
}

/// Rows the input popup shows at once; the rest scroll into view as the
/// selection moves past an edge (Up/Down/Tab wrap, mouse wheel too).
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
    /// The message being replied to, if the user clicked a row's reply button. The
    /// next sent line threads under it (on the parent's platform); shown as a
    /// "replying to" bar above the input. Cleared on send or cancel.
    replying_to: Option<controller::ReplyTo>,
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
    /// The logged-in user's personal Twitch emotes (sub/follower/global), fetched
    /// lazily the first time the picker opens. Shown on the Twitch tab.
    personal_emotes: Vec<bks_core::Emote>,
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

        Self {
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
            dismissed_pins: std::collections::HashSet::new(),
            expanded_gifts: std::collections::HashSet::new(),
            viewer_anims: HashMap::new(),
            viewer_anim_tick_pending: false,
            usercard: None,
            usercard_window: None,
            viewer_list: None,
            viewer_list_window: None,
            viewer_search: None,
            _viewer_search_sub: None,
            parent_window: window.window_handle(),
            emote_popup: None,
            replying_to: None,
            layout_drag: None,
            grid_bounds: std::rc::Rc::new(std::cell::Cell::new(gpui::Bounds::default())),
            events_list_state,
            events_shown,
            mentions_scroll: gpui::ScrollHandle::new(),
            mentions_new: false,
            personal_emotes: Vec::new(),
            picker_open: false,
            picker_tab,
            picker_search,
            picker_list_state,
            picker_rows: Vec::new(),
            picker_cells: HashMap::new(),
            completion: None,
            popup: None,
            emotes_fetched: false,
            sent_history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            history_setting: false,
            image_cache,
            log_view,
            _input_sub,
            _picker_search_sub,
        }
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
            ChannelEvent::Appended { index, msg } | ChannelEvent::Inserted { index, msg } => {
                self.list_state.splice(*index..*index, 1);
                // A new row may be a mention (for our terms) — flag the mentions
                // panel to tail + feed the all-tabs store.
                self.note_new_row(msg.as_deref());
            }
            ChannelEvent::RemovedFront => {
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
                self.list_state.reset(self.channel.read(cx).len());
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
        self.list_state.reset(channel.read(cx).len());
        self._channel_sub = cx.subscribe(&channel, Self::on_channel_event);
        self.channel_key = key;
        self.channel = channel;
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

    /// Updates the ignore list used to drop incoming messages. Affects new
    /// messages only; already-shown rows are left as-is.
    pub(crate) fn set_ignore(&mut self, ignore: bks_core::IgnoreList) {
        self.ignore = ignore;
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

    /// Applies a new chat font size (called when the preference changes). Row
    /// heights change, so the list must re-measure (`reset` keeps the count).
    pub(crate) fn set_font_size(&mut self, font_size: f32, cx: &mut Context<Self>) {
        self.font_size = font_size;
        self.remeasure(cx);
    }

    /// Re-measures every row and repaints the log — for preference changes that
    /// change row heights without changing the rows (font family/size).
    pub(crate) fn remeasure(&mut self, cx: &mut Context<Self>) {
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
            if trimmed.eq_ignore_ascii_case("/chatters") || trimmed.eq_ignore_ascii_case("/viewers")
            {
                // A UI command, not a chat line: opens the viewer-list window.
                self.open_viewer_list(cx);
            } else if !trimmed.is_empty() {
                // A pending reply threads this line and is then cleared; a
                // `/command` never replies (the controller ignores the target).
                self.controller.handle_input(&text, self.replying_to.take());
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
            },
            parent_elements: msg.elements.clone(),
        });
        self.input.update(cx, |this, cx| this.focus(window, cx));
        cx.notify();
    }

    /// Opens the pin confirmation dialog for message `msg_id`: the message is
    /// shown as it will appear pinned, and only Pin actually pins it.
    fn confirm_pin(&self, msg_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(msg) = self.message_by_id(msg_id, cx) else {
            return;
        };
        let entity = cx.entity();
        let font_size = self.font_size;
        let id = msg.id.clone();
        let label = msg.platform.label();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let entity = entity.clone();
            let id = id.clone();
            alert
                .title(format!("Pin to {label} chat?"))
                .description(pin_dialog_preview(&msg, font_size))
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Pin")
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    entity.update(cx, |this, cx| this.pin_message(&id, cx));
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
                        match platform {
                            bks_core::Platform::Twitch => {
                                this.controller.unpin_twitch(id.clone())
                            }
                            bks_core::Platform::Kick => this.controller.unpin_kick(),
                            _ => {}
                        }
                    });
                    true
                })
        });
    }

    /// Pins `msg_id` on its platform. Twitch pins by message id (Helix); Kick's
    /// endpoint wants the original message object back, rebuilt from our copy
    /// of the row.
    fn pin_message(&self, msg_id: &str, cx: &App) {
        let Some(msg) = self.message_by_id(msg_id, cx) else {
            return;
        };
        match msg.platform {
            bks_core::Platform::Twitch => self.controller.pin_twitch(msg.id.clone()),
            bks_core::Platform::Kick => {
                let pinnable = bks_kick::PinnableMessage {
                    id: msg.id.clone(),
                    user_id: msg.author.user_id.parse().unwrap_or(0),
                    login: msg.author.login.clone(),
                    username: msg.author.display_name.clone(),
                    color: msg.author.color.map(|c| format!("#{:06X}", c.to_u32())),
                    content: msg.raw_text.clone(),
                    created_at: msg.timestamp,
                };
                self.controller.pin_kick(pinnable);
            }
            _ => {}
        }
    }

    /// Whether the logged-in user can pin/unpin on `platform` (gates the per-row
    /// 📌 button and the banner's Unpin). Twitch knows real mod status (IRC
    /// USERSTATE); for Kick a login is the best signal we have — a non-mod's
    /// attempt fails with the API error as a notice, like the usercard actions.
    fn can_pin(&self, platform: bks_core::Platform, cx: &App) -> bool {
        self.channel.read(cx).can_moderate(platform)
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
            let label = if collapsed > 1 {
                format!("📌 {collapsed}")
            } else {
                "📌".to_string()
            };
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
                        .child(SharedString::from(label))
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
        // part of the log's drag-select.
        let selection = selectable::Selection::new();
        selection.begin_frame();
        let mut ordinal = 0usize;
        let message = render::render_message(
            &pin.message,
            render::RowFlags::default(),
            self.font_size,
            &selection,
            &mut ordinal,
            render::RowHandlers::default(),
        );

        let header_label = if pin.pinned_by.is_empty() {
            "Pinned".to_string()
        } else {
            format!("Pinned by {}", pin.pinned_by)
        };
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
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(self.font_size * 0.75))
                    .text_color(gpui::rgb(p.event_text))
                    .child(SharedString::from(format!("📌 {header_label}"))),
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
                    Tooltip::new("Collapse — the 📌 chip brings it back").build(window, cx)
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
                return;
            }
            // The window closed under us — fall through and open a fresh one.
        }

        // Always opens centered over the chat window; drag it away from there.
        let opened = child_window::open_centered(
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
        view.update(cx, |this, cx| {
            this.usercard_window = Some(handle);
            // The user closing the window (OS ✕) releases its content view;
            // drop the card then — unless a newer window replaced it.
            cx.observe_release(&content, move |this, _, cx| {
                if this.usercard_window == Some(handle) {
                    this.usercard_window = None;
                    this.usercard = None;
                }
                cx.notify();
            })
            .detach();
        });
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
        // Typing a `:`-emote stem needs the personal + channel native emote set,
        // which is otherwise fetched lazily on first picker open — fetch it here
        // too so autocomplete works without opening the picker first.
        if self.emote_stem_active(cx) {
            self.ensure_personal_emotes(cx);
        }
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

    /// Whether the word ending at the input cursor is a `:`-emote stem (so the
    /// emote autocomplete is what would show). Cheap; used to trigger the one-time
    /// native-emote fetch.
    fn emote_stem_active(&self, cx: &Context<Self>) -> bool {
        let state = self.input.read(cx);
        let text = state.value().to_string();
        let cursor = state.cursor().min(text.len());
        text[word_start(&text, cursor)..cursor].starts_with(':')
    }

    /// The popup state for the input's current text + cursor, or `None` when the
    /// word at the cursor is neither an `@`-mention nor a `:`-emote (or this tab
    /// has no platform to complete for).
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
        })
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

    /// A Tab press while the popup is open: selects a sole match outright,
    /// dismisses an empty ("No matches") popup, and otherwise cycles the
    /// highlight — mirroring what Enter/Escape would do once the list is
    /// unambiguous.
    fn popup_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.popup.as_ref().map(|m| m.items.len()) {
            Some(0) => {
                self.popup = None;
                cx.notify();
            }
            Some(1) => self.popup_select(0, window, cx),
            Some(_) => {
                self.popup_move(1, cx);
            }
            None => {}
        }
    }

    /// The autocomplete popup, anchored just above the input bar (its parent).
    /// Rows are clickable; the highlighted one tracks Up/Down/Tab. Only
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
                    .child(SharedString::from("No matches"))
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
    /// Kick and this tab has a Kick channel (otherwise nothing to switch).
    fn send_target_toggle(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.controller.kick_logged_in() || !self.controller.has_kick() {
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
                // mode hides those rows, so only it lists them here.
                let is_summary = ev.details.gift_count.is_some();
                let mut names = ev.details.recipients.clone();
                if is_summary && collapse {
                    names.extend(
                        model
                            .events
                            .iter()
                            .filter(|e| e.group == Some(seq))
                            .filter_map(|e| e.details.recipient.clone()),
                    );
                }
                let expandable = is_summary && !names.is_empty();
                let expanded = expandable && this.expanded_gifts.contains(&seq);
                let row = render::render_event_compact(
                    render::PanelEvent {
                        platform: ev.platform,
                        kind: ev.kind,
                        text: &ev.text,
                        timestamp: ev.timestamp,
                        details: &ev.details,
                        message: if hide_msgs { None } else { ev.message.as_deref() },
                        expandable,
                        expanded_names: expanded.then_some(names),
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
                .filter_map(|row| match row {
                    Row::Message { msg } if self.mentions.matches(&msg.raw_text) => {
                        let struck = model.is_struck(msg);
                        let decorated = log::decorate(msg, model);
                        Some(
                            render::render_message(
                                &decorated,
                                render::RowFlags {
                                    struck,
                                    mentioned: true,
                                    ..Default::default()
                                },
                                font_size,
                                &selection,
                                &mut ordinal,
                                render::RowHandlers {
                                    name_click: Some(name_click_for(&entity, msg)),
                                    ..Default::default()
                                },
                            )
                            .into_any_element(),
                        )
                    }
                    _ => None,
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
                        let log = div()
                            .relative()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .flex()
                            .flex_col()
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

    /// The emote-info popup overlay when one is open, else `None`. A transparent
    /// full-window backdrop (behind the card) closes it on any outside click; the
    /// card itself is `occlude`d so clicking it doesn't close it. The card is
    /// anchored at the click position, clamped to stay within the window.
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

    /// The "Replying to <name>: <preview>" bar shown above the input when a reply
    /// is pending, with the parent's emotes rendered inline and an ✕ to cancel.
    /// `None` when not replying.
    fn render_reply_bar(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let reply = self.replying_to.as_ref()?;
        // A stable per-reply id seed so the preview's emote images animate.
        let seed = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            reply.message_id.hash(&mut h);
            h.finish()
        };
        Some(
            h_flex()
                .w_full()
                .px_2()
                .py_1()
                .gap_2()
                .items_center()
                .bg(cx.theme().secondary)
                .text_color(cx.theme().muted_foreground)
                .text_size(px(self.font_size))
                .child(div().flex_none().child(SharedString::from(format!(
                    "Replying to {}:",
                    reply.parent.author
                ))))
                .child(div().flex_1().min_w_0().overflow_hidden().child(
                    render::render_reply_preview(&reply.parent_elements, self.font_size, seed),
                ))
                .child(
                    div()
                        .id("cancel-reply")
                        .px_1()
                        .cursor_pointer()
                        .child(SharedString::from("✕"))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.replying_to = None;
                                cx.notify();
                            }),
                        ),
                )
                .into_any_element(),
        )
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
        v_flex()
            .gap_3()
            .child(header)
            .child(actions)
            .child(messages)
            .into_any_element()
    }

    /// The moderation panel: a compact "Timeout" chip row plus Ban/Unban (and, on
    /// Twitch, Mod/VIP grant toggles). Built to fit the card's default width — small
    /// chips that wrap rather than a single overflowing row. Shown only when the
    /// logged-in user can moderate the card's platform: Twitch needs `twitch_mod`;
    /// Kick needs a Kick login (its API has ban/timeout/unban, no role grants).
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

        // (label, seconds) timeout presets. Trimmed to the common spread so the
        // chip row stays compact at the card's default width.
        const PRESETS: &[(&str, u32)] = &[
            ("1s", 1),
            ("1m", 60),
            ("5m", 300),
            ("10m", 600),
            ("30m", 1800),
            ("1h", 3600),
            ("1d", 86400),
        ];

        let timeout_chips = h_flex()
            .w_full()
            .flex_wrap()
            .gap_1()
            .children(PRESETS.iter().map(|(label, secs)| {
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
            }));

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

        // A compact, sectioned layout: a "Timeout" chip row, a Ban/Unban row, and
        // (Twitch only) a "Role" row with explicit Mod/Unmod/VIP/Unvip buttons.
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
            .when(show_ban_timeout, |col| {
                col.child(
                    v_flex()
                        .w_full()
                        .gap_1()
                        .child(section_label("Timeout"))
                        .child(timeout_chips),
                )
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap_1()
                        .child(ban)
                        .child(unban),
                )
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
        if msgs.is_empty() {
            return div()
                .text_size(px(13.))
                .text_color(cx.theme().muted_foreground)
                .child(SharedString::from("No recent messages in this channel."))
                .into_any_element();
        }
        // A throwaway selection + ordinal: the card's messages aren't part of the
        // log's drag-select, so they get their own (unused) selection context.
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

        v_flex()
            .id("usercard-messages")
            .gap_1()
            .pt_2()
            .border_t_1()
            .border_color(cx.theme().border)
            .max_h(px(280.))
            .overflow_y_scroll()
            .children(rows)
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
                historical,
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
