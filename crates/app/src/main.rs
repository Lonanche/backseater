//! Backseater — the GPUI desktop app: a tabbed, merged Twitch + Kick chat client.
//!
//! The window has a tab strip on top; each tab has its own view of channel feeds
//! and send targets. Login is app-wide and
//! shared by all tabs. Right-click a tab for settings (name + channels). Tabs are
//! saved to `<config>/backseater/tabs.json` and restored on launch.

// Release builds are GUI-subsystem so Windows doesn't spawn a console window;
// debug builds keep it (BKS_DEBUG/tracing output lands there).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod animated_img;
mod assets;
mod bridge;
mod channel_store;
mod chatview;
mod child_window;
mod commands;
mod controller;
mod emote_cache;
mod image_cache;
mod mentions;
mod popout;
mod preview;
mod render;
mod search;
mod selectable;
mod session;
mod settings;
mod settings_ui;
mod sound;
mod source_hub;
mod stale_hover;
mod streamer_mode;
mod tabs;
mod thread;
mod updater;
mod usercard;
mod viewerlist;
mod window_state;

use std::sync::Arc;

use bks_platform::EventKind;
use gpui::prelude::*;
use gpui::{
    div, img, px, AnyWindowHandle, App, Context, Div, ElementId, Entity, FontWeight, MouseButton,
    Pixels, Point, SharedString, Size, Stateful, Subscription, Task, WeakEntity, Window,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::tooltip::Tooltip;
use gpui_component::{h_flex, v_flex, ActiveTheme, IconName, Root, Sizable, TitleBar, WindowExt};

use chatview::{ChatView, LiveInfo};
use controller::Controller;
use mentions::MentionStore;
use session::Session;
use settings::Settings;
use settings_ui::{card_divider, Panel, SettingsView, TermKind, TermList, TermScope};
use tabs::TabConfig;

const SYSTEM_FONT_FAMILY: &str = ".SystemUIFont";

/// Keep at most this many rows in memory.
pub(crate) const MAX_ROWS: usize = 1000;
/// Keep at most this many events in the events panel's retained buffer. Kept
/// separate from (and larger than) the chat ring buffer so a busy chat can't
/// push events out of the events panel — they persist for the whole session.
pub(crate) const MAX_EVENTS: usize = 1000;
/// Selection ordinals are derived from `row_index * ORDINAL_STRIDE` so they are
/// globally stable (independent of which rows the virtualized list builds this
/// frame) while staying monotonic in document order. The stride caps tokens per
/// row; selection only needs ordering, so gaps between rows are harmless.
pub(crate) const ORDINAL_STRIDE: usize = 1 << 16;
/// Width (px) reserved on the right of a scrolling panel for the overlay
/// scrollbar, so content doesn't render under the thumb. gpui-component's
/// `Scrollbar` is 16px wide (`THUMB_ACTIVE_INSET*2 + THUMB_ACTIVE_WIDTH = 4*2 + 8`);
/// the extra px leave a clear gap between content (incl. the reply button) and the
/// thumb so they never overlap.
pub(crate) const SCROLLBAR_WIDTH: f32 = 20.0;
/// How many of a chatter's recent messages the usercard lists.
pub(crate) const USERCARD_MESSAGES: usize = 10;
/// Emotes per row in the (virtualized) emote picker grid. Rows are fixed-width
/// chunks of this many emotes so the grid can be windowed with `gpui::list`.
pub(crate) const PICKER_COLUMNS: usize = 8;
/// Height (px) of the scrollable emote-picker grid below the search box. Kept
/// short on purpose: every visible cell animates (see `picker.rs`), so the grid
/// height directly bounds how many emotes tick at once.
pub(crate) const PICKER_GRID_HEIGHT: f32 = 132.0;

/// A tab: its persisted config + the live feed view. `id` is a stable identity
/// for this app run (it survives a channel-swap rebuild of the view, but is not
/// persisted): mention rows carry it so clicking one can find its tab even
/// after reorders.
struct TabEntry {
    id: u64,
    config: TabConfig,
    view: Entity<ChatView>,
    /// New activity (a chat message or public event) landed while this tab
    /// wasn't the active one. Drives the bold-name unread cue on the chip;
    /// cleared when the tab is selected. Fed by a subscription to the view's
    /// [`TabActivity`] event (set up in [`make_tab`]).
    unread: bool,
    /// When one of this tab's channels last went live (and which platform),
    /// driving the brief chip flash (gated on `Settings::flash_tab_on_live`).
    /// `None` = not flashing; cleared once `TAB_FLASH_DURATION` elapses. The
    /// platform tints the flash its brand color. See `chip_flash_alpha`.
    flash_start: Option<(std::time::Instant, bks_core::Platform)>,
    /// Keeps the [`TabActivity`] subscription alive for this tab's lifetime.
    _activity_sub: gpui::Subscription,
    /// Keeps the [`TabWentLive`] subscription alive for this tab's lifetime.
    _live_sub: gpui::Subscription,
    /// Keeps the [`ActivateRequested`](chatview::ActivateRequested)
    /// subscription (search-result click → re-select this tab) alive.
    _activate_sub: gpui::Subscription,
}

/// Compiles mention terms into a matcher with the per-term mute flags applied
/// (`muted` holds normalized terms). Every matcher build goes through this so
/// no path can drop the mute list — startup restore once built with
/// `MentionMatcher::new` (all-loud) and muted terms rang again after a relaunch.
fn mention_matcher(
    terms: impl IntoIterator<Item = String>,
    muted: &[String],
) -> bks_core::MentionMatcher {
    bks_core::MentionMatcher::with_sound(terms.into_iter().map(|t| {
        let sound = !muted.contains(&bks_core::normalize_term(&t));
        (t, sound)
    }))
}

/// Allocates a [`TabEntry::id`]. A process-wide counter so every path that
/// creates a tab yields a unique id.
fn next_tab_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Drag payload for the events-panel divider. Empty — the drag only needs the
/// pointer position from `DragMoveEvent`; an `EmptyView` is its render preview.
/// Drag payload identifying which tab (by index) is being dragged. Carries the
/// label + selected state so the floating drag preview is a faithful copy of the
/// tab chip rather than a bare rectangle. Its distinct type lets the drag
/// handlers filter to tab drags only.
#[derive(Clone)]
struct DraggedTab {
    /// The tab's index when the drag began (used to seed `BackseaterApp::dragging`).
    from: usize,
    label: SharedString,
    selected: bool,
}

impl Render for DraggedTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Mirror the chip in `tab_strip` so it looks like you're carrying the tab.
        h_flex()
            .px_3()
            .py_1p5()
            .gap_2()
            .items_center()
            .rounded_md()
            .bg(gpui::rgb(render::panel_bg()))
            // The drag overlay doesn't inherit the themed text color (the label
            // otherwise renders in the gpui default, near-invisible on the chip).
            .text_color(cx.theme().foreground)
            .border_1()
            .border_color(cx.theme().border)
            .shadow_md()
            .when(self.selected, |this| this.font_weight(FontWeight::BOLD))
            .child(self.label.clone())
            .child(
                div()
                    .px_1()
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from("✕")),
            )
    }
}

/// The whole app: a tab strip over the active tab's feed. Owns the tab list and
/// the shared login session.
/// How long the pointer rests on a tab chip before its live-status tooltip shows.
const CHIP_TIP_SHOW_DELAY: std::time::Duration = std::time::Duration::from_millis(300);
/// Grace after the pointer leaves the chip (or tooltip) before the tooltip hides —
/// long enough to move the pointer into the tooltip, short enough that moving
/// along the strip doesn't drag a stale tooltip around.
const CHIP_TIP_HIDE_GRACE: std::time::Duration = std::time::Duration::from_millis(250);

/// How long a tab chip flashes after one of its channels goes live (a few
/// pulses over this window, then it settles back to normal).
const TAB_FLASH_DURATION: std::time::Duration = std::time::Duration::from_millis(2400);
/// One pulse of the flash; the alpha ramps up and down each pulse.
const TAB_FLASH_PULSE: std::time::Duration = std::time::Duration::from_millis(600);
/// Repaint cadence while any chip is flashing (drives the pulse animation
/// without touching the log — like the viewer-count ease timer).
const TAB_FLASH_TICK: std::time::Duration = std::time::Duration::from_millis(50);

/// The flash tint's current opacity for a chip whose channel went live
/// `elapsed` ago: a few triangle pulses (up-then-down each `TAB_FLASH_PULSE`)
/// over `TAB_FLASH_DURATION`, fading to zero as the window closes so the last
/// pulse doesn't cut off abruptly. `0.0` once the window has elapsed.
fn chip_flash_alpha(elapsed: std::time::Duration) -> f32 {
    if elapsed >= TAB_FLASH_DURATION {
        return 0.0;
    }
    let total = TAB_FLASH_DURATION.as_secs_f32();
    let pulse = TAB_FLASH_PULSE.as_secs_f32();
    let t = elapsed.as_secs_f32();
    // Triangle wave within the current pulse: 0 → 1 → 0.
    let phase = (t % pulse) / pulse;
    let tri = 1.0 - (phase * 2.0 - 1.0).abs();
    // Overall fade so successive pulses get gentler toward the end.
    let fade = 1.0 - t / total;
    tri * fade
}

pub(crate) struct BackseaterApp {
    session: Session,
    tabs: Vec<TabEntry>,
    active: usize,
    /// Index of the tab currently being dragged, while a reorder drag is live.
    /// Updated as the tab slides past its neighbours so swaps stay in sync.
    dragging: Option<usize>,
    /// The tab whose hand-rolled live-status tooltip is showing. Hand-rolled
    /// (an absolute overlay under the chip, not gpui's `hoverable_tooltip`) so a
    /// click dismisses it — gpui leaves a hoverable tooltip up over the chip's
    /// right-click context menu — and so the hide grace is ours to pick.
    chip_tip: Option<usize>,
    /// The chip the pointer is currently over (drives tooltip show/hide).
    chip_hovered: Option<usize>,
    /// Whether the pointer is over the tooltip panel itself: the tooltip stays
    /// while hovered so its channel links stay clickable.
    chip_tip_hovered: bool,
    /// Bumped on explicit dismissal to invalidate in-flight show timers, so a
    /// tooltip can't pop up over a context menu the user just opened.
    chip_tip_gen: u64,
    /// App-wide UI preferences (chat font size).
    settings: Settings,
    #[cfg(test)]
    persistence_enabled: bool,
    /// The open settings child window, if any: its OS window handle plus which
    /// panel it shows (app-wide settings or a specific tab's settings).
    settings_window: Option<(AnyWindowHandle, Panel)>,
    settings_view: Option<WeakEntity<SettingsView>>,
    /// The main window, where tabs live: child windows position themselves near
    /// it, and tab rebuilds bind their views to it (not to the settings window
    /// the rebuild was triggered from).
    main_window: AnyWindowHandle,
    /// Open popped-out chat windows (a channel mirrored into its own OS window,
    /// see [`popout`]). Session-only: closed on shutdown so they don't orphan,
    /// and never persisted (on restart every channel reopens in the main strip).
    /// Entries are removed when the user closes a window (observed release).
    popouts: Vec<AnyWindowHandle>,
    /// The live popped-out chat views by tab id: popouts aren't in [`tabs`](
    /// Self::tabs), so the filter refresh loops push to them separately (dead
    /// weak handles are pruned on each refresh).
    popout_views: Vec<(u64, WeakEntity<ChatView>)>,
    /// The popped-out global Mentions window, if open (only one at a time —
    /// re-triggering focuses it). Cleared when the user closes it.
    mentions_window: Option<AnyWindowHandle>,
    /// Watches the session so the account UI re-renders when login changes
    /// (e.g. after the browser OAuth round-trip completes).
    _login_watch: Task<()>,
    /// Whether a broadcast app (OBS etc.) was running at the last poll. Drives
    /// streamer mode when the setting is Auto.
    obs_running: bool,
    /// Polls the process list for broadcast software every
    /// [`streamer_mode::POLL_INTERVAL`].
    _obs_watch: Task<()>,
    /// Whether the "streamer mode is on" banner was ✕-dismissed. Session-only;
    /// reset each time streamer mode activates so the notice reappears.
    streamer_banner_dismissed: bool,
    /// Version of an update that has been downloaded and is ready to apply
    /// (drives the update banner). Set once by the update watch; Velopack also
    /// applies a pending update on the next normal launch, so dismissing the
    /// banner still updates eventually.
    update_ready: Option<String>,
    /// Whether the update banner was ✕-dismissed. Session-only.
    update_banner_dismissed: bool,
    /// The version this launch was updated to, when it is the first run after
    /// an update (drives the one-time "updated" banner; ✕ clears it).
    updated_to: Option<String>,
    /// Checks GitHub Releases for a newer build at launch and then every
    /// [`updater::CHECK_INTERVAL`]; ends once an update has been downloaded.
    _update_watch: Task<()>,
    /// The main window's current title ("Backseater - {active tab}"), memoized
    /// so render only calls `set_window_title` when it actually changes.
    window_title: String,
    /// The shared all-tabs mention feed every tab pushes into (see [`mentions`]).
    mention_store: Entity<MentionStore>,
    /// Whether the global Mentions tab (when enabled in settings) is selected
    /// instead of a normal tab. Session-only; selecting any tab clears it.
    mentions_tab_selected: bool,
    /// Scroll position of the global Mentions tab's feed (tailed like the panels).
    mentions_feed: mentions::FeedList,
    /// A mention arrived while the Mentions tab wasn't the active view. Drives
    /// the bold-name unread cue on its chip (like a normal tab's `unread`);
    /// cleared when the Mentions tab is selected.
    mentions_unread: bool,
    /// Mention-store subscriptions: row clicks → select the source tab, and
    /// new mentions → tail + repaint the global tab.
    _mention_subs: Vec<Subscription>,
}

impl gpui::EventEmitter<settings_ui::SettingsChanged> for BackseaterApp {}

impl BackseaterApp {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Loads saved logins synchronously so tabs connect authenticated from
        // their first join (no anonymous→authed reconnect at startup). Each tab
        // announces the current login state itself when it registers.
        let session = Session::new(
            bridge::runtime().handle().clone(),
            bks_auth::twitch::client_id(),
        );

