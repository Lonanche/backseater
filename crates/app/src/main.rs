//! Backseater — the GPUI desktop app: a tabbed, merged Twitch + Kick chat client.
//!
//! The window has a tab strip on top; each tab is an independent connection (its
//! own Twitch and/or Kick channel, feed, and send target). Login is app-wide and
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
mod sound;
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
    Pixels, Point, ScrollHandle, SharedString, Size, Stateful, Subscription, Task, WeakEntity,
    Window,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::combobox::{Combobox, ComboboxEvent, ComboboxState};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::scroll::ScrollableElement;
use gpui_component::searchable_list::SearchableVec;
use gpui_component::tooltip::Tooltip;
use gpui_component::{
    h_flex, v_flex, ActiveTheme, IconName, IndexPath, Root, Sizable, TitleBar, WindowExt,
};

use chatview::{ChatView, LiveInfo};
use controller::Controller;
use mentions::MentionStore;
use session::Session;
use settings::Settings;
use tabs::TabConfig;

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

/// Default size of the settings child window — a bit bigger than the usercard;
/// the OS resizes it freely from there (the Highlights inputs wrap when narrow).
const SETTINGS_WINDOW_SIZE: Size<Pixels> = Size {
    width: px(700.),
    height: px(660.),
};
/// Smallest the settings window can be resized to — the width floor keeps the
/// content column (after the 150px category sidebar + padding) usable.
const SETTINGS_MIN_SIZE: Size<Pixels> = Size {
    width: px(460.),
    height: px(300.),
};

/// What the settings child window is currently showing.
#[derive(Clone, Copy, PartialEq)]
enum Panel {
    /// App-wide settings (account, appearance, mentions).
    App,
    /// Settings for the tab at this index (name + channels).
    Tab(usize),
}

impl Panel {
    /// The window title for this panel.
    fn title(self) -> &'static str {
        match self {
            Panel::App => "Settings",
            Panel::Tab(_) => "Tab settings",
        }
    }
}

/// The settings inputs, keyed so creation and per-open rebinding share one
/// placeholder table.
#[derive(Clone, Copy)]
enum SettingsInput {
    Name,
    Twitch,
    Kick,
    YouTube,
    Mention,
    Ignore,
    Suppress,
    TabMention,
    TabIgnore,
    TabSuppress,
    MentionsTabName,
    ModName,
    ModIcon,
    ModCommand,
}

/// The font-family dropdown's state type: a searchable list of font names.
type FontCombobox = ComboboxState<SearchableVec<SharedString>>;

/// The state type for a small settings enum-picker dropdown (chat-modes /
/// streamer / mod-button mode), backed by a plain string list.
type SettingSelect = gpui_component::select::SelectState<SearchableVec<SharedString>>;

/// The four Appearance/Streamer/Mod/Link-preview enum-picker dropdowns plus their
/// subscriptions, as returned by [`BackseaterApp::build_setting_selects`].
type SettingSelects = (
    Entity<SettingSelect>,
    Entity<SettingSelect>,
    Entity<SettingSelect>,
    Entity<SettingSelect>,
    Vec<Subscription>,
);

/// The first entry in the font dropdown; selecting it restores the system font
/// (persisted as `font_family: None`).
const DEFAULT_FONT_LABEL: &str = "Default (system)";

/// The gpui-component theme's built-in font family, restored when no custom
/// font is chosen (matches the kit's `Theme` default).
const SYSTEM_FONT_FAMILY: &str = ".SystemUIFont";

/// The window-bound text inputs of the settings panel, built together so the
/// initial construction and each settings-window rebind share one list (kit
/// inputs are window-bound, so they're recreated per open — see
/// [`BackseaterApp::rebind_settings_inputs`]).
struct SettingsInputs {
    name: Entity<InputState>,
    twitch: Entity<InputState>,
    kick: Entity<InputState>,
    youtube: Entity<InputState>,
    mention: Entity<InputState>,
    ignore: Entity<InputState>,
    suppress: Entity<InputState>,
    tab_mention: Entity<InputState>,
    tab_ignore: Entity<InputState>,
    tab_suppress: Entity<InputState>,
    mentions_tab_name: Entity<InputState>,
    mod_name: Entity<InputState>,
    mod_icon: Entity<InputState>,
    mod_command: Entity<InputState>,
}

impl SettingsInputs {
    fn build(window: &mut Window, cx: &mut App) -> Self {
        Self {
            name: settings_input(SettingsInput::Name, window, cx),
            twitch: settings_input(SettingsInput::Twitch, window, cx),
            kick: settings_input(SettingsInput::Kick, window, cx),
            youtube: settings_input(SettingsInput::YouTube, window, cx),
            mention: settings_input(SettingsInput::Mention, window, cx),
            ignore: settings_input(SettingsInput::Ignore, window, cx),
            suppress: settings_input(SettingsInput::Suppress, window, cx),
            tab_mention: settings_input(SettingsInput::TabMention, window, cx),
            tab_ignore: settings_input(SettingsInput::TabIgnore, window, cx),
            tab_suppress: settings_input(SettingsInput::TabSuppress, window, cx),
            mentions_tab_name: settings_input(SettingsInput::MentionsTabName, window, cx),
            mod_name: settings_input(SettingsInput::ModName, window, cx),
            mod_icon: settings_input(SettingsInput::ModIcon, window, cx),
            mod_command: settings_input(SettingsInput::ModCommand, window, cx),
        }
    }
}

/// Creates one settings input bound to `window`.
fn settings_input(which: SettingsInput, window: &mut Window, cx: &mut App) -> Entity<InputState> {
    let placeholder = match which {
        SettingsInput::Name => "Tab name",
        SettingsInput::Twitch => "Twitch channel (optional)",
        SettingsInput::Kick => "Kick channel (optional)",
        SettingsInput::YouTube => "YouTube handle / URL (optional)",
        SettingsInput::Mention => "Add a term (e.g. mods)",
        SettingsInput::Ignore | SettingsInput::Suppress => term_placeholder(TermEntryKind::Text),
        SettingsInput::TabMention => "Add a term for this tab",
        SettingsInput::TabIgnore | SettingsInput::TabSuppress => {
            term_placeholder(TermEntryKind::Text)
        }
        SettingsInput::MentionsTabName => "Mentions",
        SettingsInput::ModName => "Button name (the tooltip)",
        SettingsInput::ModIcon => "Icon — pick below or type text/emoji",
        SettingsInput::ModCommand => "/timeout 1h reason",
    };
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
}

/// The ignore/suppress input placeholder for an add-entry mode. The static
/// table above seeds the Text one on creation; mode-segment clicks and the
/// settings-window rebind keep it pointed at the current mode.
fn term_placeholder(kind: TermEntryKind) -> &'static str {
    match kind {
        TermEntryKind::Text => "Word or phrase (e.g. buy now)",
        TermEntryKind::Regex => "Regular expression (e.g. (twitch\\.)?facepunch\\.com)",
        TermEntryKind::User => "Username (e.g. StreamElements)",
    }
}

/// The app-settings categories, shown as a sidebar of tabs in the settings panel.
/// Each maps to one section body so categories can grow without one giant scroll.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsCategory {
    Account,
    Appearance,
    Themes,
    Highlights,
    ModButtons,
    Streamer,
    About,
}

impl SettingsCategory {
    /// The categories, in sidebar order.
    const ALL: [SettingsCategory; 7] = [
        SettingsCategory::Account,
        SettingsCategory::Appearance,
        SettingsCategory::Themes,
        SettingsCategory::Highlights,
        SettingsCategory::ModButtons,
        SettingsCategory::Streamer,
        SettingsCategory::About,
    ];

    fn label(self) -> &'static str {
        match self {
            SettingsCategory::Account => "Account",
            SettingsCategory::Appearance => "Appearance",
            SettingsCategory::Themes => "Themes",
            SettingsCategory::Highlights => "Highlights",
            SettingsCategory::ModButtons => "Mod Buttons",
            SettingsCategory::Streamer => "Streamer Mode",
            SettingsCategory::About => "About",
        }
    }

    /// The sidebar entry's icon (the kit's bundled lucide set).
    fn icon(self) -> IconName {
        match self {
            SettingsCategory::Account => IconName::CircleUser,
            SettingsCategory::Appearance => IconName::ALargeSmall,
            SettingsCategory::Themes => IconName::Palette,
            SettingsCategory::Highlights => IconName::Bell,
            SettingsCategory::ModButtons => IconName::TriangleAlert,
            SettingsCategory::Streamer => IconName::EyeOff,
            SettingsCategory::About => IconName::Info,
        }
    }
}

/// The tab-settings categories — the same sidebar-rail layout as the app
/// settings, scoped to one tab.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TabSettingsCategory {
    Channels,
    Panels,
    Highlights,
}

impl TabSettingsCategory {
    /// The categories, in sidebar order.
    const ALL: [TabSettingsCategory; 3] = [
        TabSettingsCategory::Channels,
        TabSettingsCategory::Panels,
        TabSettingsCategory::Highlights,
    ];

    fn label(self) -> &'static str {
        match self {
            TabSettingsCategory::Channels => "Channels",
            TabSettingsCategory::Panels => "Panels",
            TabSettingsCategory::Highlights => "Highlights",
        }
    }

    /// The sidebar entry's icon (the kit's bundled lucide set).
    fn icon(self) -> IconName {
        match self {
            TabSettingsCategory::Channels => IconName::Globe,
            TabSettingsCategory::Panels => IconName::LayoutDashboard,
            TabSettingsCategory::Highlights => IconName::Bell,
        }
    }
}

/// One editable color in the custom-theme editor. Each maps to a field on
/// [`settings::CustomTheme`] and gets its own window-bound `ColorPickerState`
/// (see [`BackseaterApp::theme_pickers`]). The order here is the order shown.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ThemeColorField {
    ChatBg,
    DefaultName,
    FirstMessage,
    Highlighted,
    Event,
    Streak,
    Live,
    Offline,
    Mention,
    Link,
    Error,
}

impl ThemeColorField {
    /// The fields in editor order.
    const ALL: [ThemeColorField; 11] = [
        ThemeColorField::ChatBg,
        ThemeColorField::DefaultName,
        ThemeColorField::FirstMessage,
        ThemeColorField::Highlighted,
        ThemeColorField::Event,
        ThemeColorField::Streak,
        ThemeColorField::Live,
        ThemeColorField::Offline,
        ThemeColorField::Mention,
        ThemeColorField::Link,
        ThemeColorField::Error,
    ];

    fn label(self) -> &'static str {
        match self {
            ThemeColorField::ChatBg => "Background",
            ThemeColorField::DefaultName => "Default name",
            ThemeColorField::FirstMessage => "First message",
            ThemeColorField::Highlighted => "Highlighted message",
            ThemeColorField::Event => "Sub / event",
            ThemeColorField::Streak => "Watch streak",
            ThemeColorField::Live => "Went live",
            ThemeColorField::Offline => "Went offline",
            ThemeColorField::Mention => "Mention highlight",
            ThemeColorField::Link => "Links",
            ThemeColorField::Error => "Error",
        }
    }

    /// Reads this field's color out of a saved theme.
    fn get(self, t: &settings::CustomTheme) -> u32 {
        match self {
            ThemeColorField::ChatBg => t.chat_bg,
            ThemeColorField::DefaultName => t.default_name,
            ThemeColorField::FirstMessage => t.first_message,
            // Unset (a theme predating this color) shows the base default swatch.
            ThemeColorField::Highlighted => t
                .highlighted
                .unwrap_or_else(|| render::CustomColors::from_base(t.base_dark).highlighted),
            ThemeColorField::Event => t.event,
            ThemeColorField::Streak => t.streak,
            ThemeColorField::Live => t.live,
            ThemeColorField::Offline => t.offline,
            ThemeColorField::Mention => t.mention,
            ThemeColorField::Link => t.link,
            ThemeColorField::Error => t.error,
        }
    }

    /// Writes this field's color into a saved theme.
    fn set(self, t: &mut settings::CustomTheme, color: u32) {
        match self {
            ThemeColorField::ChatBg => t.chat_bg = color,
            ThemeColorField::DefaultName => t.default_name = color,
            ThemeColorField::FirstMessage => t.first_message = color,
            ThemeColorField::Highlighted => t.highlighted = Some(color),
            ThemeColorField::Event => t.event = color,
            ThemeColorField::Streak => t.streak = color,
            ThemeColorField::Live => t.live = color,
            ThemeColorField::Offline => t.offline = color,
            ThemeColorField::Mention => t.mention = color,
            ThemeColorField::Link => t.link = color,
            ThemeColorField::Error => t.error = color,
        }
    }
}

/// Whether a term list is mention terms, ignore terms, or suppress terms.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TermKind {
    Mentions,
    Ignore,
    Suppress,
}

/// Whether a term list edits the app-wide (global) terms or one tab's own terms.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TermScope {
    Global,
    Tab(usize),
}

/// What an ignore/suppress editor's Add button composes from the input: the
/// text verbatim, a `re:` regex, or a `user:` rule (with the platform picked in
/// the selector). Mentions editors are always plain text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum TermEntryKind {
    #[default]
    Text,
    Regex,
    User,
}

/// One editable term list: a kind (mentions/ignore) at a scope (global/per-tab).
/// The per-tab lists are unioned with the global ones — see `tab_mentions` /
/// `tab_ignore`.
#[derive(Clone, Copy, PartialEq, Eq)]
struct TermList {
    kind: TermKind,
    scope: TermScope,
}

impl TermList {
    fn global(kind: TermKind) -> Self {
        Self {
            kind,
            scope: TermScope::Global,
        }
    }
    fn tab(kind: TermKind, ix: usize) -> Self {
        Self {
            kind,
            scope: TermScope::Tab(ix),
        }
    }

