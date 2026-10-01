//! Settings window and its editing state. Inputs are created in this window
//! once; committed preferences and their live effects remain on the app.

use super::*;
use gpui::ScrollHandle;
use gpui_component::combobox::{Combobox, ComboboxEvent, ComboboxState};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::searchable_list::SearchableVec;
use gpui_component::IndexPath;

#[cfg(test)]
mod tests;

/// Default size of the settings child window — a bit bigger than the usercard;
/// the OS resizes it freely from there (the Highlights inputs wrap when narrow).
pub(super) const SETTINGS_WINDOW_SIZE: Size<Pixels> = Size {
    width: px(700.),
    height: px(660.),
};
/// Smallest the settings window can be resized to — the width floor keeps the
/// content column (after the 150px category sidebar + padding) usable.
pub(super) const SETTINGS_MIN_SIZE: Size<Pixels> = Size {
    width: px(460.),
    height: px(300.),
};

/// What the settings child window is currently showing.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Panel {
    /// App-wide settings (account, appearance, mentions).
    App,
    /// Settings for the tab at this index (name + channels).
    Tab(usize),
}

impl Panel {
    /// The window title for this panel.
    pub(super) fn title(self) -> &'static str {
        match self {
            Panel::App => "Settings",
            Panel::Tab(_) => "Tab settings",
        }
    }
}

/// The window-owned settings inputs, keyed by their placeholder.
#[derive(Clone, Copy)]
enum SettingsInput {
    Name,
    Twitch,
    Kick,
    YouTube,
    TikTok,
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
/// subscriptions, as returned by [`SettingsView::build_setting_selects`].
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

/// Text inputs created together in the settings window that owns them.
struct SettingsInputs {
    name: Entity<InputState>,
    twitch: Entity<InputState>,
    kick: Entity<InputState>,
    youtube: Entity<InputState>,
    tiktok: Entity<InputState>,
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
            tiktok: settings_input(SettingsInput::TikTok, window, cx),
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
        SettingsInput::TikTok => "TikTok @username / LIVE URL (optional)",
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
/// editor keep it pointed at the current mode.
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
/// (see [`SettingsView::settings_theme_pickers`]). The order here is the order shown.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ThemeColorField {
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
pub(super) enum TermKind {
    Mentions,
    Ignore,
    Suppress,
}

/// Whether a term list edits the app-wide (global) terms or one tab's own terms.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TermScope {
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
pub(super) struct TermList {
    pub(super) kind: TermKind,
    pub(super) scope: TermScope,
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

    pub(super) fn title(self) -> &'static str {
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

    /// The add-mode slot this editor uses (see `SettingsView::term_add_modes`):
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

type PermRowAction = Box<dyn Fn(&mut SettingsView, &mut Context<SettingsView>)>;

/// Only app settings, tab configuration, login or OBS changes dirty this view.
pub(super) struct SettingsChanged;

/// Read-only copies used to build one settings frame. All writes go to the app.
struct SettingsTab {
    id: u64,
    config: TabConfig,
}

pub(super) struct SettingsView {
    app: WeakEntity<BackseaterApp>,
    _app_subscription: Subscription,
    snapshot_dirty: bool,
    panel: Panel,
    tab_id: Option<u64>,
    settings: Settings,
    tabs: Vec<SettingsTab>,
    session: Session,
    obs_running: bool,
    settings_inputs: SettingsInputs,
    _settings_mentions_name_sub: Subscription,
    settings_font: Entity<FontCombobox>,
    _settings_font_sub: Subscription,
    settings_chat_modes: Entity<SettingSelect>,
    settings_streamer: Entity<SettingSelect>,
    settings_mod_mode: Entity<SettingSelect>,
    settings_link_preview: Entity<SettingSelect>,
    _settings_select_subs: Vec<Subscription>,
    settings_theme_name: Entity<InputState>,
    settings_theme_pickers: Vec<Entity<gpui_component::color_picker::ColorPickerState>>,
    _settings_theme_subs: Vec<Subscription>,
    theme_draft: Option<settings::CustomTheme>,
    settings_category: SettingsCategory,
    tab_settings_category: TabSettingsCategory,
    settings_scroll: ScrollHandle,
    tab_settings_scroll: ScrollHandle,
    mod_button_platform: Option<bks_core::Platform>,
    term_add_modes:
        std::collections::HashMap<&'static str, (TermEntryKind, Vec<bks_core::Platform>)>,
    editing_mod_button: Option<usize>,
    twitch_perm_open: bool,
    twitch_perm_choice: bks_auth::twitch::ScopeChoice,
}

impl SettingsView {
    pub(super) fn new(
        app: Entity<BackseaterApp>,
        panel: Panel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = app.read(cx).settings.clone();
        let session = app.read(cx).session.clone();
        let obs_running = app.read(cx).obs_running;
        let tabs: Vec<SettingsTab> = app
            .read(cx)
            .tabs
            .iter()
            .map(|tab| SettingsTab {
                id: tab.id,
                config: tab.config.clone(),
            })
            .collect();
        let inputs = SettingsInputs::build(window, cx);
        if let Some(name) = &settings.mentions_tab_name {
            inputs
                .mentions_tab_name
                .update(cx, |s, cx| s.set_value(name.clone(), window, cx));
        }
        let name_sub = Self::subscribe_mentions_name(&inputs.mentions_tab_name, cx);
        let (font, font_sub) = Self::font_combobox(settings.font_family.as_deref(), window, cx);
        let (chat_modes, streamer, mod_mode, link_preview, select_subs) =
            Self::build_setting_selects(&settings, window, cx);
        let theme_draft = settings.active_custom_theme().cloned();
        let (theme_name, theme_pickers, theme_subs) =
            Self::theme_inputs(theme_draft.as_ref(), window, cx);
        if let Some(draft) = &theme_draft {
            theme_name.update(cx, |s, cx| s.set_value(draft.name.clone(), window, cx));
        }
        let app_subscription = cx.subscribe(&app, |this, _, _: &SettingsChanged, cx| {
            this.snapshot_dirty = true;
            cx.notify();
        });
        let window_handle = window.window_handle();
        cx.observe_release(&app, move |_, _, cx| {
            let _ = window_handle.update(cx, |_, window, _| window.remove_window());
        })
        .detach();
        let tab_id = match panel {
            Panel::Tab(ix) => tabs.get(ix).map(|tab| tab.id),
            Panel::App => None,
        };
        let mut view = Self {
            app: app.downgrade(),
            _app_subscription: app_subscription,
            snapshot_dirty: false,
            panel,
            tab_id,
            settings,
            session,
            tabs,
            obs_running,
            settings_inputs: inputs,
            _settings_mentions_name_sub: name_sub,
            settings_font: font,
            _settings_font_sub: font_sub,
            settings_chat_modes: chat_modes,
            settings_streamer: streamer,
            settings_mod_mode: mod_mode,
            settings_link_preview: link_preview,
            _settings_select_subs: select_subs,
            settings_theme_name: theme_name,
            settings_theme_pickers: theme_pickers,
            _settings_theme_subs: theme_subs,
            theme_draft,
            settings_category: SettingsCategory::Account,
            tab_settings_category: TabSettingsCategory::Channels,
            settings_scroll: ScrollHandle::new(),
            tab_settings_scroll: ScrollHandle::new(),
            mod_button_platform: None,
            term_add_modes: Default::default(),
            editing_mod_button: None,
            twitch_perm_open: false,
            twitch_perm_choice: bks_auth::twitch::ScopeChoice::default(),
        };
        if let Panel::Tab(ix) = panel {
            view.prefill_tab_settings(ix, window, cx);
        }
        view
    }