        let settings = Settings::load();
        // Apply the persisted 7TV-cosmetics toggle process-wide before any tab
        // connects, so the bridge resolves (or skips) paints/badges accordingly.
        bks_emotes::set_paints_enabled(settings.show_7tv_paints);
        // Same for the pinned-banner + status-bar visibility the chat views read.
        settings.apply_visibility_flags();
        // And the mention-sound master + streamer-mute flags the play path reads.
        settings.apply_sound_flags();
        // And the mod-button strip's mode + custom buttons the chat rows read.
        settings.apply_mod_buttons();
        // Apply the persisted color theme to both the kit (window chrome, buttons,
        // settings) and the chat-log palette (via the bks-core flag `render` reads).
        apply_theme(&settings, window, cx);
        // And the persisted font family (the kit Root sets it window-wide).
        apply_font(settings.font_family.as_deref(), cx);

        // Remember the window's position/size (and maximized state) so the next
        // launch reopens it where the user left it.
        cx.observe_window_bounds(window, |_, window, cx| {
            window_state::main_changed(window.window_bounds(), cx);
        })
        .detach();

        // Seed streamer mode before any UI renders: one synchronous process scan
        // (a Toolhelp snapshot, cheap) so a launch while OBS is already open
        // starts hidden, not 20s later.
        let obs_running = streamer_mode::broadcast_software_running();
        let streamer_active = match settings.streamer_mode {
            settings::StreamerModeChoice::On => true,
            settings::StreamerModeChoice::Off => false,
            settings::StreamerModeChoice::Auto => obs_running,
        };
        streamer_mode::set_active(streamer_active);
        if streamer_active {
            tracing::info!("streamer mode enabled at launch");
        }

        // Initial mention terms: logged-in account names + custom terms. Kept in
        // sync afterward by the login watch and settings edits.
        let state = session.login_state();
        // The global mention terms (account names + app-wide custom terms) and the
        // global ignore list. Each tab unions the globals with its OWN per-tab
        // terms below; the global ignore is also published for the shared models to
        // drop against at ingest.
        let global_mention_terms: Vec<String> = state
            .twitch
            .into_iter()
            .chain(state.kick)
            .chain(settings.custom_mentions.iter().cloned())
            .collect();
        crate::settings::set_global_ignore(bks_core::IgnoreList::new(
            settings.ignored_terms.iter().cloned(),
        ));

        // The shared mention feed, created before the tabs so each view can
        // push into (and observe) it from birth.
        let mention_store = cx.new(|_| MentionStore::default());
        let _mention_subs = vec![
            // A clicked mention row: select its source tab, then jump that view
            // to the mentioned message (flash it, or note it's aged out). Gone
            // tab = no-op. Done synchronously so the active tab's first render
            // already has tail-follow disengaged + the reveal set — no bounce to
            // the bottom before the jump lands. The list state is intact across
            // the tab switch (background tabs stay connected; `select_tab` only
            // flips the active index), so no fresh layout is needed first.
            cx.subscribe(&mention_store, |this, _, ev: &mentions::ActivateTab, cx| {
                let Some(ix) = this.tabs.iter().position(|t| t.id == ev.tab_id) else {
                    return;
                };
                this.select_tab(ix, cx);
                let view = this.tabs[ix].view.clone();
                let platform = ev.platform;
                let msg_id = ev.msg_id.clone();
                view.update(cx, |view, cx| {
                    view.jump_to_message(platform, &msg_id, cx);
                });
            }),
            // Tail + repaint the global Mentions tab when a mention arrives.
            cx.observe(&mention_store, |this, _, cx| {
                // Mark the Mentions chip unread unless its feed is what's showing.
                if !(this.settings.mentions_tab && this.mentions_tab_selected) {
                    this.mentions_unread = true;
                }
                cx.notify();
            }),
        ];

        let tabs: Vec<TabEntry> = tabs::load()
            .into_iter()
            .map(|config| {
                // This tab's matcher/filter = globals ∪ this tab's own terms.
                let mentions = mention_matcher(
                    global_mention_terms
                        .iter()
                        .cloned()
                        .chain(config.custom_mentions.iter().cloned()),
                    &settings.muted_mentions,
                );
                let ignore = bks_core::IgnoreList::new(config.ignored_terms.iter().cloned());
                let suppress = bks_core::SuppressList::new(
                    settings
                        .suppressed_terms
                        .iter()
                        .chain(config.suppressed_terms.iter())
                        .cloned(),
                );
                Self::make_tab(
                    &session,
                    config,
                    settings.font_size,
                    mentions,
                    ignore,
                    suppress,
                    next_tab_id(),
                    &mention_store,
                    window,
                    cx,
                )
            })
            .collect();
        // Restore the last-active tab, clamped in case the tab it pointed at is
        // gone (tabs.json edited or a tab removed since).
        let active = tabs::load_active(tabs.len());

        // Re-render when login state changes (so the account dialog's
        // login/logout buttons update after an OAuth round-trip from any cause).
        let mut rx = session.subscribe();
        let _login_watch = cx.spawn(async move |weak, cx| {
            while rx.changed().await.is_ok() {
                // Login names feed mention highlighting, so refresh it too, and
                // the personal Twitch emote set (cross-channel sub emotes) is
                // per-account — drop it so the next picker/`:` refetches.
                let ok = weak.update(cx, |this, cx| {
                    this.refresh_mentions(cx);
                    cx.emit(settings_ui::SettingsChanged);
                    for tab in &this.tabs {
                        tab.view
                            .update(cx, |view, cx| view.refresh_personal_emotes(cx));
                    }
                    cx.notify();
                });
                if ok.is_err() {
                    break;
                }
            }
        });

        // Poll for broadcast software so Auto streamer mode follows OBS opening
        // and closing. Runs regardless of the current setting (so switching to
        // Auto applies instantly and the settings panel can show the detection
        // state); the scan itself runs off the main thread.
        let _obs_watch = cx.spawn(async move |weak, cx| loop {
            cx.background_executor()
                .timer(streamer_mode::POLL_INTERVAL)
                .await;
            let running = cx
                .background_executor()
                .spawn(async { streamer_mode::broadcast_software_running() })
                .await;
            tracing::debug!("broadcast-software poll: running={running}");
            let ok = weak.update(cx, |this, cx| this.set_obs_running(running, cx));
            if ok.is_err() {
                break;
            }
        });

        // Point the updater at the persisted channel before the first check.
        updater::set_beta_updates(settings.beta_updates);
        let _update_watch = Self::spawn_update_watch(cx);

        // Focus the restored active tab's composer so Ctrl+F / typing work
        // right from launch (key events only dispatch along the focus path).
        if let Some(tab) = tabs.get(active) {
            tab.view.update(cx, |v, cx| v.focus_composer(window, cx));
        }