    fn title(self) -> &'static str {
        match (self.kind, self.scope) {
            (TermKind::Mentions, TermScope::Global) => "Mentions",
            (TermKind::Ignore, TermScope::Global) => "Ignore",
            (TermKind::Suppress, TermScope::Global) => "Suppress",
            (TermKind::Mentions, TermScope::Tab(_)) => "Extra mentions (this tab)",
            (TermKind::Ignore, TermScope::Tab(_)) => "Extra ignore (this tab)",
            (TermKind::Suppress, TermScope::Tab(_)) => "Extra suppress (this tab)",
        }
    }

    /// Stem for per-term element ids; includes the scope so a global and a per-tab
    /// editor on screen at once don't collide.
    fn id_stem(self) -> String {
        let kind = match self.kind {
            TermKind::Mentions => "mention",
            TermKind::Ignore => "ignore",
            TermKind::Suppress => "suppress",
        };
        match self.scope {
            TermScope::Global => kind.to_string(),
            TermScope::Tab(ix) => format!("{kind}-tab{ix}"),
        }
    }

    /// The add-mode slot this editor uses (see `BackseaterApp::term_add_modes`):
    /// one per widget — the global and per-tab editors each share one input
    /// entity, so their mode is shared the same way. Mentions editors have no
    /// mode row and never read theirs.
    fn mode_key(self) -> &'static str {
        match (self.kind, self.scope) {
            (TermKind::Ignore, TermScope::Global) => "ignore",
            (TermKind::Suppress, TermScope::Global) => "suppress",
            (TermKind::Ignore, TermScope::Tab(_)) => "ignore-tab",
            (TermKind::Suppress, TermScope::Tab(_)) => "suppress-tab",
            (TermKind::Mentions, _) => "mentions",
        }
    }

    fn description(self) -> &'static str {
        match (self.kind, self.scope) {
            (TermKind::Mentions, TermScope::Global) => {
                "Highlight messages containing these words (your account names always count)."
            }
            (TermKind::Ignore, TermScope::Global) => {
                "Hide matching messages. Text matches as a case-insensitive \
                 substring — e.g. twitch.facepunch.com hides every message with \
                 that link; Regex takes a regular expression; User hides \
                 everything a chatter sends (on any platforms you pick, or all \
                 — you can also toggle this from their usercard)."
            }
            (TermKind::Suppress, TermScope::Global) => {
                "Dim matching messages instead of hiding them — the message \
                 stays in chat at very low opacity so you can skip it but still \
                 read it if you want. Same matching as ignore (Text, Regex, or \
                 User entries). If a term is in both lists, ignore wins."
            }
            (TermKind::Mentions, TermScope::Tab(_)) => {
                "Extra highlight terms for this tab only, added to your global mentions."
            }
            (TermKind::Ignore, TermScope::Tab(_)) => {
                "Hide messages in this tab only (added to your global ignore). \
                 The message still shows in other tabs on the same channel."
            }
            (TermKind::Suppress, TermScope::Tab(_)) => {
                "Dim (but keep visible) messages in this tab only, added to \
                 your global suppress."
            }
        }
    }
}

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
    /// Inputs backing the settings panels (tab fields + term-list adders). Kit
    /// inputs bind their focus/blur/cursor subscriptions to the window they're
    /// created in, so these are recreated against the settings window each time
    /// it opens ([`rebind_settings_inputs`](Self::rebind_settings_inputs)).
    settings_inputs: SettingsInputs,
    /// Applies edits of the Mentions-tab rename input live (label + titles).
    /// Replaced together with the inputs on each settings-window open.
    _settings_mentions_name_sub: Subscription,
    /// The searchable font-family dropdown in Appearance. Window-bound like the
    /// inputs (its popover + search field), so recreated on each settings-window
    /// open; the subscription (selection → [`set_font_family`](Self::set_font_family))
    /// is replaced with it.
    settings_font: Entity<FontCombobox>,
    _settings_font_sub: Subscription,
    /// The Appearance/Streamer/Mod-button enum pickers, as kit Select dropdowns.
    /// Window-bound (their popovers), so recreated on each settings-window open;
    /// each subscription maps the picked index back to its setting.
    settings_chat_modes: Entity<SettingSelect>,
    settings_streamer: Entity<SettingSelect>,
    settings_mod_mode: Entity<SettingSelect>,
    settings_link_preview: Entity<SettingSelect>,
    _settings_select_subs: Vec<Subscription>,
    /// Input for the name of a new/edited theme profile (Themes category).
    /// Window-bound like the other kit inputs.
    settings_theme_name: Entity<InputState>,
    /// One window-bound color picker per curated theme color (aligned to
    /// [`ThemeColorField::ALL`]), plus their subscriptions. Recreated on each
    /// settings-window open ([`rebind_settings_inputs`](Self::rebind_settings_inputs))
    /// because kit picker state binds to the window it's made in.
    settings_theme_pickers: Vec<Entity<gpui_component::color_picker::ColorPickerState>>,
    _settings_theme_subs: Vec<Subscription>,
    /// The custom theme currently being edited in the Themes category. Applied
    /// live as colors change; saved to a profile with the name input. `None` when
    /// no editor is open (a built-in theme is selected and unedited).
    theme_draft: Option<settings::CustomTheme>,
    /// The open settings child window, if any: its OS window handle plus which
    /// panel it shows (app-wide settings or a specific tab's settings).
    settings_window: Option<(AnyWindowHandle, Panel)>,
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
    /// Which category the app-settings panel's sidebar has selected.
    settings_category: SettingsCategory,
    /// The custom-mod-button editor's platform choice (`None` = both platforms),
    /// applied to the next added button.
    mod_button_platform: Option<bks_core::Platform>,
    /// Each ignore/suppress editor's add-entry mode (Text/Regex/User + the
    /// User platform multi-selection, empty = all platforms), keyed per editor
    /// *widget* ([`TermList::mode_key`]) — the tab editors share one input
    /// across tabs, so the mode follows the widget too and can never disagree
    /// with its placeholder. Session-only; absent = plain Text.
    term_add_modes:
        std::collections::HashMap<&'static str, (TermEntryKind, Vec<bks_core::Platform>)>,
    /// The list index of the mod button loaded into the editor by its ✎ (the
    /// row stays in place — highlighted — until Save replaces it in its slot;
    /// Cancel or closing the settings window leaves it untouched).
    editing_mod_button: Option<usize>,
    /// Which category the tab-settings panel's sidebar has selected. Reset to
    /// Channels on each open — that's what a right-click → Settings is for.
    tab_settings_category: TabSettingsCategory,
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
    mentions_scroll: ScrollHandle,
    /// Scroll positions of the settings content panes, so their scrollbars can be
    /// driven persistently (always visible when the content overflows).
    settings_scroll: ScrollHandle,
    tab_settings_scroll: ScrollHandle,
    /// Set when a mention arrived; the global Mentions tab tails on next render.
    mentions_new: bool,
    /// A mention arrived while the Mentions tab wasn't the active view. Drives
    /// the bold-name unread cue on its chip (like a normal tab's `unread`);
    /// cleared when the Mentions tab is selected.
    mentions_unread: bool,
    /// Mention-store subscriptions: row clicks → select the source tab, and
    /// new mentions → tail + repaint the global tab.
    _mention_subs: Vec<Subscription>,
}

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
                this.mentions_new = true;
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

        // Placeholders until the settings window opens and rebinds them to itself.
        let settings_inputs = SettingsInputs::build(window, cx);
        if let Some(name) = settings.mentions_tab_name.clone() {
            settings_inputs
                .mentions_tab_name
                .update(cx, |s, cx| s.set_value(name, window, cx));
        }
        let settings_mentions_name_sub =
            Self::subscribe_mentions_name(&settings_inputs.mentions_tab_name, cx);
        let (settings_font, settings_font_sub) =
            Self::font_combobox(settings.font_family.as_deref(), window, cx);
        let (
            settings_chat_modes,
            settings_streamer,
            settings_mod_mode,
            settings_link_preview,
            settings_select_subs,
        ) = Self::build_setting_selects(&settings, window, cx);
        // Open the theme editor on the active custom theme (if any) so its colors
        // are editable straight away; otherwise no draft (a built-in is selected).
        let theme_draft = settings.active_custom_theme().cloned();
        let (settings_theme_name, settings_theme_pickers, settings_theme_subs) =
            Self::theme_inputs(theme_draft.as_ref(), window, cx);
        if let Some(draft) = &theme_draft {
            let value = draft.name.clone();
            settings_theme_name.update(cx, |s, cx| s.set_value(value, window, cx));
        }

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
            settings_inputs,
            _settings_mentions_name_sub: settings_mentions_name_sub,
            settings_theme_name,
            settings_theme_pickers,
            _settings_theme_subs: settings_theme_subs,
            theme_draft,
            settings_font,
            _settings_font_sub: settings_font_sub,
            settings_chat_modes,
            settings_streamer,
            settings_mod_mode,
            settings_link_preview,
            _settings_select_subs: settings_select_subs,
            settings_window: None,
            main_window: window.window_handle(),
            popouts: Vec::new(),
            popout_views: Vec::new(),
            mentions_window: None,
            settings_category: SettingsCategory::Account,
            mod_button_platform: None,
            term_add_modes: std::collections::HashMap::new(),
            editing_mod_button: None,
            tab_settings_category: TabSettingsCategory::Channels,
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
            mentions_scroll: ScrollHandle::new(),
            settings_scroll: ScrollHandle::new(),
            tab_settings_scroll: ScrollHandle::new(),
            mentions_new: false,
            mentions_unread: false,
            _mention_subs,
        }
    }

    /// Recreates the settings inputs against `window` (the settings child
    /// window). Kit inputs are window-bound — one created for the main window
    /// wouldn't get focus/blur/cursor events in a child window.
    fn rebind_settings_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_inputs = SettingsInputs::build(window, cx);
        // The rebuilt inputs are empty, so a mod-button edit left open when the
        // window last closed is implicitly cancelled with them.
        self.editing_mod_button = None;
        // The fresh ignore/suppress inputs carry the Text placeholder, but the
        // editors' add modes persist for the session — re-point each one.
        for list in [
            TermList::global(TermKind::Ignore),
            TermList::global(TermKind::Suppress),
            TermList::tab(TermKind::Ignore, 0),
            TermList::tab(TermKind::Suppress, 0),
        ] {
            self.sync_term_placeholder(list, window, cx);
        }
        if let Some(name) = self.settings.mentions_tab_name.clone() {
            self.settings_inputs
                .mentions_tab_name
                .update(cx, |s, cx| s.set_value(name, window, cx));
        }
        self._settings_mentions_name_sub =
            Self::subscribe_mentions_name(&self.settings_inputs.mentions_tab_name, cx);
        let (font, font_sub) =
            Self::font_combobox(self.settings.font_family.as_deref(), window, cx);
        self.settings_font = font;
        self._settings_font_sub = font_sub;
        let (chat_modes, streamer, mod_mode, link_preview, select_subs) =
            Self::build_setting_selects(&self.settings, window, cx);
        self.settings_chat_modes = chat_modes;
        self.settings_streamer = streamer;
        self.settings_mod_mode = mod_mode;
        self.settings_link_preview = link_preview;
        self._settings_select_subs = select_subs;
        // Rebind the theme editor's inputs (name + color pickers) to this window.
        let (theme_name, theme_pickers, theme_subs) =
            Self::theme_inputs(self.theme_draft.as_ref(), window, cx);
        self.settings_theme_name = theme_name;
        self.settings_theme_pickers = theme_pickers;
        self._settings_theme_subs = theme_subs;
        if let Some(draft) = &self.theme_draft {
            let value = draft.name.clone();
            self.settings_theme_name.update(cx, |s, cx| s.set_value(value, window, cx));
        }
    }

    /// Creates the font-family dropdown bound to `window`: "Default (system)"
    /// followed by every installed font (sorted), with the current choice
    /// pre-selected. The subscription applies a selection app-wide.
    fn font_combobox(
        current: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<FontCombobox>, Subscription) {
        let mut names = cx.text_system().all_font_names();
        names.sort_by_key(|a| a.to_lowercase());
        names.dedup();
        let mut items = vec![SharedString::from(DEFAULT_FONT_LABEL)];
        items.extend(names.into_iter().map(SharedString::from));
        // An unknown saved font (uninstalled since) just shows no selection.
        let selected = current
            .and_then(|f| items.iter().position(|n| n.as_ref() == f))
            .unwrap_or(0);
        let list = SearchableVec::new(items);
        let state = cx.new(|cx| {
            ComboboxState::new(list, vec![IndexPath::default().row(selected)], window, cx)
                .searchable(true)
        });
        let sub = cx.subscribe(&state, |this, _, event: &ComboboxEvent<_>, cx| {
            if let ComboboxEvent::Change(values) = event {
                let family = values
                    .first()
                    .filter(|v| v.as_ref() != DEFAULT_FONT_LABEL)
                    .map(|v| v.to_string());
                this.set_font_family(family, cx);
            }
        });
        (state, sub)
    }

    /// Builds one small enum-picker dropdown ([`SettingSelect`]) bound to
    /// `window`: a non-searchable list of `labels` with `selected` pre-picked.
    /// On confirm, the picked label's index is mapped back through `on_pick`.
    fn setting_select(
        labels: &[&'static str],
        selected: usize,
        on_pick: impl Fn(&mut Self, usize, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<SettingSelect>, Subscription) {
        let items: Vec<SharedString> = labels.iter().map(|s| SharedString::from(*s)).collect();
        let list = SearchableVec::new(items.clone());
        let state = cx.new(|cx| {
            SettingSelect::new(list, Some(IndexPath::default().row(selected)), window, cx)
                .searchable(false)
        });
        use gpui_component::select::SelectEvent;
        let sub = cx.subscribe(&state, move |this, _, event: &SelectEvent<_>, cx| {
            let SelectEvent::Confirm(value) = event;
            if let Some(value) = value {
                if let Some(ix) = items.iter().position(|v| v == value) {
                    on_pick(this, ix, cx);
                }
            }
        });
        (state, sub)
    }

    /// Builds the three Appearance/Streamer/Mod-button enum-picker dropdowns and
    /// their subscriptions from the current settings. Used by the constructor and
    /// [`rebind_settings_inputs`](Self::rebind_settings_inputs) (kit Select state
    /// is window-bound, so they're recreated on each settings-window open).
    fn build_setting_selects(
        settings: &Settings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> SettingSelects {
        let cm = settings::ChatModesPlacement::ALL
            .iter()
            .position(|c| *c == settings.chat_modes_placement)
            .unwrap_or(0);
        let (chat_modes, s1) = Self::setting_select(
            settings::ChatModesPlacement::LABELS,
            cm,
            |this, ix, cx| this.set_chat_modes_placement(settings::ChatModesPlacement::ALL[ix], cx),
            window,
            cx,
        );

        let sm = settings::StreamerModeChoice::ALL
            .iter()
            .position(|c| *c == settings.streamer_mode)
            .unwrap_or(0);
        let (streamer, s2) = Self::setting_select(
            settings::StreamerModeChoice::LABELS,
            sm,
            |this, ix, cx| this.set_streamer_mode(settings::StreamerModeChoice::ALL[ix], cx),
            window,
            cx,
        );

        let mm = settings::ModButtonMode::ALL
            .iter()
            .position(|c| *c == settings.mod_button_mode)
            .unwrap_or(0);
        let (mod_mode, s3) = Self::setting_select(
            settings::ModButtonMode::LABELS,
            mm,
            |this, ix, cx| this.set_mod_button_mode(settings::ModButtonMode::ALL[ix], cx),
            window,
            cx,
        );

        let lp = settings::LinkPreviewMode::ALL
            .iter()
            .position(|c| *c == settings.link_preview_mode)
            .unwrap_or(0);
        let (link_preview, s4) = Self::setting_select(
            settings::LinkPreviewMode::LABELS,
            lp,
            |this, ix, cx| this.set_link_preview_mode(settings::LinkPreviewMode::ALL[ix], cx),
            window,
            cx,
        );

        (chat_modes, streamer, mod_mode, link_preview, vec![s1, s2, s3, s4])
    }

    /// Reseeds the three enum-picker dropdowns from the current settings when the
    /// settings window is open, so a change made through another path (e.g. the
    /// streamer banner's "Turn off" flipping `streamer_mode` while settings is
    /// open) is reflected — the kit Select holds its selection internally, unlike
    /// the old stateless segmented controls. A no-op when the value already
    /// matches (so a change *from* the dropdown doesn't loop). Needs the settings
    /// window for the window-bound `set_selected_index`; skipped when it's closed.
    fn resync_setting_selects(&self, cx: &mut Context<Self>) {
        let Some((handle, _)) = self.settings_window else {
            return;
        };
        let targets = [
            (
                self.settings_chat_modes.clone(),
                settings::ChatModesPlacement::ALL
                    .iter()
                    .position(|c| *c == self.settings.chat_modes_placement)
                    .unwrap_or(0),
            ),
            (
                self.settings_streamer.clone(),
                settings::StreamerModeChoice::ALL
                    .iter()
                    .position(|c| *c == self.settings.streamer_mode)
                    .unwrap_or(0),
            ),
            (
                self.settings_mod_mode.clone(),
                settings::ModButtonMode::ALL
                    .iter()
                    .position(|c| *c == self.settings.mod_button_mode)
                    .unwrap_or(0),
            ),
            (
                self.settings_link_preview.clone(),
                settings::LinkPreviewMode::ALL
                    .iter()
                    .position(|c| *c == self.settings.link_preview_mode)
                    .unwrap_or(0),
            ),
        ];
        let _ = handle.update(cx, |_, window, cx| {
            for (state, want) in targets {
                let current = state.read(cx).selected_index(cx).map(|ix| ix.row);
                if current != Some(want) {
                    state.update(cx, |s, cx| {
                        s.set_selected_index(Some(IndexPath::default().row(want)), window, cx);
                    });
                }
            }
        });
    }

    /// Builds the Themes category's window-bound inputs: the profile-name field
    /// and one [`ColorPickerState`](gpui_component::color_picker::ColorPickerState)
    /// per curated color, seeded from `draft` (or the dark base if none). Each
    /// picker's Change updates the draft's matching field live and re-applies the
    /// theme. Recreated on each settings-window open (kit state is window-bound).
    fn theme_inputs(
        draft: Option<&settings::CustomTheme>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (
        Entity<InputState>,
        Vec<Entity<gpui_component::color_picker::ColorPickerState>>,
        Vec<Subscription>,
    ) {
        use gpui_component::color_picker::{ColorPickerEvent, ColorPickerState};
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Theme name"));
        // Seed values from the draft, else the dark base (matches "New theme").
        let seed = draft
            .cloned()
            .unwrap_or_else(|| default_custom_theme(true, String::new()));
        let mut pickers = Vec::with_capacity(ThemeColorField::ALL.len());
        let mut subs = Vec::with_capacity(ThemeColorField::ALL.len());
        for field in ThemeColorField::ALL {
            let start = packed_to_hsla(field.get(&seed));
            let state =
                cx.new(|cx| ColorPickerState::new(window, cx).default_value(start));
            subs.push(cx.subscribe(
                &state,
                move |this, _, ev: &ColorPickerEvent, cx| {
                    let ColorPickerEvent::Change(color) = ev;
                    if let Some(color) = color {
                        this.set_theme_color(field, hsla_to_packed(*color), cx);
                    }
                },
            ));
            pickers.push(state);
        }
        (name, pickers, subs)
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
        cx.subscribe(
            view,
            move |this, _view, ev: &chatview::TabWentLive, cx| {
                if !this.settings.flash_tab_on_live {
                    return;
                }
                let Some(ix) = this.tabs.iter().position(|t| t.id == id) else {
                    return;
                };
                this.tabs[ix].flash_start = Some((std::time::Instant::now(), ev.platform));
                this.schedule_tab_flash_tick(cx);
                cx.notify();
            },
        )
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
                .update(cx, |view, _| view.set_mentions(matcher));
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
                view.set_suppress(suppress);
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
        self.popout_views.retain(|(_, weak)| weak.upgrade().is_some());
        for (tab_id, weak) in &self.popout_views {
            let (Some(view), Some(tab)) = (
                weak.upgrade(),
                self.tabs.iter().find(|t| t.id == *tab_id),
            ) else {
                continue;
            };
            let ignore = self.tab_ignore(&tab.config);
            let suppress = self.tab_suppress(&tab.config);
            view.update(cx, |view, cx| {
                view.set_ignore(ignore, cx);
                view.set_suppress(suppress);
                view.refresh_log(cx);
            });
        }
    }

    /// Tracks a popped-out view so [`refresh_popout_filters`](
    /// Self::refresh_popout_filters) can reach it (popouts aren't in `tabs`).
    fn track_popout_view(&mut self, tab_id: u64, view: &Entity<ChatView>) {
        self.popout_views.push((tab_id, view.downgrade()));
    }

    fn persist(&self) {
        let configs: Vec<TabConfig> = self.tabs.iter().map(|t| t.config.clone()).collect();
        tabs::save(&configs);
        // The active index can shift with any structural change (add/close/move),
        // so save it alongside the list.
        tabs::save_active(self.active);
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
            self.persist();
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
        self.persist();
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
        self.persist();
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
        self.persist();
        cx.notify();
    }

    /// Opens the settings window for tab `ix`, pre-filled with its current values.
    fn open_settings(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.tabs.get(ix).is_none() {
            return;
        }
        self.tab_settings_category = TabSettingsCategory::Channels;
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
        // with the bounds — see `child_window::open`).
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

    /// Subscribes the Mentions-tab rename input so edits apply live (created
    /// alongside the input on each settings-window rebind).
    fn subscribe_mentions_name(
        input: &Entity<InputState>,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe(input, |this, state, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                let value = state.read(cx).value().to_string();
                this.set_mentions_tab_name(value, cx);
            }
        })
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
        self.settings.save();
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
        self.settings.save();
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
        // Reuse an open window: swap its panel + title, re-prefill, focus it.
        if let Some((handle, _)) = app.read(cx).settings_window {
            let reused = handle.update(cx, |_, window, cx| {
                window.set_window_title(panel.title());
                app.update(cx, |this, cx| {
                    this.settings_window = Some((handle, panel));
                    if let Panel::Tab(ix) = panel {
                        this.prefill_tab_settings(ix, window, cx);
                    }
                    cx.notify();
                });
                window.activate_window();
            });
            if reused.is_ok() {
                return;
            }
            // The window closed under us — fall through and open a fresh one.
        }

        // Always opens centered over the chat window; drag it away from there.
        // Bare: the settings body draws its own full-height sidebar + scrolling
        // content pane instead of the shared padded panel surface.
        let opened = child_window::open_centered_bare(
            panel.title(),
            SETTINGS_WINDOW_SIZE,
            SETTINGS_MIN_SIZE,
            app.read(cx).main_window,
            app.clone(),
            |this, cx| this.settings_body(cx),
            cx,
        );
        let Ok((handle, content)) = opened else {
            return;
        };
        content.update(cx, |w, cx| w.close_on_escape(cx));
        let _ = handle.update(cx, |_, window, cx| {
            app.update(cx, |this, cx| {
                this.rebind_settings_inputs(window, cx);
                if let Panel::Tab(ix) = panel {
                    this.prefill_tab_settings(ix, window, cx);
                }
                this.settings_window = Some((handle, panel));
                // The user closing the window (OS ✕) releases its content view;
                // clear the state then — unless a newer window replaced it.
                cx.observe_release(&content, move |this, _, cx| {
                    if this.settings_window.map(|(h, _)| h) == Some(handle) {
                        this.settings_window = None;
                    }
                    cx.notify();
                })
                .detach();
                cx.notify();
            });
        });
    }

    /// Pre-fills the tab-settings inputs from tab `ix`'s current config.
    /// `window` must be the settings window the inputs are bound to.
    fn prefill_tab_settings(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        let cfg = tab.config.clone();
        self.settings_inputs.name
            .update(cx, |s, cx| s.set_value(&cfg.name, window, cx));
        self.settings_inputs.twitch
            .update(cx, |s, cx| s.set_value(&cfg.twitch_channel, window, cx));
        self.settings_inputs.kick
            .update(cx, |s, cx| s.set_value(&cfg.kick_channel, window, cx));
        self.settings_inputs.youtube
            .update(cx, |s, cx| s.set_value(&cfg.youtube_channel, window, cx));
    }

    /// The settings window's content, dispatched on the current panel. The
    /// window is bare (no built-in padding/scroll): the app panel draws its own
    /// sidebar + content pane; a tab panel gets a plain padded scroll here.
    fn settings_body(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        match self.settings_window {
            Some((_, Panel::App)) => self.app_settings_body(cx),
            Some((_, Panel::Tab(ix))) => self.tab_settings_body(ix, cx),
            None => gpui::Empty.into_any_element(),
        }
    }

    /// Applies edited settings to tab `ix`: renames it and, if its channels
    /// changed, rebuilds the feed (a fresh connection), then persists. Reads the
    /// name + channel fields off the (reused) settings input entities.
    fn apply_settings(&mut self, ix: usize, cx: &mut Context<Self>) {
        let name = self.settings_inputs.name.read(cx).value().trim().to_string();
        let twitch = self.settings_inputs.twitch.read(cx).value().trim().to_string();
        let kick = self.settings_inputs.kick.read(cx).value().trim().to_string();
        let youtube = self.settings_inputs.youtube.read(cx).value().trim().to_string();

        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        let channels_changed = tab.config.twitch_channel != twitch
            || tab.config.kick_channel != kick
            || tab.config.youtube_channel != youtube;
        // Adding or removing platforms (no channel *replaced* by a different
        // one)? Then the tab reconnects in place and keeps its log — the other
        // platforms shouldn't visibly drop and reload, and a removed platform's
        // rows stay as scrollback. Only swapping a channel for a different one
        // rebuilds from scratch (a different channel means a different log).
        let keep_log = channel_kept(&tab.config.twitch_channel, &twitch)
            && channel_kept(&tab.config.kick_channel, &kick)
            && channel_kept(&tab.config.youtube_channel, &youtube);

        let mut config = tab.config.clone();
        config.twitch_channel = twitch;
        config.kick_channel = kick;
        config.youtube_channel = youtube;
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
        self.persist();
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
        self.persist();
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
        self.persist();
        cx.notify();
    }

    /// Pushes tab `ix`'s (already-mutated) events filters to its live view and
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
        tab.view.update(cx, |view, cx| {
            view.set_events_filter(kinds, only, hide_msgs, collapse, cx)
        });
        self.persist();
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
        self.settings_category = SettingsCategory::Account;
        if !matches!(self.settings_window, Some((_, Panel::App))) {
            self.show_settings_panel(Panel::App, cx);
        }
        cx.notify();
    }

    /// The body of the app-settings panel: a full-height category rail on the
    /// left (its own surface, icons + labels) and the selected category's
    /// sections in an independently scrolling content pane, headed by the
    /// category name. Built fresh each render so it tracks live login/size/theme
    /// changes.
    fn app_settings_body(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let selected = self.settings_category;
        let rail: Vec<gpui::AnyElement> = SettingsCategory::ALL
            .into_iter()
            .map(|cat| {
                rail_item(cat.icon(), cat.label(), cat == selected, cx)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.settings_category = cat;
                            cx.notify();
                        }),
                    )
                    .into_any_element()
            })
            .collect();

        let body = match selected {
            SettingsCategory::Account => v_flex().gap_5().child(self.account_section(cx)),
            SettingsCategory::Appearance => v_flex().gap_5().child(self.appearance_section(cx)),
            SettingsCategory::Themes => v_flex().gap_5().child(self.themes_section(cx)),
            SettingsCategory::Highlights => v_flex()
                .gap_5()
                .child(self.term_list_section(TermList::global(TermKind::Mentions), cx))
                .child(self.mentions_tab_section(cx))
                .child(self.term_list_section(TermList::global(TermKind::Ignore), cx))
                .child(self.term_list_section(TermList::global(TermKind::Suppress), cx))
                .child(self.suppressed_opacity_section(cx)),
            SettingsCategory::ModButtons => {
                v_flex().gap_5().child(self.mod_buttons_section(cx))
            }
            SettingsCategory::Streamer => v_flex().gap_5().child(self.streamer_section(cx)),
            SettingsCategory::About => v_flex().gap_5().child(self.about_section(cx)),
        };

        settings_shell(
            rail,
            selected.label(),
            body.into_any_element(),
            "settings-scroll",
            &self.settings_scroll,
            cx,
        )
    }

    /// The body of a tab-settings panel: the same sidebar-rail shell as the app
    /// settings, with Channels (name + channel fields + Save), Panels (the
    /// events/mentions panel card), and Highlights (this tab's terms).
    fn tab_settings_body(&mut self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let selected = self.tab_settings_category;
        let rail: Vec<gpui::AnyElement> = TabSettingsCategory::ALL
            .into_iter()
            .map(|cat| {
                rail_item(cat.icon(), cat.label(), cat == selected, cx)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.tab_settings_category = cat;
                            cx.notify();
                        }),
                    )
                    .into_any_element()
            })
            .collect();

        let body = match selected {
            TabSettingsCategory::Channels => {
                v_flex().gap_5().child(self.tab_channels_section(ix, cx))
            }
            TabSettingsCategory::Panels => {
                v_flex().gap_5().child(self.events_panel_section(ix, cx))
            }
            TabSettingsCategory::Highlights => v_flex()
                .gap_5()
                .child(self.term_list_section(TermList::tab(TermKind::Mentions, ix), cx))
                .child(self.term_list_section(TermList::tab(TermKind::Ignore, ix), cx))
                .child(self.term_list_section(TermList::tab(TermKind::Suppress, ix), cx)),
        };

        settings_shell(
            rail,
            selected.label(),
            body.into_any_element(),
            "tab-settings-scroll",
            &self.tab_settings_scroll,
            cx,
        )
    }

    /// The Channels category of a tab's settings: the tab name + one channel
    /// field per platform, applied by Save (unlike the live-toggling switches,
    /// a channel change reconnects the tab, so it waits for an explicit apply).
    fn tab_channels_section(&self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        v_flex()
            .gap_2()
            .child(field("Name", &self.settings_inputs.name))
            .child(field("Twitch channel", &self.settings_inputs.twitch))
            .child(field("Kick channel", &self.settings_inputs.kick))
            .child(field("YouTube channel", &self.settings_inputs.youtube))
            .child(
                h_flex().justify_end().mt_2().child(
                    Button::new("save-tab-settings")
                        .label("Save")
                        .primary()
                        .small()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.apply_settings(ix, cx);
                            // This button lives in the settings window; closing
                            // is just removing the window we're dispatched in.
                            this.settings_window = None;
                            window.remove_window();
                            cx.notify();
                        })),
                ),
            )
            .into_any_element()
    }

    /// The Panels category of a tab's settings: an Events-panel card (show
    /// toggle plus, when on, its behavior switches and kind checklist) and a
    /// separate Mentions-panel card. All toggle live (no Save) and persist
    /// immediately. Built fresh each render, so it reflects the tab's current
    /// config.
    fn events_panel_section(&self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        use gpui_component::checkbox::Checkbox;
        use gpui_component::switch::Switch;

        let Some(tab) = self.tabs.get(ix) else {
            return div().into_any_element();
        };
        let show = tab.config.layout.contains(tabs::PanelKind::Events);
        let filter = tab.config.event_kinds;

        let mut card = setting_card().child(setting_row(
            "Show events panel",
            Some("Subs, raids, and other channel events in a side panel."),
            Switch::new("show-events-panel")
                .small()
                .checked(show)
                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                    this.set_panel_shown(ix, tabs::PanelKind::Events, *checked, cx);
                }))
                .into_any_element(),
        ));

        if show {
            let events_only = tab.config.events_only;
            card = card.child(card_divider()).child(setting_row(
                "Events only",
                Some("Hide events from the chat log; they show only in the panel."),
                Switch::new("events-only")
                    .small()
                    .checked(events_only)
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        this.set_events_only(ix, *checked, cx);
                    }))
                    .into_any_element(),
            ));

            card = card.child(card_divider()).child(setting_row(
                "Hide sub messages",
                Some("Show only the sub info in the panel, without the attached chat message."),
                Switch::new("hide-sub-messages")
                    .small()
                    .checked(tab.config.hide_sub_messages)
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        this.set_hide_sub_messages(ix, *checked, cx);
                    }))
                    .into_any_element(),
            ));

            card = card.child(card_divider()).child(setting_row(
                "Collapse gift batches",
                Some("One \"gifted 50 subs\" row, expandable to the recipients, instead of 50 rows."),
                Switch::new("collapse-gift-subs")
                    .small()
                    .checked(tab.config.collapse_gift_subs)
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        this.set_collapse_gift_subs(ix, *checked, cx);
                    }))
                    .into_any_element(),
            ));

            let kinds = EventKind::ALL.into_iter().map(|kind| {
                Checkbox::new(SharedString::from(format!("event-kind-{}", kind.label())))
                    .label(kind.label())
                    .checked(filter.enabled(kind))
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        this.set_event_kind(ix, kind, *checked, cx);
                    }))
            });
            card = card.child(card_divider()).child(
                v_flex()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .children(kinds.map(IntoElement::into_any_element)),
            );
        }

        let show_mentions = tab.config.layout.contains(tabs::PanelKind::Mentions);
        let mut mentions_card = setting_card().child(setting_row(
            "Show mentions panel",
            Some("Messages that mention you, in a side panel."),
            Switch::new("show-mentions-panel")
                .small()
                .checked(show_mentions)
                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                    this.set_panel_shown(ix, tabs::PanelKind::Mentions, *checked, cx);
                }))
                .into_any_element(),
        ));

        if show_mentions {
            mentions_card = mentions_card.child(card_divider()).child(setting_row(
                "Mentions from all tabs",
                Some("Click a mention to jump to its tab."),
                Switch::new("mentions-all-tabs")
                    .small()
                    .checked(tab.config.mentions_all_tabs)
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        this.set_mentions_all_tabs(ix, *checked, cx);
                    }))
                    .into_any_element(),
            ));
        }

        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_2()
                    .child(section_title("Events panel"))
                    .child(card),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(section_title("Mentions panel"))
                    .child(mentions_card),
            )
            .into_any_element()
    }

    /// Renders the Account section: one card row per platform (logo, login
    /// status, a Log in / Log out button). Actions go through the active tab's
    /// controller.
    fn account_section(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let state = self.session.login_state();
        v_flex()
            .gap_2()
            .child(section_title("Accounts"))
            .child(
                setting_card()
                    .child(self.account_row(
                        bks_core::Platform::Twitch,
                        state.twitch,
                        cx,
                        |c| c.twitch_login(),
                        |c| c.twitch_logout(),
                    ))
                    .child(card_divider())
                    .child(self.account_row(
                        bks_core::Platform::Kick,
                        state.kick,
                        cx,
                        |c| c.kick_login(),
                        |c| c.kick_logout(),
                    )),
            )
            .into_any_element()
    }

    /// One platform's account row: logo + platform name with the account name
    /// (or "Not logged in") under it, and a Log in / Log out button at the
    /// right. `account` is `Some(name)` when logged in.
    fn account_row(
        &self,
        platform: bks_core::Platform,
        account: Option<String>,
        cx: &mut Context<Self>,
        login: fn(&Controller),
        logout: fn(&Controller),
    ) -> gpui::AnyElement {
        let logged_in = account.is_some();
        let status = match &account {
            Some(name) => format!("Logged in as {name}"),
            None => "Not logged in".to_string(),
        };
        let label = platform.label();
        let button = if logged_in {
            Button::new(SharedString::from(format!("logout-{label}")))
                .label("Log out")
                .small()
                .danger()
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(c) = this.active_controller(cx) {
                        logout(&c);
                    }
                    cx.notify();
                }))
        } else {
            Button::new(SharedString::from(format!("login-{label}")))
                .label("Log in")
                .small()
                .primary()
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(c) = this.active_controller(cx) {
                        login(&c);
                    }
                    cx.notify();
                }))
        };
        h_flex()
            .w_full()
            .items_center()
            .gap_3()
            .px_3()
            .py_2p5()
            .child(
                div()
                    .flex_none()
                    .w(px(22.))
                    .flex()
                    .justify_center()
                    .child(platform_icon(platform, 20.)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::MEDIUM)
                            .child(SharedString::from(label)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(SharedString::from(status)),
                    ),
            )
            .child(button)
            .into_any_element()
    }

    /// Renders the Themes section: a selector (Dark / Light / each saved custom
    /// theme), a "New theme" button, and — when a custom theme is being edited —
    /// the color-picker editor with Save / Cancel.
    fn themes_section(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        use gpui_component::button::Button;

        let active = &self.settings.theme;
        // One selectable row for a theme: click to activate; custom rows also get
        // Edit + Delete buttons.
        let chip = |label: SharedString,
                    selected: bool,
                    swatch: u32,
                    choice: settings::ThemeChoice,
                    custom: Option<String>,
                    cx: &mut Context<Self>| {
            let id = SharedString::from(format!("theme-sel-{label}"));
            // A selected row reads as selected in both light and dark: a filled
            // accent tint plus a 2px accent bar on the left. Hover is a plainer
            // `secondary` fill so it never looks like the selection. (Previously
            // both used `secondary`, which in dark mode is nearly the card
            // background — the selected theme was indistinguishable.)
            let accent = cx.theme().primary;
            h_flex()
                .id(id)
                .w_full()
                .items_center()
                .justify_between()
                .pr_3()
                .pl(px(10.))
                .py_2()
                .rounded_md()
                .cursor_pointer()
                .border_l_2()
                .border_color(if selected { accent } else { gpui::transparent_black() })
                .when(selected, |s| s.bg(accent.opacity(0.16)))
                .when(!selected, |s| s.hover(|s| s.bg(cx.theme().secondary)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        this.set_theme(choice.clone(), window, cx)
                    }),
                )
                .child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .child(
                            // A ring around the swatch so a near-black Dark
                            // swatch stays visible against a dark card.
                            div()
                                .size(px(16.))
                                .rounded_sm()
                                .border_1()
                                .border_color(cx.theme().muted_foreground.opacity(0.6))
                                .bg(gpui::rgb(swatch)),
                        )
                        .child(
                            div()
                                .when(selected, |s| {
                                    s.font_weight(FontWeight::MEDIUM).text_color(accent)
                                })
                                .child(label),
                        ),
                )
                .when_some(custom, |row, name| {
                    let edit_name = name.clone();
                    row.child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new(SharedString::from(format!("edit-{name}")))
                                    .label("Edit")
                                    .xsmall()
                                    .ghost()
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.edit_theme(&edit_name, window, cx);
                                    })),
                            )
                            .child(
                                Button::new(SharedString::from(format!("del-{name}")))
                                    .label("✕")
                                    .xsmall()
                                    .ghost()
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.delete_theme(&name, window, cx);
                                    })),
                            ),
                    )
                })
        };

        let mut selector = v_flex()
            .gap_1()
            .child(chip(
                SharedString::from("Dark"),
                *active == settings::ThemeChoice::Dark,
                0x1a1a1d,
                settings::ThemeChoice::Dark,
                None,
                cx,
            ))
            .child(chip(
                SharedString::from("Light"),
                *active == settings::ThemeChoice::Light,
                0xf7f7f8,
                settings::ThemeChoice::Light,
                None,
                cx,
            ));
        for theme in &self.settings.custom_themes {
            selector = selector.child(chip(
                SharedString::from(theme.name.clone()),
                active.custom_name() == Some(theme.name.as_str()),
                theme.chat_bg,
                settings::ThemeChoice::Custom(theme.name.clone()),
                Some(theme.name.clone()),
                cx,
            ));
        }

        let mut body = v_flex()
            .gap_2()
            .child(section_title("Theme"))
            .child(setting_card().p_1().gap_0p5().child(selector))
            .child(
                h_flex().child(
                    Button::new("new-theme")
                        .label("+ New theme")
                        .small()
                        .outline()
                        .on_click(cx.listener(|this, _, window, cx| this.new_theme(window, cx))),
                ),
            );

        if self.theme_draft.is_some() {
            body = body.child(self.theme_editor(cx));
        }
        body.into_any_element()
    }

    /// The color-picker editor for the current [`theme_draft`](Self::theme_draft):
    /// a name field, one picker per curated color, and Save / Cancel.
    fn theme_editor(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_component::button::Button;
        use gpui_component::color_picker::ColorPicker;

        let mut card = setting_card().child(setting_row(
            "Name",
            None,
            Input::new(&self.settings_theme_name)
                .w(px(200.))
                .into_any_element(),
        ));
        for (i, field) in ThemeColorField::ALL.into_iter().enumerate() {
            let Some(picker) = self.settings_theme_pickers.get(i) else {
                continue;
            };
            card = card.child(card_divider()).child(setting_row(
                field.label(),
                None,
                ColorPicker::new(picker).into_any_element(),
            ));
        }

        v_flex()
            .gap_2()
            .mt_2()
            .child(section_title("Edit theme"))
            .child(card)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("save-theme")
                            .label("Save theme")
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _, window, cx| this.save_theme(window, cx))),
                    )
                    .child(
                        Button::new("cancel-theme")
                            .label("Cancel")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_theme_edit(cx))),
                    ),
            )
    }

    /// Renders the Appearance section: a Font card (family + size) and a Chat
    /// card (7TV name colors, live status bar, pinned-message banners), each a
    /// label-left / control-right row.
    fn appearance_section(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        use gpui_component::switch::Switch;
        let size = self.settings.font_size;
        let stepper = h_flex()
            .items_center()
            .gap_2()
            .child(
                Button::new("font-smaller")
                    .label("–")
                    .small()
                    .outline()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.adjust_font_size(-1.0, cx);
                    })),
            )
            .child(
                div()
                    .w(px(44.))
                    .text_center()
                    .text_size(px(13.))
                    .child(SharedString::from(format!("{size:.0} px"))),
            )
            .child(
                Button::new("font-larger")
                    .label("+")
                    .small()
                    .outline()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.adjust_font_size(1.0, cx);
                    })),
            );
        v_flex()
            .gap_2()
            .child(section_title("Font"))
            .child(
                setting_card()
                    .child(setting_row(
                        "Font",
                        None,
                        Combobox::new(&self.settings_font)
                            .w(px(220.))
                            .menu_max_h(px(320.))
                            .placeholder(DEFAULT_FONT_LABEL)
                            .into_any_element(),
                    ))
                    .child(card_divider())
                    .child(setting_row(
                        "Chat font size",
                        None,
                        stepper.into_any_element(),
                    )),
            )
            .child(div().h_1())
            .child(section_title("Chat"))
            .child(
                setting_card()
                    .child(setting_row(
                        "7TV name colors",
                        Some("Render 7TV paints (gradient/solid name colors) and 7TV badges."),
                        Switch::new("show-7tv-paints")
                            .small()
                            .checked(self.settings.show_7tv_paints)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.set_show_7tv_paints(*checked, cx);
                            }))
                            .into_any_element(),
                    ))
                    .child(card_divider())
                    .child(setting_row(
                        "Live status bar",
                        Some("Channel + viewer count above chat while a stream is live."),
                        Switch::new("show-status-bar")
                            .small()
                            .checked(self.settings.show_status_bar)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.set_show_status_bar(*checked, cx);
                            }))
                            .into_any_element(),
                    ))
                    .child(card_divider())
                    .child(setting_row(
                        "Chat modes bar",
                        Some(
                            "Where active restrictions (slow, followers-only, sub-only, \
                             ...) show: off, at the top of the chat panel, or above the input.",
                        ),
                        self.chat_modes_placement_seg(),
                    ))
                    .child(card_divider())
                    .child(setting_row(
                        "Link previews",
                        Some(
                            "Show a YouTube video's or a Twitch/Kick clip's title, channel, \
                             views, and thumbnail: off, as a hover tooltip, or as a card \
                             inline in chat.",
                        ),
                        self.link_preview_mode_seg(),
                    ))
                    .child(card_divider())
                    .child(setting_row(
                        "Pause chat on hover",
                        Some(
                            "Hold the chat still while the pointer is over it; it \
                             catches up to the newest message when you move away.",
                        ),
                        Switch::new("pause-chat-on-hover")
                            .small()
                            .checked(self.settings.pause_chat_on_hover)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.set_pause_chat_on_hover(*checked, cx);
                            }))
                            .into_any_element(),
                    ))
                    .child(card_divider())
                    .child(setting_row(
                        "Compact chat",
                        Some(
                            "Tighten the vertical space between messages so more \
                             lines fit on screen.",
                        ),
                        Switch::new("compact-chat")
                            .small()
                            .checked(self.settings.compact_chat)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.set_compact_chat(*checked, cx);
                            }))
                            .into_any_element(),
                    ))
                    .child(card_divider())
                    .child(setting_row(
                        "Flash tab when a channel goes live",
                        Some(
                            "Briefly pulse a tab's chip when one of its Twitch, \
                             Kick, or YouTube channels starts streaming.",
                        ),
                        Switch::new("flash-tab-on-live")
                            .small()
                            .checked(self.settings.flash_tab_on_live)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.set_flash_tab_on_live(*checked, cx);
                            }))
                            .into_any_element(),
                    ))
            )
            .child(div().h_1())
            .child(section_title("Timestamps"))
            .child(
                setting_card()
                    .child(setting_row(
                        "Chat",
                        Some("Show the time before each message in the chat log."),
                        Switch::new("show-timestamps-chat")
                            .small()
                            .checked(self.settings.show_timestamps_chat)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.set_show_timestamps(TimestampSurface::Chat, *checked, cx);
                            }))
                            .into_any_element(),
                    ))
                    .child(card_divider())
                    .child(setting_row(
                        "Events panel",
                        Some("Show the time on each row of the events panel."),
                        Switch::new("show-timestamps-events")
                            .small()
                            .checked(self.settings.show_timestamps_events)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.set_show_timestamps(TimestampSurface::Events, *checked, cx);
                            }))
                            .into_any_element(),
                    ))
                    .child(card_divider())
                    .child(setting_row(
                        "Mentions panel",
                        Some("Show the time on each row of the mentions panel."),
                        Switch::new("show-timestamps-mentions")
                            .small()
                            .checked(self.settings.show_timestamps_mentions)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.set_show_timestamps(TimestampSurface::Mentions, *checked, cx);
                            }))
                            .into_any_element(),
                    )),
            )
            .child(div().h_1())
            .child(section_title("Pinned messages"))
            .child(
                setting_card()
                    .child(self.pinned_platform_row(
                        bks_core::Platform::Twitch,
                        self.settings.show_pinned_twitch,
                        cx,
                    ))
                    .child(card_divider())
                    .child(self.pinned_platform_row(
                        bks_core::Platform::Kick,
                        self.settings.show_pinned_kick,
                        cx,
                    )),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from(
                        "The banner above chat while a moderator has a message pinned; \
                         its ✕ hides just the current pin.",
                    )),
            )
            .into_any_element()
    }

    /// One platform's row of the pinned-messages card: logo + platform name +
    /// a show/hide switch (the process-wide show-pinned flag for it).
    fn pinned_platform_row(
        &self,
        platform: bks_core::Platform,
        checked: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        use gpui_component::switch::Switch;
        h_flex()
            .w_full()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .child(
                div()
                    .flex_none()
                    .w(px(22.))
                    .flex()
                    .justify_center()
                    .child(platform_icon(platform, 18.)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(13.))
                    .font_weight(FontWeight::MEDIUM)
                    .child(SharedString::from(platform.label())),
            )
            .child(
                Switch::new(SharedString::from(format!(
                    "show-pinned-{}",
                    platform.label()
                )))
                .small()
                .checked(checked)
                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                    this.set_show_pinned(platform, *checked, cx);
                })),
            )
            .into_any_element()
    }

    /// Renders the Streamer Mode section: an Off / On / Auto segmented toggle
    /// (Auto = on while OBS & co. run), a description, and the live status.
    /// The Mod Buttons settings category: the strip's visibility mode and the
    /// custom-button editor (name / icon / command template / platform).
    fn mod_buttons_section(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mode_seg = setting_dropdown(&self.settings_mod_mode, settings::ModButtonMode::LABELS);

        // A small glyph action on a button row (▲ ▼ ✎ ✕); disabled ones render
        // muted and inert (the first row's ▲, the last row's ▼).
        fn row_action(
            id: String,
            glyph: &'static str,
            enabled: bool,
            on_click: impl Fn(&mut BackseaterApp, &mut Window, &mut Context<BackseaterApp>)
                + 'static,
            cx: &mut Context<BackseaterApp>,
        ) -> gpui::AnyElement {
            let base = div()
                .id(SharedString::from(id))
                .px_1()
                .rounded_sm()
                .child(SharedString::from(glyph));
            if !enabled {
                return base.opacity(0.3).into_any_element();
            }
            base.cursor_pointer()
                .text_color(cx.theme().muted_foreground)
                .hover(|s| s.bg(cx.theme().secondary))
                .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)))
                .into_any_element()
        }

        // Every button (the seeded stock ones included) as one editable row:
        // icon · name · command · platform, with reorder/edit/remove actions.
        // The row an open edit came from stays put, tinted, until Save/Cancel.
        let editing = self.editing_mod_button;
        let count = self.settings.mod_buttons.len();
        let mut button_rows: Vec<gpui::AnyElement> = Vec::new();
        for (ix, b) in self.settings.mod_buttons.iter().enumerate() {
            if ix > 0 {
                button_rows.push(card_divider().into_any_element());
            }
            let icon = match assets::mod_icon_path(&b.icon) {
                Some(path) => gpui::svg()
                    .path(path)
                    .size(px(14.))
                    .flex_none()
                    .text_color(cx.theme().foreground)
                    .into_any_element(),
                None => div()
                    .text_xs()
                    .flex_none()
                    .child(SharedString::from(b.icon.clone()))
                    .into_any_element(),
            };
            let platform = match b.platform {
                Some(p) => p.label(),
                None => "Both",
            };
            button_rows.push(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1p5()
                    .when(editing == Some(ix), |r| r.bg(cx.theme().secondary))
                    .child(div().flex_none().w(px(18.)).flex().justify_center().child(icon))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(13.))
                            .child(SharedString::from(b.name.clone())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(SharedString::from(format!("{} · {platform}", b.command))),
                    )
                    .child(row_action(
                        format!("mod-btn-up-{ix}"),
                        "▲",
                        ix > 0,
                        move |this, _, cx| this.move_mod_button(ix, ix.wrapping_sub(1), cx),
                        cx,
                    ))
                    .child(row_action(
                        format!("mod-btn-down-{ix}"),
                        "▼",
                        ix + 1 < count,
                        move |this, _, cx| this.move_mod_button(ix, ix + 1, cx),
                        cx,
                    ))
                    .child(row_action(
                        format!("mod-btn-edit-{ix}"),
                        "✎",
                        true,
                        move |this, window, cx| this.edit_mod_button(ix, window, cx),
                        cx,
                    ))
                    .child(row_action(
                        format!("mod-btn-rm-{ix}"),
                        "✕",
                        true,
                        move |this, _, cx| this.remove_mod_button(ix, cx),
                        cx,
                    ))
                    .into_any_element(),
            );
        }

        // The curated icon set as clickable presets that fill the icon field.
        let icon_value = self.settings_inputs.mod_icon.read(cx).value().to_string();
        let icon_presets: Vec<gpui::AnyElement> = assets::MOD_ICONS
            .iter()
            .map(|(name, path)| {
                let name: &'static str = name;
                let selected = icon_value == *name;
                div()
                    .id(SharedString::from(format!("mod-icon-{name}")))
                    .p_1p5()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |s| s.bg(cx.theme().secondary))
                    .hover(|s| s.bg(cx.theme().secondary))
                    .child(
                        gpui::svg()
                            .path(*path)
                            .size(px(16.))
                            .flex_none()
                            .text_color(cx.theme().foreground),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.settings_inputs
                            .mod_icon
                            .update(cx, |s, cx| s.set_value(name, window, cx));
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();

        // The button's platform scope (Both / Twitch / Kick) — a single choice,
        // so the pill segmented control.
        const PLATFORM_CHOICES: [Option<bks_core::Platform>; 3] = [
            None,
            Some(bks_core::Platform::Twitch),
            Some(bks_core::Platform::Kick),
        ];
        let platform_seg = segmented(
            "mod-platform-seg",
            ["Both", "Twitch", "Kick"],
            PLATFORM_CHOICES
                .iter()
                .position(|c| *c == self.mod_button_platform)
                .unwrap_or(0),
            cx.listener(move |this, ix: &usize, _, cx| {
                this.mod_button_platform = PLATFORM_CHOICES[*ix];
                cx.notify();
            }),
            cx,
        );

        v_flex()
            .gap_2()
            .child(section_title("Mod Buttons"))
            .child(
                setting_card().child(setting_row(
                    "Show mod buttons",
                    Some(
                        "Moderation buttons at the left of each message in channels \
                         you moderate. \"On hover\" shows them only while the mouse \
                         is over a message.",
                    ),
                    mode_seg,
                )),
            )
            .child(div().h_1())
            .child(section_title("Buttons"))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from(
                        "In strip order - reorder, edit, or remove any of them (the stock \
                         three included). A button runs any slash command or chat text on \
                         the message's platform, targeting it automatically - \
                         \"/timeout 1h spam\" times out the author, \"/delete\" deletes \
                         the message. For custom placement or plain text, {user} is the \
                         author's name and {msg-id} the message id, e.g. \"!so {user}\".",
                    )),
            )
            .child(if button_rows.is_empty() {
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from(
                        "No buttons - add one below, or Reset to defaults.",
                    ))
                    .into_any_element()
            } else {
                setting_card().children(button_rows).into_any_element()
            })
            .child(field("Name", &self.settings_inputs.mod_name))
            .child(field("Command", &self.settings_inputs.mod_command))
            .child(field("Icon", &self.settings_inputs.mod_icon))
            .child(h_flex().flex_wrap().gap_1().children(icon_presets))
            .child(
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .gap_2()
                    .items_center()
                    .child(platform_seg)
                    .child(div().flex_1())
                    .child(
                        Button::new("reset-mod-buttons")
                            .label("Reset to defaults")
                            .small()
                            .outline()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.reset_mod_buttons(cx);
                            })),
                    )
                    .when(editing.is_some(), |row| {
                        row.child(
                            Button::new("cancel-mod-edit")
                                .label("Cancel")
                                .small()
                                .outline()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.cancel_mod_button_edit(window, cx);
                                })),
                        )
                    })
                    .child(
                        Button::new("add-mod-button")
                            .label(if editing.is_some() { "Save" } else { "Add" })
                            .primary()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.add_mod_button(window, cx);
                            })),
                    ),
            )
            .into_any_element()
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
        self.resync_setting_selects(cx);
    }

    /// Adds a custom mod button from the editor fields. Only the command is
    /// required; an empty name falls back to the command, an empty icon to the
    /// gavel.
    fn add_mod_button(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.settings_inputs.mod_name.read(cx).value().trim().to_string();
        let icon = self.settings_inputs.mod_icon.read(cx).value().trim().to_string();
        let command = self
            .settings_inputs
            .mod_command
            .read(cx)
            .value()
            .trim()
            .to_string();
        if command.is_empty() {
            return;
        }
        let button = settings::ModButton {
            name: if name.is_empty() { command.clone() } else { name },
            icon: if icon.is_empty() { "gavel".into() } else { icon },
            command,
            platform: self.mod_button_platform,
        };
        // Saving an open edit replaces the button in its original slot; a
        // plain Add appends.
        match self.editing_mod_button.take() {
            Some(ix) if ix < self.settings.mod_buttons.len() => {
                self.settings.mod_buttons[ix] = button;
            }
            _ => self.settings.mod_buttons.push(button),
        }
        self.clear_mod_button_editor(window, cx);
        self.save_mod_buttons(cx);
    }

    /// Removes the mod button at `ix` (a row's ✕).
    fn remove_mod_button(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.settings.mod_buttons.len() {
            return;
        }
        self.settings.mod_buttons.remove(ix);
        // The edited row's index may have shifted (or vanished) — drop the
        // edit marker; the form keeps its text, Save would append.
        self.editing_mod_button = None;
        self.save_mod_buttons(cx);
    }

    /// Swaps the mod button at `ix` with the one at `other` (a row's ▲/▼).
    fn move_mod_button(&mut self, ix: usize, other: usize, cx: &mut Context<Self>) {
        let len = self.settings.mod_buttons.len();
        if ix >= len || other >= len || ix == other {
            return;
        }
        self.settings.mod_buttons.swap(ix, other);
        // Keep an open edit pointed at the row it was started on.
        self.editing_mod_button = match self.editing_mod_button {
            Some(e) if e == ix => Some(other),
            Some(e) if e == other => Some(ix),
            keep => keep,
        };
        self.save_mod_buttons(cx);
    }

    /// Loads the mod button at `ix` into the editor fields, leaving its row in
    /// place (highlighted) — Save replaces it in its slot, Cancel (or closing
    /// the window) changes nothing.
    fn edit_mod_button(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(b) = self.settings.mod_buttons.get(ix).cloned() else {
            return;
        };
        self.settings_inputs
            .mod_name
            .update(cx, |s, cx| s.set_value(b.name, window, cx));
        self.settings_inputs
            .mod_icon
            .update(cx, |s, cx| s.set_value(b.icon, window, cx));
        self.settings_inputs
            .mod_command
            .update(cx, |s, cx| s.set_value(b.command, window, cx));
        self.mod_button_platform = b.platform;
        self.editing_mod_button = Some(ix);
        cx.notify();
    }

    /// Cancels an open mod-button edit: clears the marker and empties the form.
    fn cancel_mod_button_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing_mod_button = None;
        self.clear_mod_button_editor(window, cx);
        cx.notify();
    }

    /// Empties the editor fields and resets the platform choice.
    fn clear_mod_button_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for input in [
            &self.settings_inputs.mod_name,
            &self.settings_inputs.mod_icon,
            &self.settings_inputs.mod_command,
        ] {
            input.update(cx, |s, cx| s.set_value("", window, cx));
        }
        self.mod_button_platform = None;
    }

    /// Replaces the button list with the stock three (the editor's "Reset to
    /// defaults").
    fn reset_mod_buttons(&mut self, cx: &mut Context<Self>) {
        self.settings.mod_buttons = settings::default_mod_buttons();
        self.settings.mod_buttons_seeded = true;
        self.editing_mod_button = None;
        self.save_mod_buttons(cx);
    }

    /// The shared tail of every mod-button edit: persist, push the process-wide
    /// state the rows render against, and re-measure every log (the strip
    /// changes row widths, so wrapped heights change too).
    fn save_mod_buttons(&mut self, cx: &mut Context<Self>) {
        self.settings.save();
        self.settings.apply_mod_buttons();
        self.remeasure_tabs(cx);
        cx.notify();
    }

    /// Re-measures every tab's log — for process-wide changes that alter row
    /// layout outside the rows' own data (the mod-button strip).
    fn remeasure_tabs(&self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            tab.view.update(cx, |view, cx| view.remeasure(cx));
        }
    }

    fn streamer_section(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        use settings::StreamerModeChoice;
        let current = self.settings.streamer_mode;
        let streamer_seg =
            setting_dropdown(&self.settings_streamer, settings::StreamerModeChoice::LABELS);

        let is_active = streamer_mode::is_active();
        let active = match (is_active, current) {
            (true, StreamerModeChoice::Auto) => "Active (auto)",
            (true, _) => "Active (manual — closing OBS won't turn it off)",
            (false, _) => "Inactive",
        };
        let status = format!(
            "{active} · Streaming software {}",
            if self.obs_running {
                "detected"
            } else {
                "not detected"
            }
        );

        v_flex()
            .gap_2()
            .child(section_title("Streamer Mode"))
            .child(
                setting_card()
                    .child(setting_row(
                        "Streamer mode",
                        Some(
                            "Hides things you might not want on stream — usercard avatars \
                             are blanked until clicked. Auto follows streaming software \
                             (OBS, Streamlabs, XSplit, Twitch Studio, vMix, PRISM).",
                        ),
                        streamer_seg,
                    ))
                    .child(card_divider())
                    .child(setting_row(
                        "Mute mention sounds while active",
                        Some("Mention pings stay silent so they don't leak into the stream."),
                        {
                            use gpui_component::switch::Switch;
                            Switch::new("streamer-mute-sounds")
                                .small()
                                .checked(self.settings.streamer_mute_sounds)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    this.set_streamer_mute_sounds(*checked, cx);
                                }))
                                .into_any_element()
                        },
                    ))
                    .child(card_divider())
                    .child(setting_row(
                        "Hide link preview thumbnails while active",
                        Some(
                            "Link previews still show the title and channel, but the \
                             thumbnail image is hidden so it can't reveal what a posted \
                             link points at on stream.",
                        ),
                        {
                            use gpui_component::switch::Switch;
                            Switch::new("streamer-hide-thumbnails")
                                .small()
                                .checked(self.settings.streamer_hide_thumbnails)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    this.set_streamer_hide_thumbnails(*checked, cx);
                                }))
                                .into_any_element()
                        },
                    ))
                    .child(card_divider())
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .py_2()
                            .child(
                                div()
                                    .flex_none()
                                    .size(px(7.))
                                    .rounded_full()
                                    .bg(gpui::rgb(if is_active {
                                        render::live_text()
                                    } else {
                                        render::offline_text()
                                    })),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(SharedString::from(status)),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// The About settings category: the running version, the update channel,
    /// project links, and the install location. The version lives here (not
    /// chat/window chrome).
    fn about_section(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        use gpui_component::switch::Switch;
        use gpui_component::Icon;
        let link = |id: &'static str, label: &'static str, url: String, cx: &Context<Self>| {
            h_flex()
                .id(id)
                .w_full()
                .items_center()
                .gap_4()
                .px_3()
                .py_2()
                .cursor_pointer()
                .hover(|s| s.bg(render::chrome_hover()))
                .child(
                    div()
                        .flex_1()
                        .text_size(px(13.))
                        .child(SharedString::from(label)),
                )
                .child(
                    Icon::new(IconName::ExternalLink)
                        .size(px(14.))
                        .text_color(cx.theme().muted_foreground),
                )
                .on_click(move |_, _, cx| cx.open_url(&url))
        };
        v_flex()
            .gap_2()
            .child(section_title("Updates"))
            .child(
                setting_card()
                    .child(setting_row(
                        "Version",
                        None,
                        div()
                            .text_size(px(13.))
                            .text_color(cx.theme().muted_foreground)
                            .child(SharedString::from(updater::version_label()))
                            .into_any_element(),
                    ))
                    .child(card_divider())
                    .child(setting_row(
                        "Get beta updates",
                        Some(
                            "Also install pre-release (beta) builds. A beta moves to the \
                             next stable release automatically.",
                        ),
                        Switch::new("beta-updates")
                            .small()
                            .checked(self.settings.beta_updates)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.set_beta_updates(*checked, cx);
                            }))
                            .into_any_element(),
                    )),
            )
            .child(div().h_1())
            .child(section_title("Links"))
            .child(
                setting_card()
                    .child(link(
                        "about-github",
                        "Backseater on GitHub",
                        updater::repo_url().to_string(),
                        cx,
                    ))
                    .child(card_divider())
                    .child(link(
                        "about-releases",
                        "Release notes",
                        format!("{}/releases", updater::repo_url()),
                        cx,
                    ))
                    .child(card_divider())
                    .child(setting_row(
                        "Install folder",
                        None,
                        Button::new("about-open-install")
                            .label("Open")
                            .small()
                            .outline()
                            .on_click(|_, _, cx| {
                                if let Ok(exe) = std::env::current_exe() {
                                    cx.reveal_path(&exe);
                                }
                            })
                            .into_any_element(),
                    )),
            )
            .into_any_element()
    }

    /// Toggles the mention-ping master switch (Highlights settings).
    fn set_mention_sound(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.mention_sound = on;
        self.settings.save();
        self.settings.apply_sound_flags();
        cx.notify();
    }

    /// Toggles whether active streamer mode silences mention pings.
    fn set_streamer_mute_sounds(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.streamer_mute_sounds = on;
        self.settings.save();
        self.settings.apply_sound_flags();
        cx.notify();
    }

    fn set_streamer_hide_thumbnails(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.streamer_hide_thumbnails = on;
        self.settings.save();
        self.settings.apply_visibility_flags();
        // The inline card is a fixed height with or without the thumbnail, so a
        // repaint (not a re-measure) suffices to add/drop the image.
        for tab in &self.tabs {
            tab.view.update(cx, |view, cx| view.refresh_log(cx));
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
        self.settings.save();
        self.refresh_mentions(cx);
        cx.notify();
    }

    /// The "Mentions tab" toggle in Highlights: shows/hides the pinned global
    /// Mentions pseudo-tab at the front of the tab strip.
    fn mentions_tab_section(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        use gpui_component::switch::Switch;
        setting_card()
            .child(setting_row(
                "Mentions tab",
                Some(
                    "A pinned tab collecting every tab's mentions in one feed; \
                     click a mention to jump to its tab.",
                ),
                Switch::new("show-mentions-tab")
                    .small()
                    .checked(self.settings.mentions_tab)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.settings.mentions_tab = *checked;
                        if !*checked {
                            this.mentions_tab_selected = false;
                        }
                        this.settings.save();
                        cx.notify();
                    }))
                    .into_any_element(),
            ))
            .child(card_divider())
            .child(setting_row(
                "Tab name",
                Some("What the Mentions tab is called; leave empty for the default."),
                div()
                    .w(px(180.))
                    .child(Input::new(&self.settings_inputs.mentions_tab_name))
                    .into_any_element(),
            ))
            .into_any_element()
    }

    /// Renders the Mentions section: the custom terms (removable chips) and an
    /// input + Add button. Your logged-in account names always highlight too and
    /// aren't listed here.
    /// Renders one Highlights term list (Mentions or Ignore): the current terms
    /// as removable chips plus an input + Add button.
    /// A mention chip's bell/bell-off sound toggle (only mention terms get one;
    /// ignore terms have no sound). Muting is app-wide by normalized term.
    /// Vector icons, not 🔔/🔕 emoji — small emoji bells render ambiguously
    /// (the plain bell read as "crossed"), so the two states looked alike.
    fn term_bell(&self, id_stem: &str, term: &str, cx: &mut Context<Self>) -> gpui::AnyElement {
        let muted = self
            .settings
            .muted_mentions
            .contains(&bks_core::normalize_term(term));
        let toggle = term.to_string();
        div()
            .id(SharedString::from(format!("bell-{id_stem}-{term}")))
            .px_1()
            .py_0p5()
            .rounded_md()
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().muted))
            .when(muted, |s| s.opacity(0.55))
            .child(
                gpui::svg()
                    .path(if muted {
                        "icons/bell-off.svg"
                    } else {
                        "icons/bell.svg"
                    })
                    .size(px(14.))
                    .flex_none()
                    .text_color(cx.theme().muted_foreground),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_mention_mute(&toggle, cx);
            }))
            .into_any_element()
    }

    fn term_list_section(&self, list: TermList, cx: &mut Context<Self>) -> gpui::AnyElement {
        let is_mentions = list.kind == TermKind::Mentions;
        // The global Mentions list also shows the logged-in account names as
        // fixed chips (they always highlight — no ✕), so their sound is
        // muteable like any custom term's.
        let mut chips: Vec<gpui::AnyElement> = Vec::new();
        if is_mentions && list.scope == TermScope::Global {
            let state = self.session.login_state();
            // One chip per distinct name — the same handle on Twitch and Kick
            // is one mention term (matching + muting are by normalized term).
            let mut seen: Vec<String> = Vec::new();
            for name in state.twitch.into_iter().chain(state.kick) {
                let norm = bks_core::normalize_term(&name);
                if seen.contains(&norm) {
                    continue;
                }
                seen.push(norm);
                chips.push(
                    term_chip(cx)
                        .child(SharedString::from(name.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(SharedString::from("(you)")),
                        )
                        .child(self.term_bell(&list.id_stem(), &name, cx))
                        .into_any_element(),
                );
            }
        }
        chips.extend(self.terms(list).clone().into_iter().map(|term| {
            let remove = term.clone();
            // A `user:` entry renders as a labeled user chip — a mono user
            // glyph, the name, and the scope as the platform's logo (mono
            // globe = all platforms) — instead of the raw grammar string;
            // anything else shows verbatim.
            let body: gpui::AnyElement = match bks_core::parse_user_entry(&term) {
                Some((platform, name)) => {
                    let scope: gpui::AnyElement = match platform {
                        Some(p) => match p.icon_url() {
                            Some(url) => {
                                let (w, h) = p.icon_size(12.0);
                                img(url).w(px(w)).h(px(h)).flex_none().into_any_element()
                            }
                            None => div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(SharedString::from(p.label()))
                                .into_any_element(),
                        },
                        None => gpui::svg()
                            .path("icons/globe.svg")
                            .size(px(12.))
                            .flex_none()
                            .text_color(cx.theme().muted_foreground)
                            .into_any_element(),
                    };
                    h_flex()
                        .items_center()
                        .gap_1()
                        .child(
                            gpui::svg()
                                .path("icons/user.svg")
                                .size(px(12.))
                                .flex_none()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .child(SharedString::from(name.to_string()))
                        .child(scope)
                        .into_any_element()
                }
                None => SharedString::from(term.clone()).into_any_element(),
            };
            term_chip(cx)
                .child(body)
                .when(is_mentions, |chip| {
                    chip.child(self.term_bell(&list.id_stem(), &term, cx))
                })
                .child(chip_remove(
                    SharedString::from(format!("rm-{}-{remove}", list.id_stem())),
                    cx.listener(move |this, _, _, cx| this.remove_term(list, &remove, cx)),
                    cx,
                ))
                .into_any_element()
        }));

        v_flex()
            .gap_2()
            .child(section_title(list.title()))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from(list.description())),
            )
            .when(is_mentions && list.scope == TermScope::Global, |s| {
                use gpui_component::switch::Switch;
                s.child(setting_card().child(setting_row(
                    "Play a sound on mention",
                    Some(
                        "A term's bell button mutes just that term; streamer mode \
                         mutes all sounds unless changed in Streamer Mode settings.",
                    ),
                    Switch::new("mention-sound")
                        .small()
                        .checked(self.settings.mention_sound)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.set_mention_sound(*checked, cx);
                        }))
                        .into_any_element(),
                )))
            })
            .when(!chips.is_empty(), |s| {
                s.child(h_flex().flex_wrap().gap_2().children(chips))
            })
            .when(!is_mentions, |s| s.child(self.term_add_mode_row(list, cx)))
            .child(
                // `flex_wrap` + a minimum input width: when the panel is narrow
                // the Add button wraps below instead of squeezing the input away.
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(160.))
                            .child(Input::new(self.term_input(list))),
                    )
                    .child(
                        Button::new(SharedString::from(format!("add-{}", list.id_stem())))
                            .label("Add")
                            .primary()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.add_term(list, window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    /// The add-entry mode selector shown above an ignore/suppress editor's
    /// input: Text / Regex / User segments, plus a platform scope row while
    /// User is picked. What's selected here decides how [`add_term`](
    /// Self::add_term) composes the typed value into a list entry (typing the
    /// raw `re:`/`user:` grammar in Text mode still works). The platform row is
    /// **multi-select** — no platform picked means all platforms; picking two
    /// (say Twitch + Kick) adds one `user:` entry per platform on Add.
    fn term_add_mode_row(&self, list: TermList, cx: &mut Context<Self>) -> gpui::AnyElement {
        let key = list.mode_key();
        let (add_kind, add_platforms) = self.term_add_mode(list);

        // Kind is a single choice → the kit segmented control.
        const KINDS: [TermEntryKind; 3] =
            [TermEntryKind::Text, TermEntryKind::Regex, TermEntryKind::User];
        let kind_seg = segmented(
            SharedString::from(format!("term-kind-{key}")),
            ["Text", "Regex", "User"],
            KINDS.iter().position(|k| *k == add_kind).unwrap_or(0),
            cx.listener(move |this, ix: &usize, window, cx| {
                this.term_add_modes.entry(key).or_default().0 = KINDS[*ix];
                this.sync_term_placeholder(list, window, cx);
                cx.notify();
            }),
            cx,
        );

        // Platforms are multi-select (empty = all), so they stay individual
        // toggle chips — a segmented control would imply a single choice.
        let plat_chip = |selected: bool, id: String, label: &'static str, cx: &mut Context<Self>| {
            let base = div()
                .id(SharedString::from(id))
                .flex_none()
                .px_2p5()
                .py_0p5()
                .rounded_full()
                .border_1()
                .cursor_pointer()
                .text_xs()
                .child(SharedString::from(label));
            if selected {
                base.bg(cx.theme().primary)
                    .border_color(cx.theme().primary)
                    .text_color(cx.theme().primary_foreground)
                    .font_weight(FontWeight::MEDIUM)
            } else {
                base.border_color(cx.theme().border)
                    .text_color(cx.theme().muted_foreground)
                    .hover(|s| s.bg(cx.theme().secondary_hover).text_color(cx.theme().foreground))
            }
        };
        let all_chip = plat_chip(
            add_platforms.is_empty(),
            format!("term-plat-{key}-all"),
            "All platforms",
            cx,
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            this.term_add_modes.entry(key).or_default().1.clear();
            cx.notify();
        }));
        let one_chip = |platform: bks_core::Platform, label: &'static str, cx: &mut Context<Self>| {
            plat_chip(
                add_platforms.contains(&platform),
                format!("term-plat-{key}-{label}"),
                label,
                cx,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                let picked = &mut this.term_add_modes.entry(key).or_default().1;
                if let Some(pos) = picked.iter().position(|p| *p == platform) {
                    picked.remove(pos);
                } else {
                    picked.push(platform);
                }
                cx.notify();
            }))
        };

        h_flex()
            .w_full()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(kind_seg)
            .when(add_kind == TermEntryKind::User, |row| {
                row.child(
                    h_flex()
                        .flex_wrap()
                        .items_center()
                        .gap_1()
                        .child(all_chip)
                        .child(one_chip(bks_core::Platform::Twitch, "Twitch", cx))
                        .child(one_chip(bks_core::Platform::Kick, "Kick", cx))
                        .child(one_chip(bks_core::Platform::YouTube, "YouTube", cx)),
                )
            })
            .into_any_element()
    }

    /// The add-entry mode of one term editor (see [`term_add_mode_row`](
    /// Self::term_add_mode_row)); plain Text + all platforms until changed.
    fn term_add_mode(&self, list: TermList) -> (TermEntryKind, Vec<bks_core::Platform>) {
        self.term_add_modes
            .get(list.mode_key())
            .cloned()
            .unwrap_or_default()
    }

    /// Points the editor's input placeholder at its current add mode — called
    /// on every mode-segment click and when the settings inputs are rebuilt
    /// (the fresh input starts with the Text placeholder, but the mode
    /// persists for the session).
    fn sync_term_placeholder(&self, list: TermList, window: &mut Window, cx: &mut Context<Self>) {
        let (kind, _) = self.term_add_mode(list);
        self.term_input(list)
            .clone()
            .update(cx, |s, cx| s.set_placeholder(term_placeholder(kind), window, cx));
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
            (TermScope::Tab(ix), TermKind::Suppress) => {
                &mut self.tabs[ix].config.suppressed_terms
            }
        }
    }

    fn term_input(&self, list: TermList) -> &Entity<InputState> {
        match (list.scope, list.kind) {
            (TermScope::Global, TermKind::Mentions) => &self.settings_inputs.mention,
            (TermScope::Global, TermKind::Ignore) => &self.settings_inputs.ignore,
            (TermScope::Global, TermKind::Suppress) => &self.settings_inputs.suppress,
            (TermScope::Tab(_), TermKind::Mentions) => &self.settings_inputs.tab_mention,
            (TermScope::Tab(_), TermKind::Ignore) => &self.settings_inputs.tab_ignore,
            (TermScope::Tab(_), TermKind::Suppress) => &self.settings_inputs.tab_suppress,
        }
    }

    /// Persists after an edit (global → settings.json, per-tab → tabs.json) and
    /// re-pushes the affected matcher/filter to the relevant view(s).
    fn refresh_terms(&mut self, list: TermList, cx: &mut Context<Self>) {
        match list.scope {
            TermScope::Global => self.settings.save(),
            TermScope::Tab(_) => self.persist(),
        }
        match list.kind {
            TermKind::Mentions => self.refresh_mentions(cx),
            TermKind::Ignore => self.refresh_ignore(cx),
            TermKind::Suppress => self.refresh_suppress(cx),
        }
    }

    /// Adds the term currently in the list's input (if new), clears the input,
    /// persists, and refreshes matching. Mention terms drop a leading `@`;
    /// ignore/suppress terms are composed per the editor's add-entry mode
    /// (Text = verbatim, so a typed `re:`/`user:` prefix is kept; Regex/User
    /// wrap the value in the grammar). In User mode a multi-platform selection
    /// adds one `user:` entry per picked platform. All de-duplicate
    /// case-insensitively; the input is cleared only if something was added.
    fn add_term(&mut self, list: TermList, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.term_input(list).clone();
        let mut term = input.read(cx).value().trim().to_string();
        if list.kind == TermKind::Mentions {
            term = term.trim_start_matches('@').to_string();
        }
        if term.is_empty() {
            return;
        }
        // One or (User mode, multiple platforms picked) more entries to add.
        let entries: Vec<String> = if list.kind == TermKind::Mentions {
            vec![term]
        } else {
            let (kind, platforms) = self.term_add_mode(list);
            match kind {
                TermEntryKind::Text => vec![term],
                TermEntryKind::Regex if term.starts_with("re:") => vec![term],
                TermEntryKind::Regex => vec![format!("re:{term}")],
                // Empty selection = all platforms (one unscoped entry).
                TermEntryKind::User if platforms.is_empty() => {
                    vec![bks_core::user_entry(None, &term)]
                }
                TermEntryKind::User => platforms
                    .iter()
                    .map(|p| bks_core::user_entry(Some(*p), &term))
                    .collect(),
            }
        };
        let mut added = false;
        for entry in entries {
            // An unscoped `user:name` makes any `user:<platform>/name` in the
            // same list redundant — drop those so "ignore everywhere" collapses
            // to one chip instead of piling up beside the specific ones.
            if let Some((None, name)) = bks_core::parse_user_entry(&entry) {
                let name = name.to_string();
                if bks_core::absorb_scoped_user_entries(self.terms_mut(list), &name) {
                    added = true;
                }
            }
            if !self
                .terms(list)
                .iter()
                .any(|t| t.eq_ignore_ascii_case(&entry))
            {
                self.terms_mut(list).push(entry);
                added = true;
            }
        }
        if !added {
            return;
        }
        input.update(cx, |s, cx| s.set_value("", window, cx));
        self.refresh_terms(list, cx);
        cx.notify();
    }

    /// Removes a term from the list, persists, and refreshes matching.
    fn remove_term(&mut self, list: TermList, term: &str, cx: &mut Context<Self>) {
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
        self.settings.save();
        for tab in &self.tabs {
            tab.view.update(cx, |view, cx| view.set_font_size(size, cx));
        }
        cx.notify();
    }

    /// The suppressed-message opacity control: a ±5% stepper shown under the
    /// Suppress list, mirroring the chat-font-size stepper. Only useful once at
    /// least one suppress term exists, but always shown so the value is
    /// discoverable.
    fn suppressed_opacity_section(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let pct = (self.settings.suppressed_opacity * 100.0).round() as i32;
        let stepper = h_flex()
            .items_center()
            .gap_2()
            .child(
                Button::new("suppress-opacity-down")
                    .label("–")
                    .small()
                    .outline()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.adjust_suppressed_opacity(-0.05, cx);
                    })),
            )
            .child(
                div()
                    .w(px(44.))
                    .text_center()
                    .text_size(px(13.))
                    .child(SharedString::from(format!("{pct}%"))),
            )
            .child(
                Button::new("suppress-opacity-up")
                    .label("+")
                    .small()
                    .outline()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.adjust_suppressed_opacity(0.05, cx);
                    })),
            );
        v_flex()
            .gap_2()
            .child(setting_card().child(setting_row(
                "Suppressed opacity",
                Some("How faint suppressed messages appear. Lower = easier to skip."),
                stepper.into_any_element(),
            )))
            .into_any_element()
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
        self.settings.save();
        self.settings.apply_visibility_flags();
        for tab in &self.tabs {
            tab.view.update(cx, |view, cx| view.refresh_log(cx));
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
        self.settings.save();
        apply_font(self.settings.font_family.as_deref(), cx);
        for tab in &self.tabs {
            tab.view.update(cx, |view, cx| view.remeasure(cx));
        }
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
        self.settings.save();
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
        self.settings.save();
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
        self.settings.save();
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
        self.settings.save();
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
        self.settings.save();
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
        self.settings.save();
        self.settings.apply_visibility_flags();
        for tab in &self.tabs {
            tab.view.update(cx, |_, cx| cx.notify());
        }
        cx.notify();
    }

    /// Toggles whether the chat-mode bar sits at the top of the chat panel or
    /// above the input. Persists, flips the process-wide flag, and repaints every
    /// tab (the bar lives outside the cached log, so a plain notify reaches it).
    /// The Off / Top / Bottom dropdown picking where the chat-mode bar sits.
    fn chat_modes_placement_seg(&self) -> gpui::AnyElement {
        setting_dropdown(&self.settings_chat_modes, settings::ChatModesPlacement::LABELS)
    }

    fn link_preview_mode_seg(&self) -> gpui::AnyElement {
        setting_dropdown(&self.settings_link_preview, settings::LinkPreviewMode::LABELS)
    }

    fn set_link_preview_mode(&mut self, mode: settings::LinkPreviewMode, cx: &mut Context<Self>) {
        if self.settings.link_preview_mode == mode {
            return;
        }
        let now_inline = mode == settings::LinkPreviewMode::Inline;
        self.settings.link_preview_mode = mode;
        self.settings.save();
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
        self.resync_setting_selects(cx);
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
        self.settings.save();
        self.settings.apply_visibility_flags();
        for tab in &self.tabs {
            tab.view.update(cx, |_, cx| cx.notify());
        }
        self.resync_setting_selects(cx);
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
        self.settings.save();
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
        self.settings.save();
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
                cx.notify();
            });
        }
        cx.notify();
    }

    /// Live-edits one color of the theme currently being edited. The draft is the
    /// active theme (so the change shows immediately), and if it's a saved profile
    /// the edit is persisted; otherwise it just previews until saved.
    fn set_theme_color(&mut self, field: ThemeColorField, color: u32, cx: &mut Context<Self>) {
        let Some(draft) = self.theme_draft.as_mut() else {
            return;
        };
        if field.get(draft) == color {
            return;
        }
        field.set(draft, color);
        // If this draft is a saved profile that's currently active, keep the saved
        // copy in sync so the edit sticks across restarts.
        if let Some(name) = self.settings.theme.custom_name() {
            if name == draft.name {
                let draft = draft.clone();
                if let Some(saved) = self
                    .settings
                    .custom_themes
                    .iter_mut()
                    .find(|t| t.name == draft.name)
                {
                    *saved = draft;
                    self.settings.save();
                }
            }
        }
        self.reapply_theme_colors(cx);
    }

    /// Starts editing a new custom theme: opens the editor on a fresh draft
    /// (seeded from the dark base) and rebinds the pickers to it.
    fn new_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.theme_draft = Some(default_custom_theme(true, String::new()));
        self.settings_theme_name.update(cx, |s, cx| {
            s.set_value("", window, cx);
        });
        self.rebind_theme_pickers(window, cx);
        cx.notify();
    }

    /// Opens the editor on an existing saved theme `name` (so its colors can be
    /// tweaked), seeding the name field and pickers from it.
    fn edit_theme(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(theme) = self.settings.custom_themes.iter().find(|t| t.name == name) else {
            return;
        };
        let theme = theme.clone();
        self.settings_theme_name.update(cx, |s, cx| {
            s.set_value(theme.name.clone(), window, cx);
        });
        self.theme_draft = Some(theme);
        self.rebind_theme_pickers(window, cx);
        cx.notify();
    }

    /// Closes the theme editor without saving the in-progress draft (a saved
    /// profile keeps whatever was already persisted).
    fn cancel_theme_edit(&mut self, cx: &mut Context<Self>) {
        self.theme_draft = None;
        cx.notify();
    }

    /// Rebinds just the color pickers (not the whole settings panel) to the
    /// current draft — used when the editor opens on a different theme.
    fn rebind_theme_pickers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (name, pickers, subs) =
            Self::theme_inputs(self.theme_draft.as_ref(), window, cx);
        self.settings_theme_name = name;
        self.settings_theme_pickers = pickers;
        self._settings_theme_subs = subs;
        // Re-seed the name field from the draft (theme_inputs makes it blank).
        if let Some(draft) = &self.theme_draft {
            let value = draft.name.clone();
            self.settings_theme_name.update(cx, |s, cx| {
                s.set_value(value, window, cx);
            });
        }
    }

    /// Saves the current draft as a named profile (creating or overwriting by
    /// name) and selects it as the active theme. No-op if the name is blank.
    fn save_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut draft) = self.theme_draft.clone() else {
            return;
        };
        let name = self.settings_theme_name.read(cx).value().trim().to_string();
        if name.is_empty() {
            return;
        }
        draft.name = name.clone();
        if let Some(existing) = self.settings.custom_themes.iter_mut().find(|t| t.name == name) {
            *existing = draft.clone();
        } else {
            self.settings.custom_themes.push(draft.clone());
        }
        self.theme_draft = Some(draft);
        self.settings.theme = settings::ThemeChoice::Custom(name);
        self.settings.save();
        self.reapply_theme(window, cx);
    }

    /// Deletes a saved theme profile. If it was the active theme, falls back to
    /// the dark built-in.
    fn delete_theme(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.custom_themes.retain(|t| t.name != name);
        let was_active = self.settings.theme.custom_name() == Some(name);
        if was_active {
            self.settings.theme = settings::ThemeChoice::Dark;
        }
        if self.theme_draft.as_ref().is_some_and(|d| d.name == name) {
            self.theme_draft = None;
        }
        self.settings.save();
        if was_active {
            self.reapply_theme(window, cx);
        } else {
            cx.notify();
        }
    }

    /// A poll result from the OBS watcher: updates the detection state and, in
    /// Auto mode, flips streamer mode with it.
    fn set_obs_running(&mut self, running: bool, cx: &mut Context<Self>) {
        if running == self.obs_running {
            return;
        }
        self.obs_running = running;
        self.apply_streamer_mode(cx);
    }

    /// Changes the streamer-mode setting (off / on / auto), persists it, and
    /// applies it.
    fn set_streamer_mode(&mut self, choice: settings::StreamerModeChoice, cx: &mut Context<Self>) {
        if choice == self.settings.streamer_mode {
            return;
        }
        self.settings.streamer_mode = choice;
        self.settings.save();
        self.apply_streamer_mode(cx);
        self.resync_setting_selects(cx);
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
                    // it); the usercard-avatar blanking rides the ChatView notify.
                    view.refresh_log(cx);
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
            cx.background_executor().timer(updater::CHECK_INTERVAL).await;
        })
    }

    /// Toggles the beta update channel: persists the setting, points the
    /// updater at (or away from) pre-releases, and restarts the update watch so
    /// the change takes effect now instead of at the next scheduled check.
    fn set_beta_updates(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.beta_updates = on;
        self.settings.save();
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
                let url =
                    updater::release_url(self.update_ready.as_deref().unwrap_or_default());
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
            .child(div().flex_1().min_w_0().child(SharedString::from(format!(
                "Updated to v{version}"
            ))))
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
        let rows = mentions::feed_rows(&self.mention_store, font_size, cx);
        chatview::tail_panel(&mut self.mentions_new, &self.mentions_scroll);

        let body: gpui::AnyElement = if rows.is_empty() {
            div()
                .px_3()
                .py_2()
                .text_size(px(font_size * 0.85))
                .text_color(cx.theme().muted_foreground)
                .child(SharedString::from(
                    "No mentions yet — messages that mention you, from any tab, collect here.",
                ))
                .into_any_element()
        } else {
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .child(
                    div()
                        .id("mentions-tab-list")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.mentions_scroll)
                        .text_size(px(font_size))
                        .child(v_flex().gap_1().px(px(6.0)).py_2().children(rows)),
                )
                .vertical_scrollbar(&self.mentions_scroll)
                .into_any_element()
        };
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
                                                    let view = this
                                                        .tabs
                                                        .get(ix)
                                                        .map(|t| t.view.clone());
                                                    if let Some(view) = view {
                                                        view.update(cx, |v, cx| {
                                                            v.open_search(cx)
                                                        });
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

/// Applies resolved 7TV cosmetics to an author: sets the name paint and adds the
/// 7TV badge (if any) at the front of their badge list, deduped so re-applying is
/// idempotent (a chatter's cosmetics can be applied on the lookup *and* again on a
/// later message). The 7TV badge leads so it sits beside platform badges.
pub(crate) fn apply_cosmetics_to_author(
    author: &mut bks_core::Author,
    cosmetics: &bks_emotes::Cosmetics,
) {
    if let Some(paint) = &cosmetics.paint {
        author.paint = Some(paint.clone());
    }
    if let Some(badge) = &cosmetics.badge {
        if !author.badges.iter().any(|b| b.id == badge.id) {
            author.badges.insert(0, badge.clone());
        }
    }
}

/// Packs a gpui [`Hsla`] into an opaque `0xRRGGBB` (dropping alpha — chat colors
/// are opaque), for storing a picked color in a saved theme.
fn hsla_to_packed(c: gpui::Hsla) -> u32 {
    let rgba = gpui::Rgba::from(c);
    let ch = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    (ch(rgba.r) << 16) | (ch(rgba.g) << 8) | ch(rgba.b)
}

/// Expands a packed `0xRRGGBB` to an opaque gpui [`Hsla`], to seed a color picker.
fn packed_to_hsla(color: u32) -> gpui::Hsla {
    gpui::rgb(color).into()
}

/// A fresh custom theme seeded from the dark or light base's curated colors, so
/// a "New theme" opens on a sensible starting palette.
fn default_custom_theme(dark: bool, name: String) -> settings::CustomTheme {
    let c = render::CustomColors::from_base(dark);
    settings::CustomTheme {
        name,
        base_dark: dark,
        chat_bg: c.chat_bg,
        default_name: c.default_name,
        first_message: c.first_message,
        highlighted: Some(c.highlighted),
        event: c.event,
        streak: c.streak,
        live: c.live,
        offline: c.offline,
        mention: c.mention,
        link: c.link,
        error: c.error,
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

/// A bold section heading inside the settings dialog.
/// A settings section's mini-header: small uppercase muted label (the category
/// name itself is the content pane's page title).
fn section_title(text: &str) -> impl IntoElement {
    div()
        .font_weight(FontWeight::SEMIBOLD)
        .text_size(px(11.))
        .text_color(gpui::rgb(render::offline_text()))
        .child(SharedString::from(text.to_uppercase()))
}

/// A settings card: a bordered, slightly lifted surface whose rows are divided
/// by [`card_divider`]s. The macOS-style grouped-settings look.
fn setting_card() -> gpui::Div {
    v_flex()
        .w_full()
        .rounded_lg()
        .border_1()
        .border_color(gpui::rgb(render::panel_border()))
        .bg(render::row_hover())
        .overflow_hidden()
}

/// The hairline between two rows of a [`setting_card`].
fn card_divider() -> impl IntoElement {
    div().h(px(1.)).w_full().bg(gpui::rgb(render::panel_border()))
}

/// One entry of a settings window's category rail (icon + label, selected or
/// muted). The caller attaches the click handler.
fn rail_item(
    icon: IconName,
    label: &'static str,
    is_sel: bool,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    use gpui_component::Icon;
    h_flex()
        .id(SharedString::from(format!("settings-cat-{label}")))
        .w_full()
        .items_center()
        .gap_2()
        .px_2()
        .py_1p5()
        .rounded_md()
        .cursor_pointer()
        .text_size(px(13.))
        .when(is_sel, |s| {
            s.bg(cx.theme().secondary).font_weight(FontWeight::MEDIUM)
        })
        .when(!is_sel, |s| s.text_color(cx.theme().muted_foreground))
        .hover(|s| s.bg(cx.theme().secondary))
        .child(Icon::new(icon).size(px(15.)).text_color(if is_sel {
            cx.theme().foreground
        } else {
            cx.theme().muted_foreground
        }))
        .child(SharedString::from(label))
}

/// The settings windows' shared chrome: a fixed category rail on the left, an
/// independently scrolling content pane headed by the selected category's name
/// on the right. Both the app and tab settings render through this.
fn settings_shell(
    rail: Vec<gpui::AnyElement>,
    title: &'static str,
    body: gpui::AnyElement,
    scroll_id: &'static str,
    scroll: &ScrollHandle,
    cx: &App,
) -> gpui::AnyElement {
    use gpui_component::scroll::{Scrollbar, ScrollbarAxis, ScrollbarShow};
    h_flex()
        .size_full()
        .items_stretch()
        .child(
            v_flex()
                .flex_none()
                .w(px(150.))
                .h_full()
                .gap_0p5()
                .p_2()
                .bg(cx.theme().sidebar)
                .border_r_1()
                .border_color(cx.theme().sidebar_border)
                .children(rail),
        )
        // The content pane holds the scrolling body plus an always-visible
        // scrollbar overlay (`ScrollbarShow::Always` overrides the theme's
        // fade-when-idle default) so people can see there's more settings below
        // the fold. The Scrollbar only paints a thumb when the body overflows.
        .child(
            div()
                .relative()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(
                    div()
                        .id(scroll_id)
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(scroll)
                        .px_5()
                        .py_4()
                        .child(
                            v_flex()
                                .w_full()
                                .max_w(px(520.))
                                .gap_4()
                                .child(
                                    div()
                                        .text_size(px(17.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(SharedString::from(title)),
                                )
                                .child(body),
                        ),
                )
                .child(
                    // Match gpui-component's own `ScrollbarLayer`: the Scrollbar
                    // must sit in an absolutely-positioned full-size overlay to
                    // get the bounds it paints the thumb into.
                    div().absolute().top_0().left_0().right_0().bottom_0().child(
                        Scrollbar::new(scroll)
                            .id(SharedString::from(scroll_id))
                            .axis(ScrollbarAxis::Vertical)
                            .scrollbar_show(ScrollbarShow::Always),
                    ),
                ),
        )
        .into_any_element()
}

/// One settings row: label (+ optional muted description under it) on the left,
/// the control pinned right.
/// A term/user chip shell (used for ignore / suppress / highlight / mention
/// entries): a rounded-full pill with a hairline border and a soft fill, so the
/// chips read as discrete tokens instead of flat blocks. The caller fills the
/// body (and optional bell / remove controls).
fn term_chip(cx: &App) -> gpui::Div {
    h_flex()
        .items_center()
        .gap_1p5()
        .pl_2p5()
        .pr_1p5()
        .py_0p5()
        .rounded_full()
        .bg(cx.theme().secondary)
        .border_1()
        .border_color(cx.theme().border)
        .text_sm()
}

/// The ✕ that removes a [`term_chip`]. Muted at rest, tinting red on hover so
/// its destructive action is legible without shouting on every chip.
fn chip_remove(
    id: SharedString,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(px(16.))
        .rounded_full()
        .cursor_pointer()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .hover(|s| {
            s.bg(gpui::rgb(render::highlight_error().1))
                .text_color(gpui::white())
        })
        .child(SharedString::from("✕"))
        .on_click(on_click)
}

/// Renders a small settings enum-picker dropdown from its window-bound
/// [`SettingSelect`] state (chat-modes / streamer / mod-button mode). The state
/// owns the choices + selection + change subscription (see
/// [`BackseaterApp::setting_select`]); this only draws the compact trigger.
///
/// The trigger hugs its selected label (short for "Off", wider for "Bottom"),
/// but lives in a fixed-width, right-aligned slot so its changing size never
/// reflows the row's left-hand label/description column. The opened menu is
/// given the same width so every option fits however short the current one is.
/// The width is derived from `labels` (the longest one + chrome), so it can't
/// drift out of sync with the choices the way a hand-tuned literal could.
fn setting_dropdown(state: &Entity<SettingSelect>, labels: &[&str]) -> gpui::AnyElement {
    use gpui_component::select::Select;
    let width = dropdown_width(labels);
    h_flex()
        .flex_none()
        .w(px(width))
        .justify_end()
        .child(
            div()
                .flex_none()
                .w_auto()
                .child(Select::new(state).small().menu_width(px(width))),
        )
        .into_any_element()
}

/// A slot width for a [`setting_dropdown`] wide enough for its longest label
/// plus the trigger's own chrome (padding + chevron + the menu row's check).
/// An approximation from character count — the settings labels are short ASCII,
/// so ~7px/char is comfortable without a text-system measurement.
fn dropdown_width(labels: &[&str]) -> f32 {
    let longest = labels.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    (longest as f32 * 7.0 + 48.0).max(90.0)
}

/// A pill segmented control (the term editor's inline "Text / Regex / User"
/// picker) built on the kit's [`TabBar`] Pill variant, wrapped in a bordered,
/// filled track so it reads as one control. `labels[selected]` is lit;
/// `on_click` receives the clicked index. (The fixed enum settings use
/// [`setting_dropdown`] instead; this stays for the dynamic per-list picker.)
fn segmented(
    id: impl Into<ElementId>,
    labels: impl IntoIterator<Item = &'static str>,
    selected: usize,
    on_click: impl Fn(&usize, &mut Window, &mut App) + 'static,
    cx: &App,
) -> gpui::AnyElement {
    use gpui_component::tab::TabBar;
    // The track hugs the pills tightly (no inner padding), so the border/fill is
    // barely visible outside the selected pill instead of a chunky frame.
    h_flex()
        .flex_none()
        .rounded_full()
        .bg(cx.theme().muted)
        .border_1()
        .border_color(cx.theme().border)
        .child(
            TabBar::new(id)
                .pill()
                .small()
                .selected_index(selected)
                .children(labels.into_iter().map(SharedString::from))
                .on_click(on_click),
        )
        .into_any_element()
}

fn setting_row(
    label: &str,
    desc: Option<&str>,
    control: gpui::AnyElement,
) -> gpui::AnyElement {
    h_flex()
        .w_full()
        .items_center()
        .gap_4()
        .px_3()
        .py_2()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    div()
                        .text_size(px(13.))
                        .child(SharedString::from(label.to_string())),
                )
                .children(desc.map(|d| {
                    div()
                        .text_xs()
                        .text_color(gpui::rgb(render::offline_text()))
                        .child(SharedString::from(d.to_string()))
                })),
        )
        .child(div().flex_none().child(control))
        .into_any_element()
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

/// A labelled input row for the settings dialog.
fn field(label: &str, input: &Entity<InputState>) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(
            div()
                .text_size(px(13.))
                .child(SharedString::from(label.to_string())),
        )
        .child(Input::new(input))
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
fn chip_tooltip(
    platforms: Vec<TipPlatform>,
    cx: &mut Context<BackseaterApp>,
) -> impl IntoElement {
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
        assert_eq!(chip_flash_alpha(TAB_FLASH_DURATION + Duration::from_secs(1)), 0.0);
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
        assert_eq!(format_uptime(Duration::hours(23) + Duration::minutes(59)), "23h59m");
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