    pub(super) fn set_panel(&mut self, panel: Panel, window: &mut Window, cx: &mut Context<Self>) {
        self.snapshot_dirty = true;
        self.tab_id = None;
        self.refresh_snapshot(cx);
        self.panel = panel;
        self.tab_id = match panel {
            Panel::Tab(ix) => self.tabs.get(ix).map(|tab| tab.id),
            Panel::App => None,
        };
        if let Panel::Tab(ix) = panel {
            self.tab_settings_category = TabSettingsCategory::Channels;
            self.prefill_tab_settings(ix, window, cx);
        }
        cx.notify();
    }

    pub(super) fn show_account(&mut self, cx: &mut Context<Self>) {
        self.settings_category = SettingsCategory::Account;
        cx.notify();
    }

    /// Settings UI sends committed actions to the authoritative app. No window
    /// widgets or in-progress drafts are retained by the application entity.
    fn with_app(
        &self,
        cx: &mut Context<Self>,
        action: impl FnOnce(&mut BackseaterApp, &mut Context<BackseaterApp>),
    ) {
        let _ = self.app.update(cx, action);
    }

    fn with_tab(
        &self,
        cx: &mut Context<Self>,
        action: impl FnOnce(&mut BackseaterApp, usize, &mut Context<BackseaterApp>),
    ) {
        let Some(id) = self.tab_id else {
            return;
        };
        self.with_app(cx, |app, cx| {
            if let Some(ix) = app.tabs.iter().position(|tab| tab.id == id) {
                action(app, ix, cx);
            }
        });
    }

    fn resolve_term_list(&self, mut list: TermList, app: &BackseaterApp) -> Option<TermList> {
        if let TermScope::Tab(_) = list.scope {
            let id = self.tab_id?;
            list.scope = TermScope::Tab(app.tabs.iter().position(|tab| tab.id == id)?);
        }
        Some(list)
    }

    fn remove_term(&self, list: TermList, term: &str, cx: &mut Context<Self>) {
        self.with_app(cx, |app, cx| {
            if let Some(list) = self.resolve_term_list(list, app) {
                app.remove_term(list, term, cx);
            }
        });
    }

    fn refresh_snapshot(&mut self, cx: &App) -> bool {
        if !self.snapshot_dirty {
            return true;
        }
        self.snapshot_dirty = false;
        let Some(app) = self.app.upgrade() else {
            return false;
        };
        let app = app.read(cx);
        self.settings = app.settings.clone();
        self.obs_running = app.obs_running;
        self.tabs = app
            .tabs
            .iter()
            .map(|tab| SettingsTab {
                id: tab.id,
                config: tab.config.clone(),
            })
            .collect();
        if let Some(id) = self.tab_id {
            let Some(ix) = self.tabs.iter().position(|tab| tab.id == id) else {
                return false;
            };
            self.panel = Panel::Tab(ix);
        }
        true
    }

    fn active_controller(&self, cx: &App) -> Option<Controller> {
        self.app.upgrade()?.read(cx).active_controller(cx)
    }