        Self {
            session,
            tabs,
            active,
            dragging: None,
            chip_tip: None,
            chip_hovered: None,
            chip_tip_hovered: false,
            chip_tip_gen: 0,
            settings,
            #[cfg(test)]
            persistence_enabled: true,

            settings_window: None,
            settings_view: None,
            main_window: window.window_handle(),
            popouts: Vec::new(),
            popout_views: Vec::new(),
            mentions_window: None,
            _login_watch,
            obs_running,
            _obs_watch,
            streamer_banner_dismissed: false,
            update_ready: None,
            update_banner_dismissed: false,
            updated_to: updater::just_updated_to(),
            _update_watch,
            window_title: String::new(),
            mention_store,
            mentions_tab_selected: false,
            mentions_feed: mentions::FeedList::default(),
            mentions_unread: false,
            _mention_subs,
        }
    }

    #[allow(clippy::too_many_arguments)] // A constructor threading app context.
    fn make_tab(
        session: &Session,
        config: TabConfig,
        font_size: f32,
        mentions: bks_core::MentionMatcher,
        ignore: bks_core::IgnoreList,
        suppress: bks_core::SuppressList,
        id: u64,
        mention_store: &Entity<MentionStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TabEntry {
        let view_config = config.clone();
        let session = session.clone();
        let mention_store = mention_store.clone();
        let view = cx.new(|cx| {
            ChatView::new(
                session,
                view_config,
                font_size,
                mentions,
                ignore,
                suppress,
                id,
                mention_store,
                window,
                cx,
            )
        });
        let _activity_sub = Self::subscribe_tab_activity(&view, id, cx);
        let _live_sub = Self::subscribe_tab_live(&view, id, cx);
        let _activate_sub = Self::subscribe_tab_activate(&view, id, cx);
        TabEntry {
            id,
            config,
            view,
            unread: false,
            flash_start: None,
            _activity_sub,
            _live_sub,
            _activate_sub,
        }
    }

    /// Subscribes to a tab view's [`TabActivity`] so new chat/events mark the
    /// owning tab (found by stable `id`) unread when it isn't the active one.
    /// Keyed by id, not index, so it survives reorders; keyed on the view means
    /// it survives a channel-swap rebuild (the view entity is stable).
    fn subscribe_tab_activity(
        view: &Entity<ChatView>,
        id: u64,
        cx: &mut Context<Self>,
    ) -> gpui::Subscription {
        cx.subscribe(view, move |this, _view, _ev: &chatview::TabActivity, cx| {
            let Some(ix) = this.tabs.iter().position(|t| t.id == id) else {
                return;
            };
            // The active tab (and only when a real tab, not the mentions feed,
            // is showing) is considered read as messages arrive.
            if ix == this.active && !this.mentions_tab_selected {
                return;
            }
            if !this.tabs[ix].unread {
                this.tabs[ix].unread = true;
                cx.notify();
            }
        })
    }

    /// Subscribes to a tab view's `ActivateRequested` (a clicked chat-search
    /// result) so the owning tab is re-selected before the jump is seen, in
    /// case the user switched tabs after opening the search window. Same
    /// stable-`id` keying as [`subscribe_tab_activity`]; `select_tab` also
    /// leaves the mentions feed if it's showing.
    fn subscribe_tab_activate(
        view: &Entity<ChatView>,
        id: u64,
        cx: &mut Context<Self>,
    ) -> gpui::Subscription {
        cx.subscribe(
            view,
            move |this, _view, _ev: &chatview::ActivateRequested, cx| {
                let Some(ix) = this.tabs.iter().position(|t| t.id == id) else {
                    return;
                };
                this.select_tab(ix, cx);
            },
        )
    }

    /// Subscribes to a tab view's [`TabWentLive`] so a channel going live
    /// briefly flashes the owning tab's chip (when the setting is on). Same
    /// stable-`id` / stable-view keying as [`subscribe_tab_activity`].
    fn subscribe_tab_live(
        view: &Entity<ChatView>,
        id: u64,
        cx: &mut Context<Self>,
    ) -> gpui::Subscription {
        cx.subscribe(view, move |this, _view, ev: &chatview::TabWentLive, cx| {
            if !this.settings.flash_tab_on_live {
                return;
            }
            let Some(ix) = this.tabs.iter().position(|t| t.id == id) else {
                return;
            };
            this.tabs[ix].flash_start = Some((std::time::Instant::now(), ev.platform));
            this.schedule_tab_flash_tick(cx);
            cx.notify();
        })
    }

    /// Drives the chip-flash animation: repaints on a coalesced timer while any
    /// tab is still within its flash window, clearing each tab's `flash_start`
    /// once it elapses. Repaint-only (the flash is chip chrome — no log touch),
    /// self-arming, and a no-op once nothing is flashing.
    fn schedule_tab_flash_tick(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TAB_FLASH_TICK).await;
            this.update(cx, |this, cx| {
                let mut any = false;
                for tab in &mut this.tabs {
                    if let Some((start, _)) = tab.flash_start {
                        if start.elapsed() >= TAB_FLASH_DURATION {
                            tab.flash_start = None;
                        } else {
                            any = true;
                        }
                    }
                }
                if any {
                    this.schedule_tab_flash_tick(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// One tab's effective mention matcher: the logged-in Twitch/Kick account
    /// names + the **global** custom terms + this **tab's own** custom terms
    /// (union). Built fresh so it tracks login + settings + tab-config changes.
    fn tab_mentions(&self, config: &TabConfig) -> bks_core::MentionMatcher {
        let state = self.session.login_state();
        let terms = state
            .twitch
            .into_iter()
            .chain(state.kick)
            .chain(self.settings.custom_mentions.iter().cloned())
            .chain(config.custom_mentions.iter().cloned());
        mention_matcher(terms, &self.settings.muted_mentions)
    }

    /// The **global** ignore list, compiled from the app-wide ignored terms. This
    /// drives the process-wide accessor the shared channel models drop against at
    /// ingest; per-tab ignore is separate ([`tab_ignore`](Self::tab_ignore)).
    fn effective_ignore(&self) -> bks_core::IgnoreList {
        bks_core::IgnoreList::new(self.settings.ignored_terms.iter().cloned())
    }

    /// One tab's own (per-tab) ignore list — applied at render in that view only,
    /// so a message it hides stays in the shared buffer for other tabs. The global
    /// list is applied separately at ingest.
    fn tab_ignore(&self, config: &TabConfig) -> bks_core::IgnoreList {
        bks_core::IgnoreList::new(config.ignored_terms.iter().cloned())
    }

    /// One tab's effective suppress list: the global suppressed terms unioned
    /// with that tab's own. Suppression is *never* dropped at ingest (the row
    /// must still render, dimmed), so there is no global/per-tab split like
    /// ignore has — both tiers resolve together at render, per view.
    fn tab_suppress(&self, config: &TabConfig) -> bks_core::SuppressList {
        bks_core::SuppressList::new(
            self.settings
                .suppressed_terms
                .iter()
                .chain(config.suppressed_terms.iter())
                .cloned(),
        )
    }

    /// Pushes each tab its effective mention matcher (global + that tab's own
    /// terms) after a login, logout, or settings/tab-config edit.
    fn refresh_mentions(&mut self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            let matcher = self.tab_mentions(&tab.config);
            tab.view
                .update(cx, |view, cx| view.set_mentions(matcher, cx));
        }
    }

    /// Updates the process-wide global ignore (dropped at ingest by the shared
    /// models) and pushes each tab its own per-tab ignore (applied at render).
    fn refresh_ignore(&mut self, cx: &mut Context<Self>) {
        crate::settings::set_global_ignore(self.effective_ignore());
        for tab in &self.tabs {
            let ignore = self.tab_ignore(&tab.config);
            tab.view.update(cx, |view, cx| {
                // A per-tab change hides/reveals already-buffered rows now
                // (set_ignore re-measures so hidden rows collapse cleanly); a
                // global-ignore change affects future messages. Repaint either way.
                view.set_ignore(ignore, cx);
                view.refresh_log(cx);
            });
        }
        self.refresh_popout_filters(cx);
    }

    /// Pushes each tab its effective suppress list (global + that tab's own) and
    /// repaints the log so already-buffered rows re-dim. No `list_state` reset:
    /// suppressed rows keep full height (only opacity changes), so a repaint
    /// suffices — no re-measure like a font/pane change needs.
    fn refresh_suppress(&mut self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            let suppress = self.tab_suppress(&tab.config);
            tab.view.update(cx, |view, cx| {
                view.set_suppress(suppress, cx);
                view.refresh_log(cx);
            });
        }
        self.refresh_popout_filters(cx);
    }

    /// Re-pushes both filter lists to popped-out views, which aren't in
    /// [`tabs`](Self::tabs) and would otherwise keep the lists they were opened
    /// with (editing terms in settings never reached them). A popout whose tab
    /// has since closed keeps its last lists.
    fn refresh_popout_filters(&mut self, cx: &mut Context<Self>) {
        self.popout_views
            .retain(|(_, weak)| weak.upgrade().is_some());
        for (tab_id, weak) in &self.popout_views {
            let (Some(view), Some(tab)) =
                (weak.upgrade(), self.tabs.iter().find(|t| t.id == *tab_id))
            else {
                continue;
            };
            let ignore = self.tab_ignore(&tab.config);
            let suppress = self.tab_suppress(&tab.config);
            view.update(cx, |view, cx| {
                view.set_ignore(ignore, cx);
                view.set_suppress(suppress, cx);
                view.refresh_log(cx);
            });
        }
    }

    /// Tracks a popped-out view so [`refresh_popout_filters`](
    /// Self::refresh_popout_filters) can reach it (popouts aren't in `tabs`).
    fn track_popout_view(&mut self, tab_id: u64, view: &Entity<ChatView>) {
        self.popout_views.push((tab_id, view.downgrade()));
    }

    fn persistence_enabled(&self) -> bool {
        #[cfg(test)]
        {
            self.persistence_enabled
        }
        #[cfg(not(test))]
        {
            true
        }
    }

    fn save_settings(&self, cx: &mut Context<Self>) {
        if self.persistence_enabled() {
            self.settings.save();
        }
        cx.emit(settings_ui::SettingsChanged);
    }

    fn persist(&self, cx: &mut Context<Self>) {
        if !self.persistence_enabled() {
            cx.emit(settings_ui::SettingsChanged);
            return;
        }
        let configs: Vec<TabConfig> = self.tabs.iter().map(|t| t.config.clone()).collect();
        tabs::save(&configs);
        // The active index can shift with any structural change (add/close/move),
        // so save it alongside the list.
        tabs::save_active(self.active);
        cx.emit(settings_ui::SettingsChanged);
    }

    /// Pulls each tab's live view-owned layout (divider drags, header-arrow
    /// moves) back into the persisted config, saving if any changed. Cheap: a
    /// compare per tab, only writing on an actual change.
    fn sync_layouts(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        for tab in &mut self.tabs {
            if *tab.view.read(cx).layout() != tab.config.layout {
                tab.config.layout = tab.view.read(cx).layout().clone();
                changed = true;
            }
        }
        if changed {
            self.persist(cx);
        }
    }

    fn add_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let config = TabConfig::empty();
        let mentions = self.tab_mentions(&config);
        let ignore = self.tab_ignore(&config);
        let suppress = self.tab_suppress(&config);
        let entry = Self::make_tab(
            &self.session,
            config,
            self.settings.font_size,
            mentions,
            ignore,
            suppress,
            next_tab_id(),
            &self.mention_store,
            window,
            cx,
        );
        self.tabs.push(entry);
        self.active = self.tabs.len() - 1;
        self.mentions_tab_selected = false;
        self.persist(cx);
        cx.notify();
    }

    /// Asks for confirmation before closing tab `ix`, then closes it on OK.
    fn confirm_close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 {
            return; // Keep at least one tab — nothing to confirm.
        }
        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        let name = tab.config.display_name();
        let app = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let app = app.clone();
            alert
                .confirm()
                .title("Close tab?")
                .description(format!("Close \"{name}\"?"))
                .on_ok(move |_, _, cx| {
                    app.update(cx, |app, cx| app.close_tab(ix, cx));
                    true
                })
        });
    }

    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 {
            return; // Keep at least one tab.
        }
        let removed = self.tabs.remove(ix);
        // Drop its mentions so the shared feed doesn't offer dead jumps.
        self.mention_store
            .update(cx, |store, cx| store.remove_tab(removed.id, cx));
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
        self.persist(cx);
        cx.notify();
    }

    fn select_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.tabs.len() {
            self.active = ix;
            self.mentions_tab_selected = false;
            self.tabs[ix].unread = false;
            tabs::save_active(self.active);
            // Focus the now-active tab's composer so typing / Ctrl+F work
            // without clicking into the view first (the old tab's focused
            // input is no longer rendered, leaving keys dispatching nowhere).
            // Deferred: callers run inside the main window's own listeners.
            let view = self.tabs[ix].view.clone();
            let main_window = self.main_window;
            cx.defer(move |cx| {
                let _ = main_window.update(cx, |_, window, cx| {
                    view.update(cx, |v, cx| v.focus_composer(window, cx));
                });
            });
            cx.notify();
        }
    }

    /// A chip's hover state changed: schedule the tooltip to show after the
    /// usual delay (re-validated at fire time), or to hide after a short grace
    /// (kept if the pointer moved onto the tooltip panel, so its links are
    /// clickable — or back onto the chip).
    fn chip_hover_changed(&mut self, ix: usize, hovered: bool, cx: &mut Context<Self>) {
        if hovered {
            self.chip_hovered = Some(ix);
            if self.chip_tip == Some(ix) {
                return; // already showing this chip's tooltip
            }
            let gen = self.chip_tip_gen;
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(CHIP_TIP_SHOW_DELAY).await;
                this.update(cx, |this, cx| {
                    if this.chip_tip_gen == gen && this.chip_hovered == Some(ix) {
                        this.chip_tip = Some(ix);
                        this.chip_tip_hovered = false;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        } else {
            if self.chip_hovered == Some(ix) {
                self.chip_hovered = None;
            }
            self.schedule_chip_tip_hide(cx);
        }
    }

    /// Hides the tooltip once the grace elapses, unless the pointer is back over
    /// the showing chip or over the tooltip itself (both re-checked at fire time,
    /// so a quick leave-and-return keeps it up with no timer bookkeeping).
    fn schedule_chip_tip_hide(&mut self, cx: &mut Context<Self>) {
        if self.chip_tip.is_none() {
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CHIP_TIP_HIDE_GRACE).await;
            this.update(cx, |this, cx| {
                if this.chip_tip.is_some()
                    && !this.chip_tip_hovered
                    && this.chip_hovered != this.chip_tip
                {
                    this.chip_tip = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Dismisses the tooltip immediately (any click on a chip: select, context
    /// menu, close, drag) and invalidates pending show timers, so it can't
    /// reappear until the pointer re-enters a chip.
    fn dismiss_chip_tip(&mut self, cx: &mut Context<Self>) {
        self.chip_tip_gen = self.chip_tip_gen.wrapping_add(1);
        if self.chip_tip.take().is_some() {
            cx.notify();
        }
    }

    /// Reorders the tab list, moving the tab at `from` to sit at `to`, keeping
    /// the same tab selected.
    fn move_tab(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return;
        }
        let entry = self.tabs.remove(from);
        self.tabs.insert(to, entry);
        // Follow the active tab through the shuffle.
        self.active = if self.active == from {
            to
        } else if from < self.active && self.active <= to {
            self.active - 1
        } else if to <= self.active && self.active < from {
            self.active + 1
        } else {
            self.active
        };
        self.persist(cx);
        cx.notify();
    }

    /// Opens the settings window for tab `ix`, pre-filled with its current values.
    fn open_settings(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.tabs.get(ix).is_none() {
            return;
        }
        self.show_settings_panel(Panel::Tab(ix), cx);
    }

    /// Pops tab `ix` out into its own OS window: a
    /// second, independent [`ChatView`] on the same channel (shared buffer +
    /// connection via `channel_store`). The tab stays in the main strip — this
    /// is a mirror, not a move — so closing the popout only drops the extra view.
    /// Deferred to a task: opening a window draws it synchronously and building
    /// the popout's `ChatView` must happen off any leased entity/window.
    fn pop_out_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        let config = tab.config.clone();
        if !config.has_channel() {
            return; // Nothing to show — an unconfigured tab has no feed.
        }
        let params = popout::PopoutParams {
            session: self.session.clone(),
            mentions: self.tab_mentions(&config),
            ignore: self.tab_ignore(&config),
            suppress: self.tab_suppress(&config),
            font_size: self.settings.font_size,
            tab_id: tab.id,
            mention_store: self.mention_store.clone(),
            config,
        };
        let app = cx.entity();
        cx.spawn(async move |_, cx| {
            cx.update(|cx| Self::open_popout(app, params, cx));
        })
        .detach();
    }

    /// The deferred half of [`pop_out_tab`], run from a plain `App` context (no
    /// entity lease, no window borrowed).
    fn open_popout(app: Entity<Self>, params: popout::PopoutParams, cx: &mut App) {
        let tab_id = params.tab_id;
        // Center over the main window, on its display (the display id must travel
        // with the bounds — see `child_window::open_owned`).
        let (parent, display) = child_window::parent_bounds(app.read(cx).main_window, cx);
        let bounds = child_window::centered_on(parent, popout::POPOUT_WINDOW_SIZE);
        let Ok((handle, content)) = popout::open(params, bounds, display, cx) else {
            return;
        };
        app.update(cx, |this, cx| {
            let view = content.read(cx).view().clone();
            this.track_popout_view(tab_id, &view);
            this.popouts.push(handle);
            // Drop the handle when the user closes the window (OS ✕) so the list
            // doesn't accumulate stale handles across a session.
            cx.observe_release(&content, move |this, _, _| {
                this.popouts.retain(|h| *h != handle);
            })
            .detach();
        });
    }

    /// Applies + persists a new global Mentions tab name (empty = the default),
    /// retitling the popped-out Mentions window if open.
    fn set_mentions_tab_name(&mut self, value: String, cx: &mut Context<Self>) {
        let name = value.trim();
        let name = (!name.is_empty()).then(|| name.to_string());
        if name == self.settings.mentions_tab_name {
            return;
        }
        self.settings.mentions_tab_name = name;
        self.save_settings(cx);
        if let Some(handle) = self.mentions_window {
            let title = self.mentions_window_title();
            handle
                .update(cx, |_, window, _| window.set_window_title(&title))
                .ok();
        }
        cx.notify();
    }

    /// The Mentions views' window title ("Backseater - {name}"), following the
    /// custom tab name.
    pub(crate) fn mentions_window_title(&self) -> String {
        format!("Backseater - {}", self.settings.mentions_tab_label())
    }

    /// Closes the global Mentions tab — same as unchecking "Show a Mentions tab"
    /// in Highlights settings (the ✕ on the chip is a shortcut for it).
    fn close_mentions_tab(&mut self, cx: &mut Context<Self>) {
        self.settings.mentions_tab = false;
        self.mentions_tab_selected = false;
        self.save_settings(cx);
        cx.notify();
    }

    /// Pops the global Mentions feed out into its own OS window (or focuses it if
    /// already open). Deferred like [`pop_out_tab`] — windows must open from a
    /// plain `App` context, not a leased listener.
    fn pop_out_mentions(&mut self, cx: &mut Context<Self>) {
        let app = cx.entity();
        cx.spawn(async move |_, cx| {
            cx.update(|cx| Self::open_mentions_window(app, cx));
        })
        .detach();
    }

    /// The deferred half of [`pop_out_mentions`], run from a plain `App` context.
    fn open_mentions_window(app: Entity<Self>, cx: &mut App) {
        // Only one Mentions window: focus the existing one if it's still open.
        if let Some(handle) = app.read(cx).mentions_window {
            if child_window::focus_existing(handle, None, cx) {
                return;
            }
            // The window closed under us — fall through and open a fresh one.
        }
        let (parent, display) = child_window::parent_bounds(app.read(cx).main_window, cx);
        let bounds = child_window::centered_on(parent, popout::MENTIONS_WINDOW_SIZE);
        let Ok((handle, content)) = popout::open_mentions(app.clone(), bounds, display, cx) else {
            return;
        };
        app.update(cx, |this, cx| {
            this.mentions_window = Some(handle);
            cx.observe_release(&content, move |this, _, _| {
                if this.mentions_window == Some(handle) {
                    this.mentions_window = None;
                }
            })
            .detach();
        });
    }

    /// Shows `panel` in the settings child window (opening it if needed,
    /// re-pointing + focusing it if already open). Deferred to a task because
    /// opening a window draws it synchronously and that draw re-enters this
    /// entity for the body — which would double-lease it from inside a listener.
    fn show_settings_panel(&mut self, panel: Panel, cx: &mut Context<Self>) {
        let app = cx.entity();
        cx.spawn(async move |_, cx| {
            cx.update(|cx| Self::show_settings_window(app, panel, cx));
        })
        .detach();
    }

    /// The deferred half of [`show_settings_panel`], run from a plain `App`
    /// context (no entity lease, no window borrowed).
    fn show_settings_window(app: Entity<Self>, panel: Panel, cx: &mut App) {
        let existing = app.read(cx).settings_window;
        let view = app
            .read(cx)
            .settings_view
            .as_ref()
            .and_then(WeakEntity::upgrade);
        if let (Some((handle, _)), Some(view)) = (existing, view) {
            if handle
                .update(cx, |_, window, cx| {
                    window.set_window_title(panel.title());
                    view.update(cx, |view, cx| view.set_panel(panel, window, cx));
                    app.update(cx, |app, _| app.settings_window = Some((handle, panel)));
                    window.activate_window();
                })
                .is_ok()
            {
                return;
            }
        }
        let parent = app.read(cx).main_window;
        let host = app.clone();
        let Ok((handle, view)) = child_window::open_owned(
            panel.title(),
            settings_ui::SETTINGS_WINDOW_SIZE,
            settings_ui::SETTINGS_MIN_SIZE,
            parent,
            None,
            move |window, cx| SettingsView::new(host, panel, window, cx),
            cx,
        ) else {
            return;
        };
        app.update(cx, |app, cx| {
            app.settings_window = Some((handle, panel));
            app.settings_view = Some(view.downgrade());
            cx.observe_release(&view, move |app, _, cx| {
                if app.settings_window.map(|(handle, _)| handle) == Some(handle) {
                    app.settings_window = None;
                    app.settings_view = None;
                }
                cx.notify();
            })
            .detach();
            cx.notify();
        });
    }

    /// Applies edited settings to tab `ix`: renames it and, if its channels
    /// changed, rebuilds the feed (a fresh connection), then persists. The editor
    /// passes its draft; other tab preferences retain their current live values.
    fn apply_settings(&mut self, ix: usize, draft: TabConfig, cx: &mut Context<Self>) {
        let name = draft.name;
        let twitch = draft.twitch_channel;
        let kick = draft.kick_channel;
        let youtube = draft.youtube_channel;
        let tiktok = draft.tiktok_channel;

        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        let channels_changed = tab.config.twitch_channel != twitch
            || tab.config.kick_channel != kick
            || tab.config.youtube_channel != youtube
            || tab.config.tiktok_channel != tiktok;
        // Adding or removing platforms (no channel *replaced* by a different
        // one)? Then the tab reconnects in place and keeps its log — the other
        // platforms shouldn't visibly drop and reload, and a removed platform's
        // rows stay as scrollback. Only swapping a channel for a different one
        // rebuilds from scratch (a different channel means a different log).
        let keep_log = channel_kept(&tab.config.twitch_channel, &twitch)
            && channel_kept(&tab.config.kick_channel, &kick)
            && channel_kept(&tab.config.youtube_channel, &youtube)
            && channel_kept(&tab.config.tiktok_channel, &tiktok);

        let mut config = tab.config.clone();
        config.twitch_channel = twitch;
        config.kick_channel = kick;
        config.youtube_channel = youtube;
        config.tiktok_channel = tiktok;
        // Store the name verbatim (blank if unset); the tab strip falls back to
        // the channel name via `display_name`.
        config.name = name;

        if channels_changed && keep_log {
            self.tabs[ix].config = config.clone();
            self.tabs[ix]
                .view
                .update(cx, |view, cx| view.reconnect(config, cx));
        } else if channels_changed {
            // Rebuild the tab's view on a fresh connection to the new channels.
            // The view is created against the main window (where tabs render),
            // not the settings window this runs from — kit inputs and window
            // subscriptions bind to the window they're created in.
            let mentions = self.tab_mentions(&config);
            let ignore = self.tab_ignore(&config);
            let suppress = self.tab_suppress(&config);
            let session = self.session.clone();
            let font_size = self.settings.font_size;
            // The rebuilt view keeps the tab's id, so its recorded mentions
            // still jump here.
            let id = self.tabs[ix].id;
            let store = self.mention_store.clone();
            let view_config = config.clone();
            // Build the view where the main `Window` is reachable; the
            // `TabActivity` subscription is wired below, back in `Context<Self>`.
            let Ok(view) = self.main_window.update(cx, |_, window, cx| {
                cx.new(|cx| {
                    ChatView::new(
                        session,
                        view_config,
                        font_size,
                        mentions,
                        ignore,
                        suppress,
                        id,
                        store,
                        window,
                        cx,
                    )
                })
            }) else {
                return; // Main window gone (app shutting down).
            };
            let _activity_sub = Self::subscribe_tab_activity(&view, id, cx);
            let _live_sub = Self::subscribe_tab_live(&view, id, cx);
            let _activate_sub = Self::subscribe_tab_activate(&view, id, cx);
            self.tabs[ix] = TabEntry {
                id,
                config,
                view,
                unread: false,
                flash_start: None,
                _activity_sub,
                _live_sub,
                _activate_sub,
            };
        } else {
            self.tabs[ix].config = config;
        }
        self.persist(cx);
        cx.notify();
    }

    /// Shows/hides panel `kind` in tab `ix`'s layout, pushing the new layout to
    /// the live view and persisting. Applies immediately (no Save), matching the
    /// settings checklist's live toggles.
    fn set_panel_shown(
        &mut self,
        ix: usize,
        kind: tabs::PanelKind,
        show: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get_mut(ix) else {
            return;
        };
        tab.config.layout.set_enabled(kind, show);
        let layout = tab.config.layout.clone();
        tab.view.update(cx, |view, cx| view.set_layout(layout, cx));
        self.persist(cx);
        cx.notify();
    }

    /// Toggles whether tab `ix`'s mentions panel shows every tab's mentions
    /// (the shared feed) instead of just its own. Applies live and persists.
    fn set_mentions_all_tabs(&mut self, ix: usize, all: bool, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(ix) else {
            return;
        };
        tab.config.mentions_all_tabs = all;
        tab.view
            .update(cx, |view, cx| view.set_mentions_all(all, cx));
        self.persist(cx);
        cx.notify();
    }

    /// Runs `f` on tab `ix`'s main-strip view and every live popout mirroring
    /// it (popouts aren't in [`tabs`](Self::tabs)), pruning dead popout handles
    /// first — the one fan-out path for pushing per-tab view config, so a
    /// popout can't keep acting on a stale copy.
    fn update_tab_views(
        &mut self,
        ix: usize,
        cx: &mut Context<Self>,
        f: impl Fn(&mut ChatView, &mut Context<ChatView>),
    ) {
        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        let tab_id = tab.id;
        tab.view.update(cx, |view, cx| f(view, cx));
        self.popout_views
            .retain(|(_, weak)| weak.upgrade().is_some());
        for (id, weak) in &self.popout_views {
            if *id == tab_id {
                if let Some(view) = weak.upgrade() {
                    view.update(cx, |view, cx| f(view, cx));
                }
            }
        }
    }

    /// Pushes tab `ix`'s (already-mutated) events filters to its live views and
    /// persists — shared tail of the filter setters below.
    fn push_events_filter(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        let (kinds, only, hide_msgs, collapse) = (
            tab.config.event_kinds,
            tab.config.events_only,
            tab.config.hide_sub_messages,
            tab.config.collapse_gift_subs,
        );
        self.update_tab_views(ix, cx, move |view, cx| {
            view.set_events_filter(kinds, only, hide_msgs, collapse, cx)
        });
        self.persist(cx);
        cx.notify();
    }

    /// Toggles whether `kind` appears in tab `ix`'s events panel.
    fn set_event_kind(&mut self, ix: usize, kind: EventKind, on: bool, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(ix) else {
            return;
        };
        *tab.config.event_kinds.toggle_mut(kind) = on;
        self.push_events_filter(ix, cx);
    }

    /// Toggles whether `kind` plays the alert ping for tab `ix` (the bell
    /// toggles in the events-panel settings). The popout fan-out matters here:
    /// popouts share the channel model's one-ping-per-event claim, so a stale
    /// popout copy would keep pinging with the old setting.
    fn set_event_sound(&mut self, ix: usize, kind: EventKind, on: bool, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(ix) else {
            return;
        };
        *tab.config.event_sounds.toggle_mut(kind) = on;
        let sounds = tab.config.event_sounds;
        self.update_tab_views(ix, cx, move |view, _| view.set_event_sounds(sounds));
        self.persist(cx);
        cx.notify();
    }

    /// Toggles "events only" (hide events from the main log) for tab `ix`.
    fn set_events_only(&mut self, ix: usize, only: bool, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(ix) else {
            return;
        };
        tab.config.events_only = only;
        self.push_events_filter(ix, cx);
    }

    /// Toggles hiding sub/resub attached messages in tab `ix`'s events panel.
    fn set_hide_sub_messages(&mut self, ix: usize, hide: bool, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(ix) else {
            return;
        };
        tab.config.hide_sub_messages = hide;
        self.push_events_filter(ix, cx);
    }

    /// Toggles collapsing mass-gift batches in tab `ix`'s events panel.
    fn set_collapse_gift_subs(&mut self, ix: usize, collapse: bool, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(ix) else {
            return;
        };
        tab.config.collapse_gift_subs = collapse;
        self.push_events_filter(ix, cx);
    }

    /// Opens the app-wide settings window: an Account section (Twitch/Kick login)
    /// and an Appearance section (chat font size). Account actions run on the
    /// active tab's feed so progress/error notices show there. The window body is
    /// rebuilt from the app entity each render, so it reflects live login/size
    /// changes (the window stays open across an OAuth round-trip).
    fn open_app_settings(&mut self, cx: &mut Context<Self>) {
        match self.settings_window {
            // Toggle: clicking the gear again closes the window.
            Some((handle, Panel::App)) => {
                self.settings_window = None;
                self.settings_view = None;
                let _ = handle.update(cx, |_, window, _| window.remove_window());
                cx.notify();
            }
            // Closed, or showing a tab's settings: show (switch to) app settings.
            _ => self.show_settings_panel(Panel::App, cx),
        }
    }

    /// Opens app settings focused on the Account category (used by the title-bar
    /// login indicators). Unlike the gear it never toggles closed — clicking a
    /// login icon always lands you on Account.
    fn open_account_settings(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = self.settings_view.as_ref().and_then(WeakEntity::upgrade) {
            view.update(cx, |view, cx| view.show_account(cx));
        }
        if !matches!(self.settings_window, Some((_, Panel::App))) {
            self.show_settings_panel(Panel::App, cx);
        }
        cx.notify();
    }

    /// Applies a mod-button visibility change: persist, push the process-wide
    /// flag, and re-measure every log (the strip changes row widths, so wrapped
    /// heights change too).
    fn set_mod_button_mode(&mut self, mode: settings::ModButtonMode, cx: &mut Context<Self>) {
        if self.settings.mod_button_mode == mode {
            return;
        }
        self.settings.mod_button_mode = mode;
        self.save_mod_buttons(cx);
    }

    /// The shared tail of every mod-button edit: persist, push the process-wide
    /// state the rows render against, and re-measure every log (the strip
    /// changes row widths, so wrapped heights change too).
    fn save_mod_buttons(&mut self, cx: &mut Context<Self>) {
        self.save_settings(cx);
        self.settings.apply_mod_buttons();
        self.remeasure_tabs(cx);
        cx.notify();
    }

    /// Re-measures every tab's log — for process-wide changes that alter row
    /// layout outside the rows' own data (the mod-button strip).
    fn remeasure_tabs(&self, cx: &mut Context<Self>) {
        self.mentions_feed.remeasure();
        for tab in &self.tabs {
            tab.view.update(cx, |view, cx| view.remeasure(cx));
        }
    }

    /// Toggles the mention-ping master switch (Highlights settings).
    fn set_mention_sound(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.mention_sound = on;
        self.save_settings(cx);
        self.settings.apply_sound_flags();
        cx.notify();
    }

    /// Toggles whether active streamer mode silences mention pings.
    fn set_streamer_mute_sounds(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.streamer_mute_sounds = on;
        self.save_settings(cx);
        self.settings.apply_sound_flags();
        cx.notify();
    }

    fn set_streamer_hide_thumbnails(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.streamer_hide_thumbnails = on;
        self.save_settings(cx);
        self.settings.apply_visibility_flags();
        // The inline card is a fixed height with or without the thumbnail, so a
        // repaint (not a re-measure) suffices to add/drop the image.
        for tab in &self.tabs {
            tab.view.update(cx, |view, cx| {
                view.refresh_log(cx);
                view.refresh_panel_styles(cx);
            });
        }
        cx.notify();
    }

    /// Flips one mention term's sound (the chip 🔔/🔕). Stored app-wide in the
    /// matcher's normalized form, so muting a term mutes it in every scope.
    fn toggle_mention_mute(&mut self, term: &str, cx: &mut Context<Self>) {
        let norm = bks_core::normalize_term(term);
        let muted = &mut self.settings.muted_mentions;
        if let Some(ix) = muted.iter().position(|m| *m == norm) {
            muted.remove(ix);
        } else {
            muted.push(norm);
        }
        self.save_settings(cx);
        self.refresh_mentions(cx);
        cx.notify();
    }

    fn terms(&self, list: TermList) -> &Vec<String> {
        match (list.scope, list.kind) {
            (TermScope::Global, TermKind::Mentions) => &self.settings.custom_mentions,
            (TermScope::Global, TermKind::Ignore) => &self.settings.ignored_terms,
            (TermScope::Global, TermKind::Suppress) => &self.settings.suppressed_terms,
            (TermScope::Tab(ix), TermKind::Mentions) => &self.tabs[ix].config.custom_mentions,
            (TermScope::Tab(ix), TermKind::Ignore) => &self.tabs[ix].config.ignored_terms,
            (TermScope::Tab(ix), TermKind::Suppress) => &self.tabs[ix].config.suppressed_terms,
        }
    }

    fn terms_mut(&mut self, list: TermList) -> &mut Vec<String> {
        match (list.scope, list.kind) {
            (TermScope::Global, TermKind::Mentions) => &mut self.settings.custom_mentions,
            (TermScope::Global, TermKind::Ignore) => &mut self.settings.ignored_terms,
            (TermScope::Global, TermKind::Suppress) => &mut self.settings.suppressed_terms,
            (TermScope::Tab(ix), TermKind::Mentions) => &mut self.tabs[ix].config.custom_mentions,
            (TermScope::Tab(ix), TermKind::Ignore) => &mut self.tabs[ix].config.ignored_terms,
            (TermScope::Tab(ix), TermKind::Suppress) => &mut self.tabs[ix].config.suppressed_terms,
        }
    }

    /// Persists after an edit (global → settings.json, per-tab → tabs.json) and
    /// re-pushes the affected matcher/filter to the relevant view(s).
    fn refresh_terms(&mut self, list: TermList, cx: &mut Context<Self>) {
        match list.scope {
            TermScope::Global => self.save_settings(cx),
            TermScope::Tab(_) => self.persist(cx),
        }
        match list.kind {
            TermKind::Mentions => self.refresh_mentions(cx),
            TermKind::Ignore => self.refresh_ignore(cx),
            TermKind::Suppress => self.refresh_suppress(cx),
        }
    }

    fn add_terms(&mut self, list: TermList, entries: Vec<String>, cx: &mut Context<Self>) -> bool {
        if matches!(list.scope, TermScope::Tab(ix) if ix >= self.tabs.len()) {
            return false;
        }
        let mut added = false;
        for entry in entries {
            if let Some((None, name)) = bks_core::parse_user_entry(&entry) {
                let name = name.to_string();
                added |= bks_core::absorb_scoped_user_entries(self.terms_mut(list), &name);
            }
            if !self
                .terms(list)
                .iter()
                .any(|term| term.eq_ignore_ascii_case(&entry))
            {
                self.terms_mut(list).push(entry);
                added = true;
            }
        }
        if added {
            self.refresh_terms(list, cx);
            cx.notify();
        }
        added
    }

    /// Removes a term from the list, persists, and refreshes matching.
    fn remove_term(&mut self, list: TermList, term: &str, cx: &mut Context<Self>) {
        if matches!(list.scope, TermScope::Tab(ix) if ix >= self.tabs.len()) {
            return;
        }
        self.terms_mut(list).retain(|t| t != term);
        self.refresh_terms(list, cx);
        cx.notify();
    }

    /// The active tab's controller, used by the account settings actions.
    fn active_controller(&self, cx: &App) -> Option<Controller> {
        self.tabs
            .get(self.active)
            .map(|t| t.view.read(cx).controller().clone())
    }

    /// Changes the chat font size by `delta` px (clamped), persists it, and pushes
    /// the new size to every tab's view.
    fn adjust_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        let size = (self.settings.font_size + delta)
            .clamp(settings::MIN_FONT_SIZE, settings::MAX_FONT_SIZE);
        if size == self.settings.font_size {
            return;
        }
        self.settings.font_size = size;
        self.save_settings(cx);
        for tab in &self.tabs {
            tab.view.update(cx, |view, cx| view.set_font_size(size, cx));
        }
        cx.notify();
    }

    /// Steps the suppressed-message opacity by `delta`, clamped to the allowed
    /// range, then publishes the flag and repaints every log. No re-measure:
    /// opacity doesn't change row height (unlike font size).
    fn adjust_suppressed_opacity(&mut self, delta: f32, cx: &mut Context<Self>) {
        let opacity = (self.settings.suppressed_opacity + delta).clamp(
            *settings::SUPPRESSED_OPACITY_RANGE.start(),
            *settings::SUPPRESSED_OPACITY_RANGE.end(),
        );
        if opacity == self.settings.suppressed_opacity {
            return;
        }
        self.settings.suppressed_opacity = opacity;
        self.save_settings(cx);
        self.settings.apply_visibility_flags();
        for tab in &self.tabs {
            tab.view.update(cx, |view, cx| {
                view.refresh_log(cx);
                view.refresh_panel_styles(cx);
            });
        }
        cx.notify();
    }

    /// Changes the UI font family (`None` = system default), persists it, and
    /// applies it app-wide via the kit theme. Glyph metrics change row heights,
    /// so every tab's log re-measures.
    fn set_font_family(&mut self, family: Option<String>, cx: &mut Context<Self>) {
        if family == self.settings.font_family {
            return;
        }
        self.settings.font_family = family;
        self.save_settings(cx);
        apply_font(self.settings.font_family.as_deref(), cx);
        self.remeasure_tabs(cx);
        cx.notify();
    }

    /// Toggles 7TV name paints + badges. Persists, flips the process-wide gate (so
    /// the bridge starts/stops resolving cosmetics), and updates every tab live:
    /// turning it off strips paints/badges already applied to on-screen rows; on,
    /// they reappear as chatters speak again (or for messages still resolving).
    fn set_show_7tv_paints(&mut self, on: bool, cx: &mut Context<Self>) {
        if on == self.settings.show_7tv_paints {
            return;
        }
        self.settings.show_7tv_paints = on;
        self.save_settings(cx);
        bks_emotes::set_paints_enabled(on);
        if !on {
            for tab in &self.tabs {
                tab.view.update(cx, |view, cx| {
                    view.clear_cosmetics(cx);
                    cx.notify();
                });
            }
        }
        cx.notify();
    }

    /// Toggles the pinned-message banner for one platform. Persists, flips the
    /// process-wide flag the chat views render against, and repaints every tab
    /// (the banner lives outside the cached log, so a plain notify reaches it).
    fn set_show_pinned(&mut self, platform: bks_core::Platform, on: bool, cx: &mut Context<Self>) {
        let field = match platform {
            bks_core::Platform::Twitch => &mut self.settings.show_pinned_twitch,
            bks_core::Platform::Kick => &mut self.settings.show_pinned_kick,
            _ => return,
        };
        if *field == on {
            return;
        }
        *field = on;
        self.save_settings(cx);
        self.settings.apply_visibility_flags();
        for tab in &self.tabs {
            tab.view.update(cx, |_, cx| cx.notify());
        }
        cx.notify();
    }

    /// Toggles pause-on-hover. Persists + flips the process-wide flag; a view
    /// already paused resumes on its next hover-exit (checked at engage time).
    fn set_pause_chat_on_hover(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.settings.pause_chat_on_hover == on {
            return;
        }
        self.settings.pause_chat_on_hover = on;
        self.save_settings(cx);
        self.settings.apply_visibility_flags();
        cx.notify();
    }

    /// Toggles compact chat. Persists, flips the process-wide flag, and
    /// re-measures every tab's log — the per-row vertical padding changes, so
    /// the virtualized list's cached heights must be recomputed.
    fn set_compact_chat(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.settings.compact_chat == on {
            return;
        }
        self.settings.compact_chat = on;
        self.save_settings(cx);
        self.settings.apply_visibility_flags();
        self.remeasure_tabs(cx);
        cx.notify();
    }

    /// Toggles the go-live tab flash. Persists only — the flag is read per
    /// render in `tab_strip`, and the flash is armed from the `TabWentLive`
    /// subscription (which re-reads the setting each time), so turning it off
    /// just stops future flashes.
    fn set_flash_tab_on_live(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.settings.flash_tab_on_live == on {
            return;
        }
        self.settings.flash_tab_on_live = on;
        self.save_settings(cx);
        // Off should also clear any flash already in progress.
        if !on {
            for tab in &mut self.tabs {
                tab.flash_start = None;
            }
        }
        cx.notify();
    }

    /// Toggles the live status bar (viewer counts). Persists, flips the
    /// process-wide flag, and repaints every tab (the bar lives outside the
    /// cached log, so a plain notify reaches it).
    fn set_show_status_bar(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.settings.show_status_bar == on {
            return;
        }
        self.settings.show_status_bar = on;
        self.save_settings(cx);
        self.settings.apply_visibility_flags();
        for tab in &self.tabs {
            tab.view.update(cx, |_, cx| cx.notify());
        }
        cx.notify();
    }

    fn set_link_preview_mode(&mut self, mode: settings::LinkPreviewMode, cx: &mut Context<Self>) {
        if self.settings.link_preview_mode == mode {
            return;
        }
        let now_inline = mode == settings::LinkPreviewMode::Inline;
        self.settings.link_preview_mode = mode;
        self.save_settings(cx);
        self.settings.apply_visibility_flags();
        // Inline cards change row heights, so every tab's log must re-measure;
        // switching *to* Inline also arms fetches for already-buffered messages
        // (they arrived before inline was on, so their cards weren't armed).
        for tab in &self.tabs {
            tab.view.update(cx, |view, cx| {
                if now_inline {
                    view.arm_buffered_inline_previews(cx);
                }
                view.remeasure(cx);
            });
        }
        cx.notify();
    }

    fn set_chat_modes_placement(
        &mut self,
        placement: settings::ChatModesPlacement,
        cx: &mut Context<Self>,
    ) {
        if self.settings.chat_modes_placement == placement {
            return;
        }
        self.settings.chat_modes_placement = placement;
        self.save_settings(cx);
        self.settings.apply_visibility_flags();
        for tab in &self.tabs {
            tab.view.update(cx, |_, cx| cx.notify());
        }
        cx.notify();
    }

    /// Toggles a per-surface "show timestamps" setting (chat log / events panel /
    /// mentions panel). Persists, flips the process-wide flag, and re-measures
    /// every tab's log since hiding the timestamp changes row layout/wrap.
    fn set_show_timestamps(&mut self, surface: TimestampSurface, on: bool, cx: &mut Context<Self>) {
        let field = match surface {
            TimestampSurface::Chat => &mut self.settings.show_timestamps_chat,
            TimestampSurface::Events => &mut self.settings.show_timestamps_events,
            TimestampSurface::Mentions => &mut self.settings.show_timestamps_mentions,
        };
        if *field == on {
            return;
        }
        *field = on;
        self.save_settings(cx);
        self.settings.apply_visibility_flags();
        self.remeasure_tabs(cx);
        cx.notify();
    }

    /// Switches the app color theme (dark/light), persists it, and re-renders. The
    /// chat-log palette updates because `render` reads the process-wide flag
    /// `apply_theme` sets; every tab re-renders on the `notify`.
    fn set_theme(
        &mut self,
        choice: settings::ThemeChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if choice == self.settings.theme {
            return;
        }
        self.settings.theme = choice;
        self.save_settings(cx);
        self.reapply_theme(window, cx);
    }

    /// Re-applies the current theme (kit chrome + custom palette + font) and
    /// re-renders every tab's cached log. Called after a theme switch and after a
    /// live edit to the active custom theme's colors.
    fn reapply_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        apply_theme(&self.settings, window, cx);
        // `Theme::change` re-applies the theme config, which may carry its own
        // font — re-assert the user's choice like the surface-color overrides.
        apply_font(self.settings.font_family.as_deref(), cx);
        // Row colors depend on the palette. The log renders in a *cached* child
        // view, so it must be dirtied explicitly — a plain notify on the ChatView
        // doesn't reach it.
        for tab in &self.tabs {
            tab.view.update(cx, |view, cx| {
                view.refresh_log(cx);
                view.refresh_panel_styles(cx);
                cx.notify();
            });
        }
        cx.notify();
    }

    /// Re-applies the theme colors (custom palette + kit surfaces) and refreshes
    /// every cached log. Window-free variant of [`reapply_theme`](Self::reapply_theme),
    /// for a live color edit that doesn't flip the dark/light chrome mode.
    fn reapply_theme_colors(&mut self, cx: &mut Context<Self>) {
        apply_theme_surfaces(&self.settings, cx);
        for tab in &self.tabs {
            tab.view.update(cx, |view, cx| {
                view.refresh_log(cx);
                view.refresh_panel_styles(cx);
                cx.notify();
            });
        }
        cx.notify();
    }

    /// A poll result from the OBS watcher: updates the detection state and, in
    /// Auto mode, flips streamer mode with it.
    fn set_obs_running(&mut self, running: bool, cx: &mut Context<Self>) {
        if running == self.obs_running {
            return;
        }
        self.obs_running = running;
        cx.emit(settings_ui::SettingsChanged);
        self.apply_streamer_mode(cx);
    }

    /// Changes the streamer-mode setting (off / on / auto), persists it, and
    /// applies it.
    fn set_streamer_mode(&mut self, choice: settings::StreamerModeChoice, cx: &mut Context<Self>) {
        if choice == self.settings.streamer_mode {
            return;
        }
        self.settings.streamer_mode = choice;
        self.save_settings(cx);
        self.apply_streamer_mode(cx);
    }

    /// Recomputes whether streamer mode is active from the setting + OBS state,
    /// updates the process-wide flag, and re-renders everything that reads it
    /// (open usercards render against their tab's ChatView, so each tab is
    /// notified too).
    fn apply_streamer_mode(&mut self, cx: &mut Context<Self>) {
        let on = match self.settings.streamer_mode {
            settings::StreamerModeChoice::On => true,
            settings::StreamerModeChoice::Off => false,
            settings::StreamerModeChoice::Auto => self.obs_running,
        };
        if on != streamer_mode::is_active() {
            streamer_mode::set_active(on);
            tracing::info!("streamer mode {}", if on { "enabled" } else { "disabled" });
            // Each activation is news — undo a previous ✕ so the banner shows.
            if on {
                self.streamer_banner_dismissed = false;
            }
            for tab in &self.tabs {
                tab.view.update(cx, |view, cx| {
                    // The inline preview thumbnail lives in the cached log, so it
                    // needs an explicit log refresh (a bare notify wouldn't reach
                    // it); owned panels also need their appearance refreshed.
                    view.refresh_log(cx);
                    view.refresh_panel_styles(cx);
                    cx.notify();
                });
            }
        }
        cx.notify();
    }

    /// The "streamer mode is on" banner under the tab strip: a quick "Turn off"
    /// (sets the setting to Off) and an ✕ that dismisses just the notice —
    /// streamer mode stays on, and the banner returns on its next activation.
    fn streamer_banner(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let label = if self.settings.streamer_mode == settings::StreamerModeChoice::Auto {
            "Streamer mode is on — streaming software detected"
        } else {
            "Streamer mode is on — enabled manually"
        };
        h_flex()
            .w_full()
            .px_3()
            .py_1()
            .gap_2()
            .items_center()
            .bg(cx.theme().warning.opacity(0.12))
            .border_l_2()
            .border_color(cx.theme().warning)
            .text_size(px(13.))
            .child(SharedString::from("🕶"))
            .child(div().flex_1().min_w_0().child(SharedString::from(label)))
            .child(
                Button::new("streamer-banner-off")
                    .label("Turn off")
                    .outline()
                    .xsmall()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_streamer_mode(settings::StreamerModeChoice::Off, cx);
                    })),
            )
            .child(
                div()
                    .id("streamer-banner-dismiss")
                    .px_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(cx.theme().muted_foreground)
                    .hover(|s| s.bg(cx.theme().secondary))
                    .child(SharedString::from("✕"))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.streamer_banner_dismissed = true;
                            cx.notify();
                        }),
                    ),
            )
            .into_any_element()
    }

    /// Looks for a newer release now and on a slow cadence after; the blocking
    /// Velopack call (network + disk) runs off the main thread. Finding one
    /// downloads it, shows the banner, and ends the loop.
    fn spawn_update_watch(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |weak, cx| loop {
            let version = cx
                .background_executor()
                .spawn(async { updater::check_and_download() })
                .await;
            if let Some(version) = version {
                weak.update(cx, |this, cx| {
                    this.update_ready = Some(version);
                    cx.notify();
                })
                .ok();
                break;
            }
            cx.background_executor()
                .timer(updater::CHECK_INTERVAL)
                .await;
        })
    }

    /// Toggles the beta update channel: persists the setting, points the
    /// updater at (or away from) pre-releases, and restarts the update watch so
    /// the change takes effect now instead of at the next scheduled check.
    fn set_beta_updates(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.beta_updates = on;
        self.save_settings(cx);
        updater::set_beta_updates(on);
        // An already-downloaded update stays offered; otherwise re-check under
        // the new channel immediately.
        if self.update_ready.is_none() {
            self._update_watch = Self::spawn_update_watch(cx);
        }
        cx.notify();
    }

    /// The "update ready" banner under the tab strip: a newer release has been
    /// downloaded in the background; Restart applies it now, ✕ hides the notice
    /// (Velopack still applies the pending update on the next launch).
    fn update_banner(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let label = format!(
            "Update {} is ready — restart to apply",
            self.update_ready.as_deref().unwrap_or_default()
        );
        h_flex()
            .w_full()
            .px_3()
            .py_1()
            .gap_2()
            .items_center()
            .bg(cx.theme().info.opacity(0.12))
            .border_l_2()
            .border_color(cx.theme().info)
            .text_size(px(13.))
            .child(SharedString::from("⭳"))
            .child(div().flex_1().min_w_0().child(SharedString::from(label)))
            .child({
                let url = updater::release_url(self.update_ready.as_deref().unwrap_or_default());
                div()
                    .id("update-whats-new")
                    .cursor_pointer()
                    .text_color(gpui::rgb(render::link_color()))
                    .hover(|s| s.underline())
                    .child(SharedString::from("What's new"))
                    .on_click(move |_, _, cx| cx.open_url(&url))
            })
            .child(
                Button::new("update-banner-restart")
                    .label("Restart")
                    .outline()
                    .xsmall()
                    .on_click(|_, _, _| updater::restart_to_update()),
            )
            .child(
                div()
                    .id("update-banner-dismiss")
                    .px_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(cx.theme().muted_foreground)
                    .hover(|s| s.bg(cx.theme().secondary))
                    .child(SharedString::from("✕"))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.update_banner_dismissed = true;
                            cx.notify();
                        }),
                    ),
            )
            .into_any_element()
    }

    /// The one-time "updated" banner: the first launch after an update applied
    /// (Velopack's restarted hook) announces the new version with a link to its
    /// release notes. ✕ dismisses; a normal launch never shows it.
    fn updated_banner(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let version = self.updated_to.clone().unwrap_or_default();
        let url = updater::release_url(&version);
        h_flex()
            .w_full()
            .px_3()
            .py_1()
            .gap_2()
            .items_center()
            .bg(cx.theme().success.opacity(0.12))
            .border_l_2()
            .border_color(cx.theme().success)
            .text_size(px(13.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(SharedString::from(format!("Updated to v{version}"))),
            )
            .child(
                div()
                    .id("updated-whats-new")
                    .cursor_pointer()
                    .text_color(gpui::rgb(render::link_color()))
                    .hover(|s| s.underline())
                    .child(SharedString::from("What's new"))
                    .on_click(move |_, _, cx| cx.open_url(&url)),
            )
            .child(
                div()
                    .id("updated-banner-dismiss")
                    .px_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(cx.theme().muted_foreground)
                    .hover(|s| s.bg(cx.theme().secondary))
                    .child(SharedString::from("✕"))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.updated_to = None;
                            cx.notify();
                        }),
                    ),
            )
            .into_any_element()
    }

    /// Shared base for every tab chip (normal tabs + the Mentions pseudo-tab), so
    /// they render identically. A compact rounded chip: the active one gets a faint
    /// accent-tinted fill, full-foreground text, and a 2px accent underline;
    /// inactive ones sit on a muted recessed fill and lift on hover. Both carry the
    /// bottom border (transparent when inactive) so selecting doesn't shift the text.
    fn tab_chip_base(
        id: impl Into<ElementId>,
        selected: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let accent = gpui::rgb(render::accent());
        h_flex()
            .id(id)
            .h_6()
            .px_2p5()
            .my_0p5()
            .mr_0p5()
            .gap_1p5()
            .items_center()
            .rounded_t_md()
            .border_b_2()
            .cursor_pointer()
            .text_size(px(13.))
            .map(|chip| {
                if selected {
                    chip.bg(accent.opacity(0.14))
                        .border_color(accent)
                        .text_color(cx.theme().foreground)
                        .font_weight(FontWeight::SEMIBOLD)
                } else {
                    chip.bg(gpui::rgb(render::tab_inactive_bg()))
                        .border_color(gpui::transparent_black())
                        .text_color(cx.theme().muted_foreground)
                        .hover(|s| {
                            s.bg(render::chrome_hover())
                                .text_color(cx.theme().foreground)
                        })
                }
            })
    }

    /// The pinned "@ Mentions" chip at the front of the tab strip (only when
    /// enabled in Highlights settings): selecting it shows the shared all-tabs
    /// mention feed instead of a tab. Rendered like a normal tab (`tab_chip_base`);
    /// closing it (right-click → Close tab) unchecks the setting; no drag, no
    /// per-tab settings.
    fn mentions_tab_chip(&self, selected: bool, cx: &mut Context<Self>) -> impl IntoElement {
        // A custom name shows verbatim; only the default keeps the "@" mark.
        let label = SharedString::from(
            self.settings
                .mentions_tab_name
                .clone()
                .unwrap_or_else(|| "@ Mentions".to_string()),
        );
        // Unread bolds + un-dims the name, like a normal tab's chip; only
        // meaningful when the Mentions feed isn't the current view.
        let unread = self.mentions_unread && !selected;
        Self::tab_chip_base("mentions-tab", selected, cx)
            .flex_none()
            .child(if unread {
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().foreground)
                    .child(label)
            } else {
                div().child(label)
            })
            .on_click(cx.listener(|this, _, _, cx| {
                this.mentions_tab_selected = true;
                this.mentions_unread = false;
                cx.notify();
            }))
            // Right-click: Open in new window / Close tab (closing unchecks the
            // Highlights setting, like a normal tab's Close).
            .context_menu({
                let app = cx.entity().downgrade();
                move |menu, _window, _cx| {
                    let for_popout = app.clone();
                    let for_close = app.clone();
                    menu.min_w(px(200.))
                        .item(
                            PopupMenuItem::new("Open in new window")
                                .icon(IconName::WindowMaximize)
                                .on_click(move |_, _, cx| {
                                    for_popout
                                        .update(cx, |this, cx| this.pop_out_mentions(cx))
                                        .ok();
                                }),
                        )
                        .separator()
                        .item(
                            PopupMenuItem::new("Close tab")
                                .icon(IconName::Close)
                                .on_click(move |_, _, cx| {
                                    for_close
                                        .update(cx, |this, cx| this.close_mentions_tab(cx))
                                        .ok();
                                }),
                        )
                }
            })
    }

    /// The global Mentions tab's body: the shared all-tabs feed at full size,
    /// on the chat surface, tailing like the side panels. `pub(crate)` so the
    /// popped-out Mentions window ([`popout::MentionsWindow`]) can render it too.
    pub(crate) fn mentions_tab_body(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let font_size = self.settings.font_size;
        let body = self
            .mentions_feed
            .render(&self.mention_store, font_size, cx);
        v_flex()
            .size_full()
            .min_h_0()
            .bg(gpui::rgb(render::chat_bg()))
            .child(body)
            .into_any_element()
    }

    /// The custom title bar (kit `TitleBar`): a draggable caption whose right
    /// side carries the per-platform login indicators + the settings gear, with
    /// the OS min/max/close controls the kit draws after them. Replaces the
    /// native Windows caption (the window is opened with a transparent titlebar —
    /// see `window_state::main_window_options`).
    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.session.login_state();
        // Clicking any login icon (or the app name) opens Account settings; the
        // gear toggles the settings window like before.
        TitleBar::new()
            // Left: the app name, so the empty caption still reads as "Backseater"
            // and gives a comfortable drag target.
            .child(
                div()
                    .flex()
                    .items_center()
                    .min_w_0()
                    .flex_shrink(1.0)
                    .overflow_hidden()
                    .truncate()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from("Backseater")),
            )
            // Right: login indicators + gear. Shrinks (login names truncate) so a
            // narrow window keeps the window controls reachable; the gear stays
            // fixed. `stop_propagation` on mouse-down so clicking a control doesn't
            // also start a window drag.
            .child(
                h_flex()
                    .items_center()
                    .justify_end()
                    .min_w_0()
                    .flex_shrink(1.0)
                    .pr_1()
                    .gap_0p5()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(self.login_indicator(
                        bks_core::Platform::Twitch,
                        state.twitch.clone(),
                        cx,
                    ))
                    .child(self.login_indicator(bks_core::Platform::Kick, state.kick.clone(), cx))
                    .child(self.titlebar_gear(cx)),
            )
    }

    /// One platform's login indicator in the title bar: the platform logo
    /// (full-opacity when logged in, dimmed when not) followed by the account
    /// name when logged in — truncating on a narrow window so it never pushes the
    /// window controls off — or a muted "○" hint when logged out. Hover shows the
    /// account name / a "Log in to <platform>" hint; click opens Account settings.
    fn login_indicator(
        &self,
        platform: bks_core::Platform,
        account: Option<String>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let logged_in = account.is_some();
        let icon_url = platform.icon_url().map(SharedString::from);
        let name = account.clone().map(SharedString::from);
        let tip = SharedString::from(match &account {
            Some(name) => format!("{}: {}", platform.label(), name),
            None => format!("Log in to {}", platform.label()),
        });

        h_flex()
            .id(("login", platform as usize))
            .h_6()
            .px_1()
            .gap_1()
            .items_center()
            .rounded_md()
            .cursor_pointer()
            // The whole chip may shrink (name-first) so it never pushes the window
            // controls off a narrow window.
            .min_w_0()
            .flex_shrink(1.0)
            .hover(|s| s.bg(render::chrome_hover()))
            .when_some(icon_url, |this, url| {
                this.child(
                    img(url)
                        .id("login-icon")
                        .flex_shrink_0()
                        .h(px(15.))
                        .w(px(15.))
                        // Dim the logo when logged out so "logged in" reads at a glance.
                        .when(!logged_in, |img| img.opacity(0.4)),
                )
            })
            .map(|this| match name {
                // Logged in: show the account name (truncating on a tight window,
                // where it clips before the controls do). The visible name already
                // signals "logged in", so no status dot is needed.
                Some(name) => this.child(
                    div()
                        .min_w_0()
                        .max_w(px(120.))
                        .overflow_hidden()
                        .truncate()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child(name),
                ),
                // Logged out: a small muted dot as the "not logged in" hint.
                None => this.child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(8.))
                        .text_color(cx.theme().muted_foreground)
                        .child(SharedString::from("○")),
                ),
            })
            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            .on_click(cx.listener(move |this, _, _, cx| this.open_account_settings(cx)))
    }

    /// The settings gear in the title bar (same action as the old strip gear).
    fn titlebar_gear(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .id("titlebar-settings")
            .h_6()
            .w_6()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded_md()
            .cursor_pointer()
            .text_color(cx.theme().muted_foreground)
            .hover(|s| {
                s.bg(render::chrome_hover())
                    .text_color(cx.theme().foreground)
            })
            .child(SharedString::from("⚙"))
            .tooltip(|window, cx| Tooltip::new("Settings").build(window, cx))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.open_app_settings(cx)),
            )
    }

    fn tab_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active;
        // While the global Mentions pseudo-tab is selected, no normal chip is.
        let mentions_selected = self.settings.mentions_tab && self.mentions_tab_selected;
        // Collected eagerly so the `cx` borrow held by this map's listeners ends
        // here, freeing `cx` for the arrow buttons built further down.
        let tabs: Vec<_> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(ix, tab)| {
                let selected = ix == active && !mentions_selected;
                let label = SharedString::from(tab.config.display_name());
                let being_dragged = self.dragging == Some(ix);
                // Unread (new activity while this tab wasn't active) bolds +
                // un-dims the name, like an unread email. Only meaningful on an
                // inactive chip — selecting a tab clears its unread flag.
                let unread = tab.unread && !selected;
                // Snapshot each set platform's latest live status for the hover
                // tooltip. Read eagerly so the tooltip closure owns its data (uptime is
                // still recomputed at show time from the captured start). `None` until
                // a poll lands → the tooltip reads "offline".
                let view = tab.view.read(cx);
                let tip_platforms = vec![
                    TipPlatform {
                        platform: bks_core::Platform::Twitch,
                        channel: tab.config.twitch_channel.trim().to_string(),
                        status: view.live_status(bks_core::Platform::Twitch, cx),
                        viewers: view.viewer_count(bks_core::Platform::Twitch, cx),
                    },
                    TipPlatform {
                        platform: bks_core::Platform::Kick,
                        channel: tab.config.kick_channel.trim().to_string(),
                        status: view.live_status(bks_core::Platform::Kick, cx),
                        viewers: view.viewer_count(bks_core::Platform::Kick, cx),
                    },
                    TipPlatform {
                        platform: bks_core::Platform::YouTube,
                        channel: tab.config.youtube_channel.trim().to_string(),
                        status: view.live_status(bks_core::Platform::YouTube, cx),
                        viewers: view.viewer_count(bks_core::Platform::YouTube, cx),
                    },
                    TipPlatform {
                        platform: bks_core::Platform::TikTok,
                        channel: tab.config.tiktok_channel.trim().to_string(),
                        status: view.live_status(bks_core::Platform::TikTok, cx),
                        viewers: view.viewer_count(bks_core::Platform::TikTok, cx),
                    },
                ];
                let has_channel = tab.config.has_channel();
                // Any of the tab's platforms currently live → a green dot on the chip.
                let any_live = tip_platforms
                    .iter()
                    .any(|p| !p.channel.is_empty() && p.status.as_ref().is_some_and(|s| s.live));
                // A just-went-live pulse tint (opt-in), in the platform's brand
                // color. The chip is `relative()` already for the tooltip, so
                // the flash is an absolute overlay — no layout disturbance,
                // painted over the base fill.
                let flash = tab.flash_start.and_then(|(start, platform)| {
                    let alpha = chip_flash_alpha(start.elapsed());
                    (alpha > 0.0).then(|| (alpha, platform.color().to_u32()))
                });
                // Compact pill chip (see `tab_chip_base`): a rounded, self-contained
                // pill so each tab reads as its own, active one filled with the accent.
                Self::tab_chip_base(("tab", ix), selected, cx)
                    // Anchor for the tooltip overlay (absolute, just below the chip).
                    .relative()
                    .when_some(flash, |this, (alpha, color)| {
                        this.child(
                            div()
                                .absolute()
                                .inset_0()
                                .rounded_t_md()
                                .bg(gpui::rgb(color).opacity(alpha * 0.5)),
                        )
                    })
                    // A live-status tooltip per set platform, only when the tab has a
                    // channel (an empty tab shows its "right-click → Settings" prompt).
                    // Hand-rolled (see the `chip_tip` field): hover here drives the
                    // show/hide timers, the overlay child below is the panel itself.
                    .when(has_channel, |this| {
                        this.on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                            this.chip_hover_changed(ix, *hovered, cx);
                        }))
                    })
                    .when(has_channel && self.chip_tip == Some(ix), |this| {
                        this.child(chip_tooltip(tip_platforms.clone(), cx))
                    })
                    // Any click on the chip dismisses the tooltip (a hover tooltip
                    // left up would obscure the context menu the right click opens).
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, _, _, cx| this.dismiss_chip_tip(cx)),
                    )
                    // Fade the source chip while its copy is being carried.
                    .when(being_dragged, |this| this.opacity(0.4))
                    // The live dot, theme-aware green (matches the tooltip's ● LIVE).
                    .when(any_live, |this| {
                        this.child(
                            div()
                                .text_size(px(10.))
                                .text_color(gpui::rgb(render::live_text()))
                                .child(SharedString::from("●")),
                        )
                    })
                    // Unread bolds + un-dims the name (overriding the inactive
                    // chip's muted weight/color from `tab_chip_base`).
                    .child(if unread {
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(cx.theme().foreground)
                            .child(label.clone())
                    } else {
                        div().child(label.clone())
                    })
                    // Drag left/right to reorder. We swap live the moment the cursor
                    // crosses this tab's horizontal midpoint, tracking the dragged
                    // tab's new index in `self.dragging`.
                    .on_drag(
                        DraggedTab {
                            from: ix,
                            label,
                            selected,
                        },
                        move |tab, _offset: Point<Pixels>, _window, cx| cx.new(|_| tab.clone()),
                    )
                    .on_drag_move(cx.listener(
                        move |this, ev: &gpui::DragMoveEvent<DraggedTab>, _window, cx| {
                            // Seed the live-drag index from the payload on first move.
                            let from = this.dragging.unwrap_or_else(|| ev.drag(cx).from);
                            this.dragging = Some(from);
                            this.dismiss_chip_tip(cx);
                            if from == ix {
                                return;
                            }
                            // Chips wrap onto multiple rows, so a swap is gated on the
                            // pointer being within this chip's own row band (so chips
                            // on other rows can't grab it), then past its horizontal
                            // midpoint in the travel direction.
                            let pos = ev.event.position;
                            let top = ev.bounds.origin.y;
                            let bottom = ev.bounds.bottom();
                            let mid = ev.bounds.origin.x + ev.bounds.size.width / 2.0;
                            let in_row = pos.y >= top && pos.y <= bottom;
                            let past = in_row
                                && if from < ix {
                                    pos.x >= mid
                                } else {
                                    pos.x <= mid
                                };
                            if past {
                                this.move_tab(from, ix, cx);
                                this.dragging = Some(ix);
                            }
                        },
                    ))
                    // A drag released onto any tab ends the live reorder.
                    .on_drop(cx.listener(|this, _: &DraggedTab, _window, cx| {
                        this.dragging = None;
                        cx.notify();
                    }))
                    // Left click selects; right click opens the context menu below.
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.dragging = None;
                        this.dismiss_chip_tip(cx);
                        this.select_tab(ix, cx);
                    }))
                    // Right-click: Settings / Open in new window / Close tab. The
                    // item click closures run in `App` context, so they hop back to
                    // this app entity to call its methods (which need `Context<Self>`).
                    .context_menu({
                        let app = cx.entity().downgrade();
                        move |menu, _window, _cx| {
                            let for_settings = app.clone();
                            let for_search = app.clone();
                            let for_popout = app.clone();
                            let for_close = app.clone();
                            // A comfortable width so the items don't feel cramped.
                            menu.min_w(px(200.))
                                .item(
                                    PopupMenuItem::new("Settings")
                                        .icon(IconName::Settings)
                                        .on_click(move |_, _, cx| {
                                            for_settings
                                                .update(cx, |this, cx| this.open_settings(ix, cx))
                                                .ok();
                                        }),
                                )
                                .separator()
                                .item(
                                    // Nothing to search until the tab has a channel.
                                    PopupMenuItem::new("Search (Ctrl+F)")
                                        .icon(IconName::Search)
                                        .disabled(!has_channel)
                                        .on_click(move |_, _, cx| {
                                            for_search
                                                .update(cx, |this, cx| {
                                                    let view =
                                                        this.tabs.get(ix).map(|t| t.view.clone());
                                                    if let Some(view) = view {
                                                        view.update(cx, |v, cx| v.open_search(cx));
                                                    }
                                                })
                                                .ok();
                                        }),
                                )
                                .separator()
                                .item(
                                    // Nothing to pop out until the tab has a channel.
                                    PopupMenuItem::new("Open in new window")
                                        .icon(IconName::WindowMaximize)
                                        .disabled(!has_channel)
                                        .on_click(move |_, _, cx| {
                                            for_popout
                                                .update(cx, |this, cx| this.pop_out_tab(ix, cx))
                                                .ok();
                                        }),
                                )
                                .separator()
                                .item(
                                    PopupMenuItem::new("Close tab")
                                        .icon(IconName::Close)
                                        .on_click(move |_, window, cx| {
                                            for_close
                                                .update(cx, |this, cx| {
                                                    this.confirm_close_tab(ix, window, cx)
                                                })
                                                .ok();
                                        }),
                                )
                        }
                    })
            })
            .collect();

        let bg = gpui::rgb(render::tab_bar_bg());

        // No hard border under the strip: the bar sits one elevation step above
        // the chat surface, and that contrast is the separation. Compact pill chips
        // are vertically centered on the bar. The settings gear + login status now
        // live in the title bar above; when there are more tabs than fit one line
        // they wrap onto additional rows (Chatterino-style) rather than scrolling
        // horizontally, and each wrapped row starts at the strip's true left edge.
        h_flex()
            .w_full()
            .px_1()
            .bg(bg)
            .items_start()
            // The Mentions pseudo-tab + tabs + `add` button all live in the wrapping
            // strip so every wrapped row (including the first) aligns to the same
            // left edge. `min_w_0` lets the strip take the remaining width (so
            // wrapping is measured against it, not the chips' natural width);
            // `flex_none` chips keep their size.
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .flex_wrap()
                    .items_start()
                    // The global Mentions pseudo-tab, first so it's always reachable.
                    // Enabled in Highlights settings.
                    .when(self.settings.mentions_tab, |this| {
                        this.child(self.mentions_tab_chip(mentions_selected, cx))
                    })
                    .children(tabs.into_iter().map(|t| t.flex_none()))
                    // The `+` matches a tab chip's box (h_6 + my_0p5) so it sits on
                    // the same baseline as the tabs it follows.
                    .child(
                        h_flex()
                            .id("add-tab")
                            .flex_none()
                            .h_6()
                            .my_0p5()
                            .px_2p5()
                            .items_center()
                            .rounded_md()
                            .cursor_pointer()
                            .text_color(cx.theme().muted_foreground)
                            .hover(|s| {
                                s.bg(render::chrome_hover())
                                    .text_color(cx.theme().foreground)
                            })
                            .child(SharedString::from("+"))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, window, cx| this.add_tab(window, cx)),
                            ),
                    ),
            )
    }
}

/// Maps a saved [`CustomTheme`](settings::CustomTheme) profile to the render
/// crate's [`CustomColors`](render::CustomColors) so a full palette can be built.
fn custom_colors(t: &settings::CustomTheme) -> render::CustomColors {
    render::CustomColors {
        base_dark: t.base_dark,
        chat_bg: t.chat_bg,
        default_name: t.default_name,
        first_message: t.first_message,
        // A theme saved before the highlighted-row color existed deserializes to
        // None; seed it from the base default (a real pick of pure black is
        // Some(0), so it's preserved rather than treated as unset).
        highlighted: t
            .highlighted
            .unwrap_or_else(|| render::CustomColors::from_base(t.base_dark).highlighted),
        event: t.event,
        streak: t.streak,
        live: t.live,
        offline: t.offline,
        mention: t.mention,
        link: t.link,
        error: t.error,
    }
}

/// Whether the app's chrome should be in dark mode for these settings (a custom
/// theme reports its own base; a built-in reports itself).
fn theme_is_dark(settings: &settings::Settings) -> bool {
    settings
        .active_custom_theme()
        .map_or(settings.theme.is_dark(), |t| t.base_dark)
}

/// Applies the settings' [`ThemeChoice`] to the whole app: switches the
/// gpui-component kit dark/light mode (needs a window) then applies the coordinated
/// surface colors + chat-log palette. Use [`apply_theme_surfaces`] when only the
/// colors change (a live custom-color edit), which needs no window.
fn apply_theme(settings: &settings::Settings, window: &mut Window, cx: &mut App) {
    let mode = if theme_is_dark(settings) {
        gpui_component::ThemeMode::Dark
    } else {
        gpui_component::ThemeMode::Light
    };
    gpui_component::Theme::change(mode, Some(window), cx);
    apply_theme_surfaces(settings, cx);
}