    fn terms(&self, list: TermList) -> &[String] {
        match (list.scope, list.kind) {
            (TermScope::Global, TermKind::Mentions) => &self.settings.custom_mentions,
            (TermScope::Global, TermKind::Ignore) => &self.settings.ignored_terms,
            (TermScope::Global, TermKind::Suppress) => &self.settings.suppressed_terms,
            (TermScope::Tab(ix), kind) => self
                .tabs
                .get(ix)
                .map(|tab| match kind {
                    TermKind::Mentions => tab.config.custom_mentions.as_slice(),
                    TermKind::Ignore => tab.config.ignored_terms.as_slice(),
                    TermKind::Suppress => tab.config.suppressed_terms.as_slice(),
                })
                .unwrap_or_default(),
        }
    }

    fn apply_settings(&self, ix: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        let mut config = tab.config.clone();
        config.name = self
            .settings_inputs
            .name
            .read(cx)
            .value()
            .trim()
            .to_string();
        config.twitch_channel = self
            .settings_inputs
            .twitch
            .read(cx)
            .value()
            .trim()
            .to_string();
        config.kick_channel = self
            .settings_inputs
            .kick
            .read(cx)
            .value()
            .trim()
            .to_string();
        config.youtube_channel = self
            .settings_inputs
            .youtube
            .read(cx)
            .value()
            .trim()
            .to_string();
        let tiktok = self
            .settings_inputs
            .tiktok
            .read(cx)
            .value()
            .trim()
            .to_string();
        config.tiktok_channel = bks_core::normalize_tiktok_channel(&tiktok).unwrap_or(tiktok);
        self.with_tab(cx, |app, ix, cx| app.apply_settings(ix, config, cx));
    }

    fn sync_selects(&self, window: &mut Window, cx: &mut Context<Self>) {
        let targets = [
            (
                &self.settings_chat_modes,
                settings::ChatModesPlacement::ALL
                    .iter()
                    .position(|v| *v == self.settings.chat_modes_placement)
                    .unwrap_or(0),
            ),
            (
                &self.settings_streamer,
                settings::StreamerModeChoice::ALL
                    .iter()
                    .position(|v| *v == self.settings.streamer_mode)
                    .unwrap_or(0),
            ),
            (
                &self.settings_mod_mode,
                settings::ModButtonMode::ALL
                    .iter()
                    .position(|v| *v == self.settings.mod_button_mode)
                    .unwrap_or(0),
            ),
            (
                &self.settings_link_preview,
                settings::LinkPreviewMode::ALL
                    .iter()
                    .position(|v| *v == self.settings.link_preview_mode)
                    .unwrap_or(0),
            ),
        ];
        for (state, want) in targets {
            if state.read(cx).selected_index(cx).map(|ix| ix.row) != Some(want) {
                state.update(cx, |s, cx| {
                    s.set_selected_index(Some(IndexPath::default().row(want)), window, cx)
                });
            }
        }
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::stale_hover::clear(window, cx);
        if !self.refresh_snapshot(cx) {
            window.remove_window();
            return div().into_any_element();
        }
        self.sync_selects(window, cx);
        match self.panel {
            Panel::App => self.app_settings_body(cx),
            Panel::Tab(ix) => self.tab_settings_body(ix, cx),
        }
    }
}

impl SettingsView {
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
                this.with_app(cx, |app, cx| app.set_font_family(family, cx));
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
    /// Builds the settings dropdowns and subscriptions in their owning window.
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
            |this, ix, cx| {
                this.with_app(cx, |app, cx| {
                    app.set_chat_modes_placement(settings::ChatModesPlacement::ALL[ix], cx)
                })
            },
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
            |this, ix, cx| {
                this.with_app(cx, |app, cx| {
                    app.set_streamer_mode(settings::StreamerModeChoice::ALL[ix], cx)
                })
            },
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
            |this, ix, cx| {
                this.with_app(cx, |app, cx| {
                    app.set_mod_button_mode(settings::ModButtonMode::ALL[ix], cx)
                })
            },
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
            |this, ix, cx| {
                this.with_app(cx, |app, cx| {
                    app.set_link_preview_mode(settings::LinkPreviewMode::ALL[ix], cx)
                })
            },
            window,
            cx,
        );