/// The window-free half of [`apply_theme`]: mirrors the theme into the flag
/// `render` reads, installs/clears any custom palette, and overrides the kit's
/// surface colors to match. Called directly on a live custom-color edit (no
/// dark/light-mode flip, so no window needed).
fn apply_theme_surfaces(settings: &settings::Settings, cx: &mut App) {
    let custom = settings.active_custom_theme();
    // Flip our own flag first so `render::*` accessors return the new palette,
    // then install (or clear) any custom palette on top of it.
    bks_core::set_dark_theme(theme_is_dark(settings));
    render::set_custom_palette(custom.map(|t| render::Palette::from_custom(custom_colors(t))));

    // The kit's dark theme uses a near-black (`#0a0a0a`) for `background`/`popover`,
    // which made tooltips, dialogs, dropdowns and other kit surfaces read as flat
    // black holes next to the chat. Override the kit's surface colors with our
    // coordinated palette so *every* kit popover/panel matches the app.
    let theme = gpui_component::Theme::global_mut(cx);
    let hsla = |packed: u32| -> gpui::Hsla { gpui::rgb(packed).into() };
    let panel = hsla(render::panel_bg());
    let bar = hsla(render::tab_bar_bg());
    let recessed = hsla(render::tab_inactive_bg());
    // Popovers (tooltips, dropdowns, menus, alert dialogs) → elevated panel tone.
    theme.popover = panel;
    // The window backdrop sits behind the chat surface — a touch darker than chat.
    theme.background = bar;
    // Secondary surfaces (input bar, chips, segmented controls) → recessed tone.
    theme.secondary = recessed;
    // The (kit) title/tab bars, in case any kit widget uses them.
    theme.title_bar = bar;
    theme.tab_bar = bar;
    // Widgets that read the *resolved tokens* (e.g. the Tooltip uses
    // `theme.tokens.popover`, not `theme.popover`) bypass the color fields above, so
    // mirror the same overrides into `tokens` or they keep the kit's near-black.
    theme.tokens.popover = panel.into();
    theme.tokens.background = bar.into();
    theme.tokens.secondary = recessed.into();
    theme.tokens.title_bar = bar.into();
    theme.tokens.tab_bar = bar.into();

    // Re-assert the scrollbar preference so a theme switch doesn't revert it:
    // visible only while scrolling (fades out when idle) — the log keeps a
    // right gutter for the thumb, so nothing shifts when it appears.
    theme.scrollbar_show = gpui_component::scroll::ScrollbarShow::Scrolling;
}

/// Applies the chosen font family to the kit theme; the kit `Root` element sets
/// `theme.font_family` on every window's root div, so all text (chat included)
/// inherits it. `None` restores the kit's system default. Also publishes the
/// font's vertical metrics (per-em ascent/descent/cap-height ratios) so chat
/// rows align images and timestamps to this font's real baseline
/// (`render::set_font_metrics`).
fn apply_font(family: Option<&str>, cx: &mut App) {
    let family = SharedString::from(family.unwrap_or(SYSTEM_FONT_FAMILY).to_string());
    gpui_component::Theme::global_mut(cx).font_family = family.clone();
    let text_system = cx.text_system();
    let font_id = text_system.resolve_font(&gpui::font(family));
    // Query at a big size and divide: metrics scale linearly with the size.
    let em = px(1000.);
    render::set_font_metrics(
        f32::from(text_system.ascent(font_id, em)) / 1000.0,
        f32::from(text_system.descent(font_id, em)) / 1000.0,
        f32::from(text_system.cap_height(font_id, em)) / 1000.0,
    );
}

/// Whether editing a tab's channel from `old` to `new` keeps its log: unchanged,
/// newly added (`old` empty), or removed (`new` empty) all keep it — only
/// *replacing* one channel with a different one means a different log.
fn channel_kept(old: &str, new: &str) -> bool {
    old == new || old.is_empty() || new.is_empty()
}