        (
            chat_modes,
            streamer,
            mod_mode,
            link_preview,
            vec![s1, s2, s3, s4],
        )
    }
    /// Builds the Themes category's window-bound inputs: the profile-name field
    /// and one [`ColorPickerState`](gpui_component::color_picker::ColorPickerState)
    /// per curated color, seeded from `draft` (or the dark base if none). Each
    /// picker's Change updates the draft's matching field live and re-applies the
    /// theme. These widgets are created inside their owning settings window.
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
            let state = cx.new(|cx| ColorPickerState::new(window, cx).default_value(start));
            subs.push(
                cx.subscribe(&state, move |this, _, ev: &ColorPickerEvent, cx| {
                    let ColorPickerEvent::Change(color) = ev;
                    if let Some(color) = color {
                        this.set_theme_color(field, hsla_to_packed(*color), cx);
                    }
                }),
            );
            pickers.push(state);
        }
        (name, pickers, subs)
    }
    /// Subscribes the Mentions-tab rename input so edits apply live (created
    /// alongside the input in its owning settings window).
    fn subscribe_mentions_name(input: &Entity<InputState>, cx: &mut Context<Self>) -> Subscription {
        cx.subscribe(input, |this, state, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                let value = state.read(cx).value().to_string();
                this.with_app(cx, |app, cx| app.set_mentions_tab_name(value, cx));
            }
        })
    }
    /// Pre-fills the tab-settings inputs from tab `ix`'s current config.
    /// `window` must be the settings window the inputs are bound to.
    fn prefill_tab_settings(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        let cfg = tab.config.clone();
        self.settings_inputs
            .name
            .update(cx, |s, cx| s.set_value(&cfg.name, window, cx));
        self.settings_inputs
            .twitch
            .update(cx, |s, cx| s.set_value(&cfg.twitch_channel, window, cx));
        self.settings_inputs
            .kick
            .update(cx, |s, cx| s.set_value(&cfg.kick_channel, window, cx));
        self.settings_inputs
            .youtube
            .update(cx, |s, cx| s.set_value(&cfg.youtube_channel, window, cx));
        self.settings_inputs
            .tiktok
            .update(cx, |s, cx| s.set_value(&cfg.tiktok_channel, window, cx));
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
            SettingsCategory::ModButtons => v_flex().gap_5().child(self.mod_buttons_section(cx)),
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
            .child(field(
                "TikTok channel (read-only)",
                &self.settings_inputs.tiktok,
            ))
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
                    this.with_tab(cx, |app, ix, cx| {
                        app.set_panel_shown(ix, tabs::PanelKind::Events, *checked, cx)
                    });
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
                        this.with_tab(cx, |app, ix, cx| app.set_events_only(ix, *checked, cx));
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
                        this.with_tab(cx, |app, ix, cx| {
                            app.set_hide_sub_messages(ix, *checked, cx)
                        });
                    }))
                    .into_any_element(),
            ));

            card = card.child(card_divider()).child(setting_row(
                "Collapse gift batches",
                Some(
                    "One \"gifted 50 subs\" row, expandable to the recipients, instead of 50 rows.",
                ),
                Switch::new("collapse-gift-subs")
                    .small()
                    .checked(tab.config.collapse_gift_subs)
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        this.with_tab(cx, |app, ix, cx| {
                            app.set_collapse_gift_subs(ix, *checked, cx)
                        });
                    }))
                    .into_any_element(),
            ));
        }

        // The kind list renders with or without the panel: the visibility
        // checkbox is panel-only (a plain label replaces it when the panel is
        // off), but the sound bells always apply — events still show in the
        // chat log, and their ping shouldn't require the panel.
        let muted_fg = cx.theme().muted_foreground;
        let kinds = EventKind::ALL.into_iter().map(|kind| {
            h_flex()
                .justify_between()
                .child(if show {
                    Checkbox::new(SharedString::from(format!("event-kind-{}", kind.label())))
                        .label(kind.label())
                        .checked(filter.enabled(kind))
                        .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                            this.with_tab(cx, |app, ix, cx| {
                                app.set_event_kind(ix, kind, *checked, cx)
                            });
                        }))
                        .into_any_element()
                } else {
                    div().child(kind.label()).into_any_element()
                })
                .child(self.event_bell(ix, kind, cx))
        });
        card = card.child(card_divider()).child(
            v_flex()
                .gap_1()
                .px_3()
                .py_2()
                .child(div().text_xs().text_color(muted_fg).child(if show {
                    "The bell also plays a sound when that event happens."
                } else {
                    "The bell plays a sound when that event happens (events show in the chat log)."
                }))
                .children(kinds.map(IntoElement::into_any_element)),
        );

        let show_mentions = tab.config.layout.contains(tabs::PanelKind::Mentions);
        let mut mentions_card = setting_card().child(setting_row(
            "Show mentions panel",
            Some("Messages that mention you, in a side panel."),
            Switch::new("show-mentions-panel")
                .small()
                .checked(show_mentions)
                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                    this.with_tab(cx, |app, ix, cx| {
                        app.set_panel_shown(ix, tabs::PanelKind::Mentions, *checked, cx)
                    });
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
                        this.with_tab(cx, |app, ix, cx| {
                            app.set_mentions_all_tabs(ix, *checked, cx)
                        });
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
    /// controller. The Twitch row carries the login-permissions chooser (an
    /// inline expander, not a dialog — child windows render no dialog layer).
    fn account_section(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let state = self.session.login_state();
        let mut card = setting_card().child(self.twitch_account_row(state.twitch, cx));
        if self.twitch_perm_open {
            card = card
                .child(card_divider())
                .child(self.twitch_permissions_editor(cx));
        }
        card = card.child(card_divider()).child(self.account_row(
            bks_core::Platform::Kick,
            state.kick,
            cx,
            |c| c.kick_login(),
            |c| c.kick_logout(),
        ));
        v_flex()
            .gap_2()
            .child(section_title("Accounts"))
            .child(card)
            .into_any_element()
    }
    /// The Twitch account row: logo + status — including what the token is
    /// allowed to do ("full moderator + broadcaster") — and Log in /
    /// Permissions… / Log out. Log in expands the permissions chooser instead
    /// of jumping straight to the browser; Permissions… reopens it to re-login
    /// at a different tier (the only way scopes can change).
    fn twitch_account_row(
        &self,
        account: Option<String>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let logged_in = account.is_some();
        let status = match &account {
            // A legacy token with no stored scopes is unknown, not chat-only —
            // show no tier rather than a wrong one (gating skips it too).
            Some(name) => match session::twitch_granted_scopes().filter(|s| !s.is_empty()) {
                Some(scopes) => format!(
                    "Logged in as {name} — {}",
                    bks_auth::twitch::ScopeChoice::summarize(&scopes)
                ),
                None => format!("Logged in as {name}"),
            },
            None => "Not logged in".to_string(),
        };
        let buttons = if logged_in {
            h_flex()
                .gap_1()
                .child(
                    Button::new("twitch-permissions")
                        .label("Permissions…")
                        .small()
                        .outline()
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_twitch_permissions(cx))),
                )
                .child(
                    Button::new("logout-Twitch")
                        .label("Log out")
                        .small()
                        .danger()
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(c) = this.active_controller(cx) {
                                c.twitch_logout();
                            }
                            this.twitch_perm_open = false;
                            cx.notify();
                        })),
                )
        } else {
            h_flex().child(
                Button::new("login-Twitch")
                    .label("Log in")
                    .small()
                    .primary()
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_twitch_permissions(cx))),
            )
        };
        self.account_row_shell(
            bks_core::Platform::Twitch,
            status,
            buttons.into_any_element(),
            cx,
        )
    }
    /// The shared account-row layout (platform icon, name + status column, the
    /// action buttons at the right) both platform rows render through, so they
    /// can't drift apart inside the same card.
    fn account_row_shell(
        &self,
        platform: bks_core::Platform,
        status: String,
        buttons: gpui::AnyElement,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
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
                            .child(SharedString::from(platform.label())),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(SharedString::from(status)),
                    ),
            )
            .child(buttons)
            .into_any_element()
    }
    /// Opens (seeding from the saved choice, chat-only for a first login) or
    /// closes the Twitch permissions chooser.
    fn toggle_twitch_permissions(&mut self, cx: &mut Context<Self>) {
        self.twitch_perm_open = !self.twitch_perm_open;
        if self.twitch_perm_open {
            self.twitch_perm_choice =
                self.settings
                    .twitch_login_scopes
                    .unwrap_or(bks_auth::twitch::ScopeChoice {
                        preset: bks_auth::twitch::ScopePreset::ChatOnly,
                        broadcaster: false,
                    });
        }
        cx.notify();
    }
    /// Persists the chosen tier and launches the browser login with it.
    fn confirm_twitch_login(&mut self, cx: &mut Context<Self>) {
        let choice = self.twitch_perm_choice;
        self.twitch_perm_open = false;
        self.with_app(cx, |app, cx| {
            app.settings.twitch_login_scopes = Some(choice);
            app.save_settings(cx);
            if let Some(controller) = app.active_controller(cx) {
                controller.twitch_login(choice);
            }
            cx.notify();
        });
        cx.notify();
    }
    /// The expanded permissions chooser: three preset tiers as radio rows, the
    /// broadcaster add-on checkbox, and the button that opens the browser.
    /// What's picked here is exactly what Twitch's consent screen lists — the
    /// point of the chooser (the full scope list scares off someone who only
    /// wants to chat).
    fn twitch_permissions_editor(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        use bks_auth::twitch::ScopePreset;
        use gpui_component::Icon;

        let choice = self.twitch_perm_choice;
        let accent = cx.theme().primary;
        // One selectable option row — the radio tiers and the checkbox add-on
        // share this chrome so their styling can't drift apart.
        let option_row = |id: &'static str,
                          selected: bool,
                          indicator: gpui::AnyElement,
                          title: &'static str,
                          blurb: &'static str,
                          on_click: PermRowAction,
                          cx: &mut Context<Self>| {
            let hover_bg = cx.theme().secondary;
            h_flex()
                .id(id)
                .w_full()
                .items_start()
                .gap_2()
                .px_2()
                .py_1p5()
                .rounded_md()
                .cursor_pointer()
                .when(selected, |el| el.bg(accent.opacity(0.12)))
                .when(!selected, |el| el.hover(move |s| s.bg(hover_bg)))
                .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
                .child(indicator)
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            div()
                                .text_size(px(13.))
                                .font_weight(FontWeight::MEDIUM)
                                .child(SharedString::from(title)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(SharedString::from(blurb)),
                        ),
                )
        };
        let radio_dot = |selected: bool, cx: &Context<Self>| {
            div()
                .flex_none()
                .mt(px(2.))
                .size(px(14.))
                .rounded_full()
                .border_2()
                .border_color(if selected { accent } else { cx.theme().border })
                .flex()
                .items_center()
                .justify_center()
                .when(selected, |el| {
                    el.child(div().size(px(6.)).rounded_full().bg(accent))
                })
                .into_any_element()
        };
        let preset_row = |id: &'static str,
                          preset: ScopePreset,
                          title: &'static str,
                          blurb: &'static str,
                          cx: &mut Context<Self>| {
            let selected = choice.preset == preset;
            option_row(
                id,
                selected,
                radio_dot(selected, cx),
                title,
                blurb,
                Box::new(move |this, cx| {
                    this.twitch_perm_choice.preset = preset;
                    cx.notify();
                }),
                cx,
            )
        };

        v_flex()
            .w_full()
            .px_3()
            .py_2p5()
            .gap_2()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from(
                        "Choose how much Backseater may do with your Twitch account — \
                         the browser's consent screen lists exactly this, nothing more. \
                         Change it any time by logging in again.",
                    )),
            )
            .child(preset_row(
                "twitch-perm-chat",
                ScopePreset::ChatOnly,
                "Chat only",
                "Read and send chat messages. The shortest consent screen.",
                cx,
            ))
            .child(preset_row(
                "twitch-perm-basic",
                ScopePreset::BasicModeration,
                "Basic moderation",
                "Chat, plus ban/timeout, delete messages, pin messages, and the viewer list.",
                cx,
            ))
            .child(preset_row(
                "twitch-perm-full",
                ScopePreset::FullModerator,
                "Full moderator",
                "Every moderation tool: warnings, announcements, chat modes, the AutoMod \
                 queue, suspicious users, and the rich mod-action feed.",
                cx,
            ))
            .child({
                // A checkbox indicator in the same row chrome — it's an
                // add-on, not a fourth tier.
                let checked = choice.broadcaster;
                let boxed = div()
                    .flex_none()
                    .mt(px(2.))
                    .size(px(14.))
                    .rounded_sm()
                    .border_2()
                    .border_color(if checked { accent } else { cx.theme().border })
                    .when(checked, |el| el.bg(accent))
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(checked, |el| {
                        el.child(
                            Icon::new(IconName::Check)
                                .size(px(10.))
                                .text_color(cx.theme().primary_foreground),
                        )
                    })
                    .into_any_element();
                option_row(
                    "twitch-perm-broadcaster",
                    checked,
                    boxed,
                    "Broadcaster tools",
                    "/raid and granting mod/VIP — only useful on your own channel.",
                    Box::new(|this, cx| {
                        this.twitch_perm_choice.broadcaster = !this.twitch_perm_choice.broadcaster;
                        cx.notify();
                    }),
                    cx,
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("twitch-perm-cancel")
                            .label("Cancel")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.twitch_perm_open = false;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("twitch-perm-go")
                            .label("Open Twitch login")
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| this.confirm_twitch_login(cx))),
                    ),
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
        self.account_row_shell(platform, status, button.into_any_element(), cx)
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
                .border_color(if selected {
                    accent
                } else {
                    gpui::transparent_black()
                })
                .when(selected, |s| s.bg(accent.opacity(0.16)))
                .when(!selected, |s| s.hover(|s| s.bg(cx.theme().secondary)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        this.with_app(cx, |app, cx| app.set_theme(choice.clone(), window, cx))
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
                            .on_click(
                                cx.listener(|this, _, window, cx| this.save_theme(window, cx)),
                            ),
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
                        this.with_app(cx, |app, cx| app.adjust_font_size(-1.0, cx));
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
                        this.with_app(cx, |app, cx| app.adjust_font_size(1.0, cx));
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
                                this.with_app(cx, |app, cx| app.set_show_7tv_paints(*checked, cx));
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
                                this.with_app(cx, |app, cx| app.set_show_status_bar(*checked, cx));
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
                                this.with_app(cx, |app, cx| {
                                    app.set_pause_chat_on_hover(*checked, cx)
                                });
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
                                this.with_app(cx, |app, cx| app.set_compact_chat(*checked, cx));
                            }))
                            .into_any_element(),
                    ))
                    .child(card_divider())
                    .child(setting_row(
                        "Flash tab when a channel goes live",
                        Some(
                            "Briefly pulse a tab's chip when one of its Twitch, \
                             Kick, YouTube, or TikTok channels starts streaming.",
                        ),
                        Switch::new("flash-tab-on-live")
                            .small()
                            .checked(self.settings.flash_tab_on_live)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.with_app(cx, |app, cx| {
                                    app.set_flash_tab_on_live(*checked, cx)
                                });
                            }))
                            .into_any_element(),
                    )),
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
                                this.with_app(cx, |app, cx| {
                                    app.set_show_timestamps(TimestampSurface::Chat, *checked, cx)
                                });
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
                                this.with_app(cx, |app, cx| {
                                    app.set_show_timestamps(TimestampSurface::Events, *checked, cx)
                                });
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
                                this.with_app(cx, |app, cx| {
                                    app.set_show_timestamps(
                                        TimestampSurface::Mentions,
                                        *checked,
                                        cx,
                                    )
                                });
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
                    this.with_app(cx, |app, cx| app.set_show_pinned(platform, *checked, cx));
                })),
            )
            .into_any_element()
    }
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
            on_click: impl Fn(&mut SettingsView, &mut Window, &mut Context<SettingsView>) + 'static,
            cx: &mut Context<SettingsView>,
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
                    .child(
                        div()
                            .flex_none()
                            .w(px(18.))
                            .flex()
                            .justify_center()
                            .child(icon),
                    )
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
            .child(setting_card().child(setting_row(
                "Show mod buttons",
                Some(
                    "Moderation buttons at the left of each message in channels \
                         you moderate. \"On hover\" shows them only while the mouse \
                         is over a message.",
                ),
                mode_seg,
            )))
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
    /// Adds a custom mod button from the editor fields. Only the command is
    /// required; an empty name falls back to the command, an empty icon to the
    /// gavel.
    fn add_mod_button(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self
            .settings_inputs
            .mod_name
            .read(cx)
            .value()
            .trim()
            .to_string();
        let icon = self
            .settings_inputs
            .mod_icon
            .read(cx)
            .value()
            .trim()
            .to_string();
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
            name: if name.is_empty() {
                command.clone()
            } else {
                name
            },
            icon: if icon.is_empty() {
                "gavel".into()
            } else {
                icon
            },
            command,
            platform: self.mod_button_platform,
        };
        let editing = self.editing_mod_button.take();
        self.with_app(cx, |app, cx| {
            match editing {
                Some(ix) if ix < app.settings.mod_buttons.len() => {
                    app.settings.mod_buttons[ix] = button
                }
                _ => app.settings.mod_buttons.push(button),
            }
            app.save_mod_buttons(cx);
        });
        self.clear_mod_button_editor(window, cx);
        cx.notify();
    }
    /// Removes the mod button at `ix` (a row's ✕).
    fn remove_mod_button(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.with_app(cx, |app, cx| {
            if ix < app.settings.mod_buttons.len() {
                app.settings.mod_buttons.remove(ix);
                app.save_mod_buttons(cx);
            }
        });
        self.editing_mod_button = None;
        cx.notify();
    }
    /// Swaps the mod button at `ix` with the one at `other` (a row's ▲/▼).
    fn move_mod_button(&mut self, ix: usize, other: usize, cx: &mut Context<Self>) {
        let moved = self
            .app
            .update(cx, |app, cx| {
                let len = app.settings.mod_buttons.len();
                if ix >= len || other >= len || ix == other {
                    return false;
                }
                app.settings.mod_buttons.swap(ix, other);
                app.save_mod_buttons(cx);
                true
            })
            .unwrap_or(false);
        if moved {
            self.editing_mod_button = match self.editing_mod_button {
                Some(e) if e == ix => Some(other),
                Some(e) if e == other => Some(ix),
                keep => keep,
            };
            cx.notify();
        }
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
        self.with_app(cx, |app, cx| {
            app.settings.mod_buttons = settings::default_mod_buttons();
            app.settings.mod_buttons_seeded = true;
            app.save_mod_buttons(cx);
        });
        self.editing_mod_button = None;
        cx.notify();
    }
    fn streamer_section(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        use settings::StreamerModeChoice;
        let current = self.settings.streamer_mode;
        let streamer_seg = setting_dropdown(
            &self.settings_streamer,
            settings::StreamerModeChoice::LABELS,
        );

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
                        "Mute alert sounds while active",
                        Some("Mention and event pings stay silent so they don't leak into the stream."),
                        {
                            use gpui_component::switch::Switch;
                            Switch::new("streamer-mute-sounds")
                                .small()
                                .checked(self.settings.streamer_mute_sounds)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    this.with_app(cx, |app, cx| app.set_streamer_mute_sounds(*checked, cx));
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
                                    this.with_app(cx, |app, cx| app.set_streamer_hide_thumbnails(*checked, cx));
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
                                this.with_app(cx, |app, cx| app.set_beta_updates(*checked, cx));
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
                        this.with_app(cx, |app, cx| {
                            app.settings.mentions_tab = *checked;
                            if !*checked {
                                app.mentions_tab_selected = false;
                            }
                            app.save_settings(cx);
                            cx.notify();
                        });
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
    /// The shared bell/bell-off sound-toggle chrome (mention chips + event
    /// kinds): `ringing` = the sound is on. Vector icons, not 🔔/🔕 emoji —
    /// small emoji bells render ambiguously (the plain bell read as "crossed"),
    /// so the two states looked alike.
    fn bell_toggle(
        id: SharedString,
        ringing: bool,
        cx: &Context<Self>,
        on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    ) -> gpui::AnyElement {
        div()
            .id(id)
            .px_1()
            .py_0p5()
            .rounded_md()
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().muted))
            .when(!ringing, |s| s.opacity(0.55))
            .child(
                gpui::svg()
                    .path(if ringing {
                        "icons/bell.svg"
                    } else {
                        "icons/bell-off.svg"
                    })
                    .size(px(14.))
                    .flex_none()
                    .text_color(cx.theme().muted_foreground),
            )
            .on_click(on_click)
            .into_any_element()
    }
    /// A mention chip's bell toggle (only mention terms get one; ignore terms
    /// have no sound). Muting is app-wide by normalized term.
    fn term_bell(&self, id_stem: &str, term: &str, cx: &mut Context<Self>) -> gpui::AnyElement {
        let muted = self
            .settings
            .muted_mentions
            .contains(&bks_core::normalize_term(term));
        let toggle = term.to_string();
        Self::bell_toggle(
            SharedString::from(format!("bell-{id_stem}-{term}")),
            !muted,
            cx,
            cx.listener(move |this, _, _, cx| {
                this.with_app(cx, |app, cx| app.toggle_mention_mute(&toggle, cx));
            }),
        )
    }
    /// An event kind's bell toggle in the events-panel settings: whether that
    /// kind plays the alert ping when it arrives live.
    fn event_bell(&self, ix: usize, kind: EventKind, cx: &mut Context<Self>) -> gpui::AnyElement {
        let on = self
            .tabs
            .get(ix)
            .is_some_and(|t| t.config.event_sounds.enabled(kind));
        Self::bell_toggle(
            SharedString::from(format!("event-bell-{}", kind.label())),
            on,
            cx,
            cx.listener(move |this, _, _, cx| {
                this.with_tab(cx, |app, ix, cx| {
                    let on = app.tabs[ix].config.event_sounds.enabled(kind);
                    app.set_event_sound(ix, kind, !on, cx);
                });
            }),
        )
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
        chips.extend(self.terms(list).iter().map(|term| {
            let remove = term.clone();
            // A `user:` entry renders as a labeled user chip — a mono user
            // glyph, the name, and the scope as the platform's logo (mono
            // globe = all platforms) — instead of the raw grammar string;
            // anything else shows verbatim.
            let body: gpui::AnyElement = match bks_core::parse_user_entry(term) {
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
                    chip.child(self.term_bell(&list.id_stem(), term, cx))
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
                s.child(
                    setting_card().child(setting_row(
                        "Play a sound on mention",
                        Some(
                            "A term's bell button mutes just that term; streamer mode \
                         mutes all sounds unless changed in Streamer Mode settings.",
                        ),
                        Switch::new("mention-sound")
                            .small()
                            .checked(self.settings.mention_sound)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.with_app(cx, |app, cx| app.set_mention_sound(*checked, cx));
                            }))
                            .into_any_element(),
                    )),
                )
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
        const KINDS: [TermEntryKind; 3] = [
            TermEntryKind::Text,
            TermEntryKind::Regex,
            TermEntryKind::User,
        ];
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
        let plat_chip =
            |selected: bool, id: String, label: &'static str, cx: &mut Context<Self>| {
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
                        .hover(|s| {
                            s.bg(cx.theme().secondary_hover)
                                .text_color(cx.theme().foreground)
                        })
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
        let one_chip =
            |platform: bks_core::Platform, label: &'static str, cx: &mut Context<Self>| {
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
                        .child(one_chip(bks_core::Platform::YouTube, "YouTube", cx))
                        .child(one_chip(bks_core::Platform::TikTok, "TikTok", cx)),
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
    /// Points the editor's input placeholder at its current add mode whenever
    /// the user changes the mode selector.
    fn sync_term_placeholder(&self, list: TermList, window: &mut Window, cx: &mut Context<Self>) {
        let (kind, _) = self.term_add_mode(list);
        self.term_input(list).clone().update(cx, |s, cx| {
            s.set_placeholder(term_placeholder(kind), window, cx)
        });
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
        let added = self
            .app
            .update(cx, |app, cx| {
                self.resolve_term_list(list, app)
                    .is_some_and(|list| app.add_terms(list, entries, cx))
            })
            .unwrap_or(false);
        if added {
            input.update(cx, |s, cx| s.set_value("", window, cx));
            cx.notify();
        }
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
                        this.with_app(cx, |app, cx| app.adjust_suppressed_opacity(-0.05, cx));
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
                        this.with_app(cx, |app, cx| app.adjust_suppressed_opacity(0.05, cx));
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
    /// The Off / Top / Bottom dropdown picking where the chat-mode bar sits.
    fn chat_modes_placement_seg(&self) -> gpui::AnyElement {
        setting_dropdown(
            &self.settings_chat_modes,
            settings::ChatModesPlacement::LABELS,
        )
    }
    fn link_preview_mode_seg(&self) -> gpui::AnyElement {
        setting_dropdown(
            &self.settings_link_preview,
            settings::LinkPreviewMode::LABELS,
        )
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
        let draft = draft.clone();
        self.with_app(cx, |app, cx| {
            if app.settings.theme.custom_name() == Some(draft.name.as_str()) {
                if let Some(saved) = app
                    .settings
                    .custom_themes
                    .iter_mut()
                    .find(|t| t.name == draft.name)
                {
                    *saved = draft;
                    app.save_settings(cx);
                }
            }
            app.reapply_theme_colors(cx);
        });
        cx.notify();
    }
    /// Starts editing a new custom theme: opens the editor on a fresh draft
    /// (seeded from the dark base) and seeds its input controls.
    fn new_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.theme_draft = Some(default_custom_theme(true, String::new()));
        self.reset_theme_editor(window, cx);
        cx.notify();
    }
    /// Opens the editor on an existing saved theme `name` (so its colors can be
    /// tweaked), seeding the name field and pickers from it.
    fn edit_theme(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(theme) = self.settings.custom_themes.iter().find(|t| t.name == name) else {
            return;
        };
        let theme = theme.clone();
        self.theme_draft = Some(theme);
        self.reset_theme_editor(window, cx);
        cx.notify();
    }
    /// Closes the theme editor without saving the in-progress draft (a saved
    /// profile keeps whatever was already persisted).
    fn cancel_theme_edit(&mut self, cx: &mut Context<Self>) {
        self.theme_draft = None;
        cx.notify();
    }
    /// Seeds the theme controls from a newly selected draft, within the same
    /// settings window that owns all of the editor's inputs.
    fn reset_theme_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (name, pickers, subs) = Self::theme_inputs(self.theme_draft.as_ref(), window, cx);
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
        self.theme_draft = Some(draft.clone());
        self.with_app(cx, |app, cx| {
            if let Some(existing) = app
                .settings
                .custom_themes
                .iter_mut()
                .find(|t| t.name == name)
            {
                *existing = draft;
            } else {
                app.settings.custom_themes.push(draft);
            }
            app.settings.theme = settings::ThemeChoice::Custom(name);
            app.save_settings(cx);
            app.reapply_theme(window, cx);
        });
        cx.notify();
    }
    /// Deletes a saved theme profile. If it was the active theme, falls back to
    /// the dark built-in.
    fn delete_theme(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .theme_draft
            .as_ref()
            .is_some_and(|draft| draft.name == name)
        {
            self.theme_draft = None;
        }
        self.with_app(cx, |app, cx| {
            app.settings
                .custom_themes
                .retain(|theme| theme.name != name);
            let was_active = app.settings.theme.custom_name() == Some(name);
            if was_active {
                app.settings.theme = settings::ThemeChoice::Dark;
            }
            app.save_settings(cx);
            if was_active {
                app.reapply_theme(window, cx);
            } else {
                cx.notify();
            }
        });
        cx.notify();
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
pub(super) fn card_divider() -> impl IntoElement {
    div()
        .h(px(1.))
        .w_full()
        .bg(gpui::rgb(render::panel_border()))
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
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .child(
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
/// [`SettingsView::setting_select`]); this only draws the compact trigger.
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

fn setting_row(label: &str, desc: Option<&str>, control: gpui::AnyElement) -> gpui::AnyElement {
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