impl Render for BackseaterApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Un-stick hover state if the pointer left the window (see stale_hover.rs).
        stale_hover::clear(window, cx);

        // Layout edits (divider drags, header-arrow moves) happen inside a tab's
        // view, which updates its own live config; sync any change back into the
        // persisted config here (the only place with both the view and the tab
        // list) and save it.
        self.sync_layouts(cx);

        // Title follows the active tab; memoized here so every path that changes
        // it (select, rename, close, restore) is covered without call-site hooks.
        let mentions_tab = self.settings.mentions_tab && self.mentions_tab_selected;
        let title = if mentions_tab {
            self.mentions_window_title()
        } else {
            match self.tabs.get(self.active) {
                Some(tab) => format!("Backseater - {}", tab.config.display_name()),
                None => "Backseater".to_string(),
            }
        };
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }

        let content: gpui::AnyElement = if mentions_tab {
            self.mentions_tab_body(cx)
        } else if let Some(tab) = self.tabs.get(self.active) {
            tab.view.clone().into_any_element()
        } else {
            div().into_any_element()
        };
        // Root draws the view + tooltip/menu overlays but not dialogs; the view
        // must render the dialog layer itself for `open_dialog` to appear.
        let dialog_layer = Root::render_dialog_layer(window, cx);

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .child(self.render_title_bar(cx))
            .child(self.tab_strip(cx))
            .when(
                streamer_mode::is_active() && !self.streamer_banner_dismissed,
                |el| el.child(self.streamer_banner(cx)),
            )
            .when(
                self.update_ready.is_some() && !self.update_banner_dismissed,
                |el| el.child(self.update_banner(cx)),
            )
            .when(self.updated_to.is_some(), |el| {
                el.child(self.updated_banner(cx))
            })
            // `min_h_0` lets this flex item shrink below its (tall) content so
            // the feed is bounded to the window and scrolls instead of overflowing.
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_hidden()
                    .child(content),
            )
            .children(dialog_layer)
    }
}

fn main() {
    // Velopack first: its install/update hooks may restart or exit the process
    // before the app proper starts.
    updater::startup();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let app = gpui_platform::application().with_assets(assets::Assets);

    app.run(|cx: &mut App| {
        gpui_component::init(cx);
        // gpui_component::init defaults to light; switch to a dark theme.
        gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
        // Scrollbars show only while scrolling and fade out when idle, keeping
        // the chat chrome clean while the log tail-follows.
        gpui_component::Theme::global_mut(cx).scrollbar_show =
            gpui_component::scroll::ScrollbarShow::Scrolling;

        // Required so `img(<https url>)` can fetch remote emote images.
        let http = reqwest_client::ReqwestClient::user_agent("backseater/0.1")
            .expect("failed to build http client");
        cx.set_http_client(Arc::new(http));

        cx.activate(true);

        cx.spawn(async move |cx| {
            // Reopen where the user left the window (position/size/maximized),
            // falling back to defaults when nothing is saved or the saved
            // display is gone.
            let options = cx.update(window_state::main_window_options);
            let handle = cx
                .open_window(options, |window, cx| {
                    // Pick emote image sizes for this display's DPI:
                    // 1x at 100% scaling, 2x above. Fetching a bigger variant than the
                    // screen needs is wasted bytes + decode + heap RAM.
                    let scale = if window.scale_factor() > 1.25 { 2 } else { 1 };
                    bks_core::set_preferred_scale(scale);
                    let app = cx.new(|cx| BackseaterApp::new(window, cx));
                    cx.new(|cx| Root::new(app, window, cx).bg(cx.theme().background))
                })
                .expect("failed to open window");
            // Closing the main window quits the app even while child windows
            // (settings, usercards) are open — without this the default quit
            // rule ("last window closed") would leave them running orphaned.
            let main_id = gpui::AnyWindowHandle::from(handle).window_id();
            cx.update(|cx| {
                cx.on_window_closed(move |cx, id| {
                    // A close can outrun the debounced bounds save; write the
                    // final position now (also catches quit, right below).
                    window_state::flush();
                    if id == main_id {
                        cx.quit();
                    }
                })
                .detach();
            });
        })
        .detach();
    });
}

/// Formats an elapsed stream uptime compactly: under an hour shows minutes
/// ("23m"), an hour or more shows hours + minutes ("1h23m", "2h00m"), and a full
/// day or more shows days + hours ("1d", "1d22h", "3d4h" — a "last live" from
/// days/weeks ago shouldn't read "46h00m" or "730h00m"); a negative span (clock
/// skew) clamps to "0m". Used by the tab strip's live tooltip.
fn format_uptime(elapsed: chrono::Duration) -> String {
    let total_mins = elapsed.num_minutes().max(0);
    let (h, m) = (total_mins / 60, total_mins % 60);
    if h == 0 {
        format!("{m}m")
    } else if h < 24 {
        format!("{h}h{m:02}m")
    } else {
        let (d, h) = (h / 24, h % 24);
        if h == 0 {
            format!("{d}d")
        } else {
            format!("{d}d{h}h")
        }
    }
}

/// Which surface a "show timestamps" toggle applies to (chat log / events panel /
/// mentions panel), used by [`BackseaterApp::set_show_timestamps`].
#[derive(Clone, Copy)]
enum TimestampSurface {
    Chat,
    Events,
    Mentions,
}

/// One platform's snapshot for a tab tooltip: the channel it's set to plus its
/// latest known live status (`None` until the first poll). Built eagerly per
/// render so the tooltip closure owns its data.
#[derive(Clone)]
struct TipPlatform {
    platform: bks_core::Platform,
    channel: String,
    status: Option<LiveInfo>,
    /// Latest concurrent viewer count, shown while live.
    viewers: Option<u64>,
}

/// A small platform logo for chrome (tooltip headers, the status bar, account
/// rows) — the real logo when the platform ships one ([`Platform::icon_url`]),
/// else its brand-colored glyph, at a fixed `size` (chrome, not a font-scaled
/// chat row).
pub(crate) fn platform_icon(platform: bks_core::Platform, size: f32) -> gpui::AnyElement {
    match platform.icon_url() {
        Some(url) => {
            let (w, h) = platform.icon_size(size);
            img(SharedString::from(url))
                .h(px(h))
                .w(px(w))
                .flex_none()
                .into_any_element()
        }
        None => div()
            .flex_none()
            .font_weight(FontWeight::BOLD)
            .text_color(gpui::rgb(platform.color().to_u32()))
            .child(SharedString::from(platform.glyph()))
            .into_any_element(),
    }
}

/// Builds a tab chip's tooltip body — one compact stream card per set platform.
/// Header: [platform icon] + channel name (a click target opening the stream /
/// channel page, truncated when long) with a LIVE pill — or a muted
/// "last seen 3h ago" — pinned to the right edge. A live stream adds its title
/// (clamped to two lines) and a muted stats line (uptime · viewers · category,
/// ellipsized — the category used to overflow the panel) underneath; offline
/// stays a single header line. Times are computed here (at show time) so they
/// stay current. A platform with no channel set is omitted; with no channels at
/// all the tooltip is a single "no channel set" line. Multiple platforms get a
/// hairline divider between cards.
fn live_tooltip_content(platforms: &[TipPlatform]) -> gpui::Div {
    let now = chrono::Utc::now();
    let mut col = v_flex().gap_2();
    let set: Vec<&TipPlatform> = platforms.iter().filter(|p| !p.channel.is_empty()).collect();
    if set.is_empty() {
        return col.child(SharedString::from("no channel set"));
    }
    for (idx, p) in set.into_iter().enumerate() {
        if idx > 0 {
            col = col.child(card_divider());
        }
        let live = matches!(&p.status, Some(info) if info.live);
        // While live, prefer the stream's own watch link (YouTube's
        // `watch?v=` — a specific video) over the channel page.
        let url = p
            .status
            .as_ref()
            .filter(|s| s.live)
            .and_then(|s| s.link.clone())
            .unwrap_or_else(|| p.platform.channel_url(&p.channel));
        let mut header = h_flex()
            .gap_2()
            .items_center()
            .child(platform_icon(p.platform, 16.))
            .child(
                div()
                    .id(SharedString::from(format!(
                        "tip-open-{}",
                        p.platform.label()
                    )))
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::BOLD)
                    .cursor_pointer()
                    .hover(|s| s.text_color(gpui::rgb(render::link_color())))
                    .child(SharedString::from(p.channel.clone()))
                    .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                        cx.open_url(&url);
                    }),
            )
            .child(div().flex_1());
        if live {
            let (pill_bg, _) = render::highlight_live(true);
            header = header.child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .items_center()
                    .px_1p5()
                    .py_0p5()
                    .rounded_full()
                    .bg(gpui::rgb(pill_bg))
                    .child(
                        div()
                            .size(px(6.))
                            .rounded_full()
                            .bg(gpui::rgb(render::live_text())),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(gpui::rgb(render::live_text()))
                            .child("LIVE"),
                    ),
            );
        } else {
            // Offline: when the last stream's end is known that's the whole
            // story (falling back to its start when the source reports no end
            // — Twitch's IVR); otherwise a plain "offline".
            let last_seen = p
                .status
                .as_ref()
                .and_then(|s| s.last_stream.as_ref())
                .map(|last| {
                    let since = last.ended_at.unwrap_or(last.started_at);
                    format!("last seen {} ago", format_uptime(now - since))
                })
                .unwrap_or_else(|| "offline".to_string());
            header = header.child(
                div()
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(gpui::rgb(render::offline_text()))
                    .child(SharedString::from(last_seen)),
            );
        }
        let mut section = v_flex().gap_1().child(header);
        if let Some(info) = p.status.as_ref().filter(|s| s.live) {
            let title = info.title.trim();
            if !title.is_empty() {
                section = section.child(
                    div()
                        .w_full()
                        .min_w_0()
                        .line_clamp(2)
                        .text_size(px(12.))
                        .child(SharedString::from(title.to_string())),
                );
            }
            // Stats line: uptime · viewers · category. Category goes last so a
            // long name ellipsizes without eating the numbers.
            let mut stats: Vec<String> = Vec::new();
            if let Some(started) = info.started_at {
                stats.push(format_uptime(now - started));
            }
            if let Some(n) = p.viewers {
                stats.push(format!("{} viewers", bks_core::format_count(n)));
            }
            let game = info.game.trim();
            if !game.is_empty() {
                stats.push(game.to_string());
            }
            if !stats.is_empty() {
                section = section.child(
                    div()
                        .w_full()
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.))
                        .text_color(gpui::rgb(render::offline_text()))
                        .child(SharedString::from(stats.join(" · "))),
                );
            }
        }
        col = col.child(section);
    }
    col
}

/// The hand-rolled tab-chip tooltip: [`live_tooltip_content`] in a popover-styled
/// panel, absolutely positioned just below the chip (which is `relative()`) and
/// painted deferred so the chat content below doesn't cover it. Its hover keeps
/// the tooltip up (the channel names are click targets) and schedules the hide
/// grace on leave — see `BackseaterApp::chip_hover_changed` for the state model.
fn chip_tooltip(platforms: Vec<TipPlatform>, cx: &mut Context<BackseaterApp>) -> impl IntoElement {
    gpui::deferred(
        div()
            .absolute()
            .left_0()
            .top(gpui::relative(1.))
            // `anchored` shifts the panel back inside the window when the natural
            // spot (chip's bottom-left) would clip it off an edge — a chip near
            // the right edge gets its tooltip nudged left instead of cut off.
            .child(
                gpui::anchored().snap_to_window_with_margin(px(4.)).child(
                    div()
                        .id("chip-tip")
                        .occlude()
                        .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                            this.chip_tip_hovered = *hovered;
                            if !*hovered {
                                this.schedule_chip_tip_hide(cx);
                            }
                        }))
                        .mt_1()
                        .px_3()
                        .py_2()
                        .min_w(px(240.))
                        .max_w(px(380.))
                        .rounded_lg()
                        .border_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().popover)
                        .text_color(cx.theme().popover_foreground)
                        .text_size(px(13.))
                        .shadow_lg()
                        .child(live_tooltip_content(&platforms)),
                ),
            ),
    )
    .with_priority(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scope lists live in two crates — bks-auth defines what a login tier
    /// requests, bks-twitch defines what the EventSub feed needs — with no
    /// dependency between them. This is the tie: if `channel.moderate` grows a
    /// new required scope, this fails instead of the feed silently going dark
    /// for full-moderator logins.
    #[test]
    fn full_moderator_tier_powers_the_whole_eventsub_feed() {
        use bks_auth::twitch::{ScopeChoice, ScopePreset};
        let auth = |preset| bks_twitch::EventsubAuth {
            client_id: String::new(),
            token: String::new(),
            user_id: String::new(),
            scopes: ScopeChoice {
                preset,
                broadcaster: false,
            }
            .scopes()
            .iter()
            .map(|s| s.to_string())
            .collect(),
        };
        let full = auth(ScopePreset::FullModerator);
        assert!(
            full.wants_moderate(),
            "full tier must cover channel.moderate"
        );
        assert!(full.wants_automod());
        assert!(full.wants_suspicious());
        // Basic deliberately leaves the rich feed off (generic notices remain).
        assert!(!auth(ScopePreset::BasicModeration).feed_available());
    }

    #[test]
    fn flash_alpha_pulses_then_ends() {
        use std::time::Duration;
        // Starts and ends each pulse near zero, peaks at the pulse midpoint.
        assert!(chip_flash_alpha(Duration::ZERO) < 0.05);
        let peak = chip_flash_alpha(TAB_FLASH_PULSE / 2);
        assert!(peak > 0.7, "peak was {peak}");
        // Later pulses are gentler (the overall fade), but still positive mid-window.
        let mid = chip_flash_alpha(TAB_FLASH_PULSE + TAB_FLASH_PULSE / 2);
        assert!(mid > 0.0 && mid < peak, "mid was {mid}");
        // Nothing past the window.
        assert_eq!(chip_flash_alpha(TAB_FLASH_DURATION), 0.0);
        assert_eq!(
            chip_flash_alpha(TAB_FLASH_DURATION + Duration::from_secs(1)),
            0.0
        );
    }

    #[test]
    fn uptime_formats_compactly() {
        use chrono::Duration;
        assert_eq!(format_uptime(Duration::minutes(0)), "0m");
        assert_eq!(format_uptime(Duration::minutes(23)), "23m");
        assert_eq!(format_uptime(Duration::minutes(59)), "59m");
        assert_eq!(format_uptime(Duration::minutes(60)), "1h00m");
        assert_eq!(format_uptime(Duration::minutes(83)), "1h23m");
        assert_eq!(
            format_uptime(Duration::hours(2) + Duration::minutes(5)),
            "2h05m"
        );
        assert_eq!(
            format_uptime(Duration::hours(23) + Duration::minutes(59)),
            "23h59m"
        );
        // A full day or more (a "last live" from days/weeks ago) switches to days + hours.
        assert_eq!(format_uptime(Duration::hours(24)), "1d");
        assert_eq!(format_uptime(Duration::hours(46)), "1d22h");
        assert_eq!(format_uptime(Duration::hours(48)), "2d");
        assert_eq!(format_uptime(Duration::hours(76)), "3d4h");
        assert_eq!(format_uptime(Duration::days(30)), "30d");
        // Seconds round down to the started minute.
        assert_eq!(format_uptime(Duration::seconds(90)), "1m");
        // A negative span (clock skew) clamps rather than going negative.
        assert_eq!(format_uptime(Duration::minutes(-5)), "0m");
    }

    #[test]
    fn channel_kept_only_rejects_replacements() {
        assert!(channel_kept("posty", "posty")); // unchanged
        assert!(channel_kept("", "posty")); // platform added
        assert!(channel_kept("posty", "")); // platform removed
        assert!(channel_kept("", "")); // never had one
        assert!(!channel_kept("posty", "qaixx")); // replaced → new log
    }
}
