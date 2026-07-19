//! App-wide UI preferences (currently just the chat font size).
//!
//! Persisted to `<config>/backseater/settings.json` and loaded on launch. Unlike
//! [`Session`](crate::session::Session) (login) these aren't security-sensitive;
//! they're plain UI state the settings dialog reads and writes.

use serde::{Deserialize, Serialize};

const STORE_NAME: &str = "settings";

/// Which color theme the app uses. Dark by default (the original look); Light is
/// a brighter palette; `Custom(name)` selects a user-defined
/// theme profile (see [`CustomTheme`]). Persisted so the choice survives restart.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    #[default]
    Dark,
    Light,
    /// A saved custom theme, keyed by its `name` in [`Settings::custom_themes`].
    Custom(String),
}

impl ThemeChoice {
    /// Whether this choice reads as a *dark* base (for the kit chrome mode). A
    /// custom theme reports its own `base_dark`; the caller resolves that.
    pub fn is_dark(&self) -> bool {
        matches!(self, ThemeChoice::Dark)
    }

    /// The saved-theme name this choice refers to, if it's a custom theme.
    pub fn custom_name(&self) -> Option<&str> {
        match self {
            ThemeChoice::Custom(name) => Some(name),
            _ => None,
        }
    }
}

/// A user-defined theme profile: a name plus the curated set of colors the theme
/// editor exposes. Colors are packed `0xRRGGBB`. The remaining palette fields are
/// derived from these at apply time (`render::Palette::from_custom`), so a saved
/// theme is small and forward-compatible.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomTheme {
    pub name: String,
    /// Whether the theme is built on the dark or light base (drives derived chrome
    /// tones + the kit's dark/light chrome mode).
    #[serde(default = "default_true")]
    pub base_dark: bool,
    pub chat_bg: u32,
    pub default_name: u32,
    pub first_message: u32,
    /// Highlighted (channel-point "Highlight My Message") row background. Added
    /// after the other theme colors, so a theme saved before it exists deserializes
    /// to `None`; `main.rs::custom_colors` then seeds it from the base default.
    /// `Some(0x000000)` is a real user pick (pure black), kept distinct from unset.
    #[serde(default)]
    pub highlighted: Option<u32>,
    pub event: u32,
    pub streak: u32,
    pub live: u32,
    pub offline: u32,
    pub mention: u32,
    pub link: u32,
    pub error: u32,
}

/// What drives streamer mode (see [`crate::streamer_mode`]). `Auto` (default)
/// turns it on while a broadcast app (OBS etc.) is running; `On`/`Off` force it,
/// with `Off` also disabling the auto-enable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamerModeChoice {
    Off,
    On,
    #[default]
    Auto,
}

impl StreamerModeChoice {
    /// The choices in display order (aligned with [`Self::LABELS`]).
    pub const ALL: [Self; 3] = [Self::Off, Self::On, Self::Auto];
    /// The labels shown in the settings picker (aligned with [`Self::ALL`]).
    pub const LABELS: &'static [&'static str] = &["Off", "On", "Auto"];
}

/// Where the chat-mode bar (slow / followers-only / sub-only / ...) sits:
/// hidden entirely, at the top of the chat panel (below the "Chat" header,
/// above pinned messages), or just above the input box. `Top` by default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatModesPlacement {
    Off,
    #[default]
    Top,
    Bottom,
}

impl ChatModesPlacement {
    /// The choices in display order (aligned with [`Self::LABELS`]).
    pub const ALL: [Self; 3] = [Self::Off, Self::Top, Self::Bottom];
    /// The labels shown in the settings picker (aligned with [`Self::ALL`]).
    pub const LABELS: &'static [&'static str] = &["Off", "Top", "Bottom"];
}

/// How link previews (YouTube videos + Twitch/Kick clips) are shown: not at all,
/// as a hover tooltip, or as an inline in-chat card under the message.
/// `Tooltip` by default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkPreviewMode {
    Off,
    #[default]
    Tooltip,
    Inline,
}

impl LinkPreviewMode {
    /// The choices in display order (aligned with [`Self::LABELS`]).
    pub const ALL: [Self; 3] = [Self::Off, Self::Tooltip, Self::Inline];
    /// The labels shown in the settings picker (aligned with [`Self::ALL`]).
    pub const LABELS: &'static [&'static str] = &["Off", "Tooltip on hover", "Inline in chat"];
}

/// How the per-message moderation buttons show: on every row the user can
/// moderate, only while the row is hovered, or not at all. `Hover` still
/// reserves the strip's width so message text doesn't shift as the pointer moves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModButtonMode {
    Off,
    #[default]
    Always,
    Hover,
}

impl ModButtonMode {
    /// The choices in display order (aligned with [`Self::LABELS`]).
    pub const ALL: [Self; 3] = [Self::Off, Self::Always, Self::Hover];
    /// The labels shown in the settings picker (aligned with [`Self::ALL`]).
    pub const LABELS: &'static [&'static str] = &["Off", "Always", "On hover"];
}

/// One moderation button in the chat rows' left-side strip. Clicking it runs
/// `command` with `{user}` replaced by the message author's login and
/// `{msg-id}` by the message id, targeted at the message's platform — any
/// slash command works, and plain text is sent to that platform's chat (e.g. a
/// bot command). The stock delete/ban/timeout are ordinary entries seeded on
/// first run ([`default_mod_buttons`]), so users reorder/edit/remove them too.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModButton {
    /// Shown as the button's tooltip.
    pub name: String,
    /// A bundled icon name (e.g. "gavel", "clock") or free text/emoji drawn as
    /// the button face (see `render::mod_icon_path`).
    pub icon: String,
    /// The command template, e.g. "/timeout {user} 1h spam".
    pub command: String,
    /// `None` = the button shows on every platform's rows.
    #[serde(default)]
    pub platform: Option<bks_core::Platform>,
}

/// The stock mod buttons, seeded into [`Settings::mod_buttons`] on first run
/// and restored by the settings panel's "Reset to defaults". No placeholders
/// needed: known commands get the row's target injected from the registry's
/// usage shape (`commands::implicit_target`) — "/delete" acts on the message
/// (which also ghosts it on local-echo rows, `commands::needs_msg_id`), the
/// others on its author.
pub fn default_mod_buttons() -> Vec<ModButton> {
    vec![
        ModButton {
            name: "Delete message".into(),
            icon: "trash".into(),
            command: "/delete".into(),
            platform: None,
        },
        ModButton {
            name: "Ban".into(),
            icon: "ban".into(),
            command: "/ban".into(),
            platform: None,
        },
        ModButton {
            name: "Timeout 10m".into(),
            icon: "clock".into(),
            command: "/timeout 600".into(),
            platform: None,
        },
    ]
}

/// The smallest / largest chat font size the UI offers, in pixels.
pub const MIN_FONT_SIZE: f32 = 12.0;
pub const MAX_FONT_SIZE: f32 = 28.0;
/// Default chat font size (matches the previous hard-coded value).
pub const DEFAULT_FONT_SIZE: f32 = 18.0;

/// Default opacity of a suppressed (term-matched) message: barely visible so the
/// eye skips it, still readable up close. User-adjustable in Highlights settings.
pub const DEFAULT_SUPPRESSED_OPACITY: f32 = 0.18;

/// Bounds the suppressed-opacity setting so it can never go fully invisible (0,
/// which would make suppress indistinguishable from ignore) or fully opaque (1,
/// which would make it do nothing).
pub const SUPPRESSED_OPACITY_RANGE: std::ops::RangeInclusive<f32> = 0.05..=0.9;

/// Persisted UI preferences.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    /// The UI font family (applies to chat and the rest of the app). `None` =
    /// the system default.
    #[serde(default)]
    pub font_family: Option<String>,
    /// Extra whole-word terms that highlight a message as a mention (in addition
    /// to your logged-in account names), e.g. "mods". Case-insensitive.
    #[serde(default)]
    pub custom_mentions: Vec<String>,
    /// Words/phrases whose messages are hidden from chat. A plain entry matches
    /// as a case-insensitive substring; a `re:`-prefixed entry is a regex.
    #[serde(default)]
    pub ignored_terms: Vec<String>,
    /// Words/phrases whose messages stay in chat but render at very low opacity
    /// (dimmed but readable) so the eye skips them. Same grammar as
    /// `ignored_terms`; the middle tier between show and ignore.
    #[serde(default)]
    pub suppressed_terms: Vec<String>,
    /// Opacity a suppressed message renders at (see `suppressed_terms`). Lower =
    /// fainter/easier to skip. Clamped to [`SUPPRESSED_OPACITY_RANGE`] on apply.
    #[serde(default = "default_suppressed_opacity")]
    pub suppressed_opacity: f32,
    /// Whether to show 7TV name paints (gradient/solid name colors) and 7TV
    /// badges. On by default; toggled live in settings (no restart).
    #[serde(default = "default_true")]
    pub show_7tv_paints: bool,
    /// The app color theme (dark/light/a custom profile). Dark by default;
    /// switched live in settings.
    #[serde(default)]
    pub theme: ThemeChoice,
    /// Saved user-defined theme profiles, selectable by name in the Themes
    /// settings category.
    #[serde(default)]
    pub custom_themes: Vec<CustomTheme>,
    /// Streamer mode: forced on/off, or auto (on while OBS & co. run).
    #[serde(default)]
    pub streamer_mode: StreamerModeChoice,
    /// Whether the pinned-message banner shows for Twitch pins. Hiding a platform
    /// suppresses its banner entirely (the ✕ on the banner itself dismisses just
    /// the current pin instead).
    #[serde(default = "default_true")]
    pub show_pinned_twitch: bool,
    /// Whether the pinned-message banner shows for Kick pins.
    #[serde(default = "default_true")]
    pub show_pinned_kick: bool,
    /// Whether the live status bar (per-platform viewer counts) shows above the
    /// chat log. On by default.
    #[serde(default = "default_true")]
    pub show_status_bar: bool,
    /// Where the chat-mode bar (slow / followers-only / sub-only / ...) sits:
    /// off, at the top of the chat panel, or above the input box. Top by default.
    #[serde(default)]
    pub chat_modes_placement: ChatModesPlacement,
    /// Whether message timestamps show in the chat log. On by default.
    #[serde(default = "default_true")]
    pub show_timestamps_chat: bool,
    /// Whether hovering the chat log pauses it (new messages held back until the
    /// pointer leaves; a view the user scrolled up themselves is left alone).
    /// Off by default.
    #[serde(default)]
    pub pause_chat_on_hover: bool,
    /// Compact chat: less vertical gap between messages, fitting more lines on
    /// screen. Off by default (the roomier layout is the default look).
    #[serde(default)]
    pub compact_chat: bool,
    /// Briefly flash a tab's chip when one of its channels goes live. Off by
    /// default. Read only by `BackseaterApp` (the tab strip), so no process-wide
    /// flag — a plain field is enough.
    #[serde(default)]
    pub flash_tab_on_live: bool,
    /// Whether timestamps show on the events panel's rows. On by default.
    #[serde(default = "default_true")]
    pub show_timestamps_events: bool,
    /// Whether timestamps show on the mentions panel's rows. On by default.
    #[serde(default = "default_true")]
    pub show_timestamps_mentions: bool,
    /// Whether the tab strip shows the global Mentions tab: a pinned pseudo-tab
    /// collecting every tab's mentions in one feed. Off by default.
    #[serde(default)]
    pub mentions_tab: bool,
    /// A custom display name for the global Mentions tab (chip label + window
    /// titles). `None` = the default "Mentions".
    #[serde(default)]
    pub mentions_tab_name: Option<String>,
    /// Whether the updater also installs pre-release (beta) builds. Off by
    /// default; betas move to the next stable automatically (semver ordering).
    #[serde(default)]
    pub beta_updates: bool,
    /// Whether a mention plays the alert ping. Off by default (opt-in);
    /// individual terms can then be muted via [`muted_mentions`](Self::muted_mentions).
    #[serde(default)]
    pub mention_sound: bool,
    /// Mention terms (normalized: lowercase, no `@`) whose matches stay silent
    /// while [`mention_sound`](Self::mention_sound) is on. May include account
    /// names and per-tab terms — one app-wide mute list.
    #[serde(default)]
    pub muted_mentions: Vec<String>,
    /// Whether streamer mode also mutes alert sounds (mention + event pings).
    /// On by default — going live shouldn't leak pings into the stream unless
    /// the user opts out.
    #[serde(default = "default_true")]
    pub streamer_mute_sounds: bool,
    /// Whether streamer mode hides link-preview thumbnails (tooltip + inline
    /// card) — a thumbnail can reveal what a posted link points at on stream. On
    /// by default while streamer mode is active; the rest of the preview (title,
    /// channel, views) still shows.
    #[serde(default = "default_true")]
    pub streamer_hide_thumbnails: bool,
    /// How link previews show (off / hover tooltip / inline card). Tooltip by
    /// default.
    #[serde(default)]
    pub link_preview_mode: LinkPreviewMode,
    /// How the per-message moderation buttons show (always / on hover / off).
    #[serde(default)]
    pub mod_button_mode: ModButtonMode,
    /// The moderation buttons, in strip order — the stock three seeded on
    /// first run, plus whatever the user added/edited/reordered.
    #[serde(default)]
    pub mod_buttons: Vec<ModButton>,
    /// Whether the stock buttons were seeded into [`mod_buttons`](Self::mod_buttons)
    /// yet, so an intentionally emptied list stays empty on later launches.
    #[serde(default)]
    pub mod_buttons_seeded: bool,
    /// The scope tier the last Twitch login requested (Account → the login
    /// permissions chooser). `None` = never chosen — the chooser then defaults
    /// to chat-only, the least scary consent screen.
    #[serde(default)]
    pub twitch_login_scopes: Option<bks_auth::twitch::ScopeChoice>,
}

fn default_font_size() -> f32 {
    DEFAULT_FONT_SIZE
}

fn default_suppressed_opacity() -> f32 {
    DEFAULT_SUPPRESSED_OPACITY
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font_size: DEFAULT_FONT_SIZE,
            font_family: None,
            custom_mentions: Vec::new(),
            ignored_terms: Vec::new(),
            suppressed_terms: Vec::new(),
            suppressed_opacity: DEFAULT_SUPPRESSED_OPACITY,
            show_7tv_paints: true,
            theme: ThemeChoice::Dark,
            custom_themes: Vec::new(),
            streamer_mode: StreamerModeChoice::Auto,
            show_pinned_twitch: true,
            show_pinned_kick: true,
            show_status_bar: true,
            chat_modes_placement: ChatModesPlacement::default(),
            show_timestamps_chat: true,
            pause_chat_on_hover: false,
            compact_chat: false,
            flash_tab_on_live: false,
            show_timestamps_events: true,
            show_timestamps_mentions: true,
            mentions_tab: false,
            mentions_tab_name: None,
            beta_updates: false,
            mention_sound: false,
            muted_mentions: Vec::new(),
            streamer_mute_sounds: true,
            streamer_hide_thumbnails: true,
            link_preview_mode: LinkPreviewMode::default(),
            mod_button_mode: ModButtonMode::default(),
            mod_buttons: default_mod_buttons(),
            mod_buttons_seeded: true,
            twitch_login_scopes: None,
        }
    }
}

impl Settings {
    /// Loads saved settings, falling back to defaults if none are saved.
    pub fn load() -> Self {
        match bks_auth::store::load::<Settings>(STORE_NAME) {
            Ok(Some(mut s)) => {
                // One-time seed of the stock mod buttons into a pre-existing
                // settings file (fresh installs get them via `Default`). Not
                // saved here — idempotent on every launch until the next save.
                if !s.mod_buttons_seeded {
                    let mut buttons = default_mod_buttons();
                    buttons.append(&mut s.mod_buttons);
                    s.mod_buttons = buttons;
                    s.mod_buttons_seeded = true;
                }
                s
            }
            _ => Settings::default(),
        }
    }

    /// The global Mentions tab's display name: the custom name if one is set,
    /// else "Mentions".
    pub fn mentions_tab_label(&self) -> &str {
        self.mentions_tab_name.as_deref().unwrap_or("Mentions")
    }

    /// The saved custom theme matching the active [`theme`](Self::theme) choice,
    /// if it's a `Custom` selection and the profile still exists.
    pub fn active_custom_theme(&self) -> Option<&CustomTheme> {
        let name = self.theme.custom_name()?;
        self.custom_themes.iter().find(|t| t.name == name)
    }

    /// Persists the settings, logging on failure (not fatal to the UI).
    pub fn save(&self) {
        if let Err(err) = bks_auth::store::save(STORE_NAME, self) {
            tracing::warn!("failed to save settings: {err:#}");
        }
    }

    /// Pushes the pinned-banner and status-bar visibility into the process-wide
    /// flags the chat views render against (same pattern as the theme flag).
    /// Call on load and after every toggle.
    pub fn apply_visibility_flags(&self) {
        SHOW_PINNED_TWITCH.store(self.show_pinned_twitch, Ordering::Relaxed);
        SHOW_PINNED_KICK.store(self.show_pinned_kick, Ordering::Relaxed);
        SHOW_STATUS_BAR.store(self.show_status_bar, Ordering::Relaxed);
        CHAT_MODES_PLACEMENT.store(self.chat_modes_placement as u8, Ordering::Relaxed);
        SHOW_TIMESTAMPS_CHAT.store(self.show_timestamps_chat, Ordering::Relaxed);
        SHOW_TIMESTAMPS_EVENTS.store(self.show_timestamps_events, Ordering::Relaxed);
        SHOW_TIMESTAMPS_MENTIONS.store(self.show_timestamps_mentions, Ordering::Relaxed);
        PAUSE_CHAT_ON_HOVER.store(self.pause_chat_on_hover, Ordering::Relaxed);
        COMPACT_CHAT.store(self.compact_chat, Ordering::Relaxed);
        LINK_PREVIEW_MODE.store(self.link_preview_mode as u8, Ordering::Relaxed);
        STREAMER_HIDE_THUMBNAILS.store(self.streamer_hide_thumbnails, Ordering::Relaxed);
        let opacity = self
            .suppressed_opacity
            .clamp(*SUPPRESSED_OPACITY_RANGE.start(), *SUPPRESSED_OPACITY_RANGE.end());
        SUPPRESSED_OPACITY.store(opacity.to_bits(), Ordering::Relaxed);
    }

    /// Pushes the mention-sound master + streamer-mute toggles into the
    /// process-wide flags the play path reads. Call on load and after toggles.
    pub fn apply_sound_flags(&self) {
        MENTION_SOUND.store(self.mention_sound, Ordering::Relaxed);
        STREAMER_MUTE.store(self.streamer_mute_sounds, Ordering::Relaxed);
    }

    /// Pushes the mod-button mode + custom button list into the process-wide
    /// state the chat rows render against. Call on load and after every edit.
    pub fn apply_mod_buttons(&self) {
        *MOD_BUTTONS.write().unwrap() = (
            self.mod_button_mode,
            Some(Arc::new(self.mod_buttons.clone())),
        );
    }
}

// `Option` because `Arc::new` isn't const; `None` (pre-`apply_mod_buttons`)
// reads as an empty list.
static MOD_BUTTONS: RwLock<(ModButtonMode, Option<Arc<Vec<ModButton>>>)> =
    RwLock::new((ModButtonMode::Always, None));

/// The active mod-button visibility mode (process-wide, like the theme flag).
pub fn mod_button_mode() -> ModButtonMode {
    MOD_BUTTONS.read().unwrap().0
}

/// The mod buttons in strip order (a cheap `Arc` clone per row render).
pub fn mod_buttons() -> Arc<Vec<ModButton>> {
    MOD_BUTTONS.read().unwrap().1.clone().unwrap_or_default()
}

/// Whether the mention ping is enabled at all (the master toggle).
pub fn mention_sound_enabled() -> bool {
    MENTION_SOUND.load(Ordering::Relaxed)
}

/// Whether active streamer mode should silence mention pings.
pub fn streamer_mute_sounds() -> bool {
    STREAMER_MUTE.load(Ordering::Relaxed)
}

static MENTION_SOUND: AtomicBool = AtomicBool::new(false);
static STREAMER_MUTE: AtomicBool = AtomicBool::new(true);

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, RwLock};

/// Suppressed-message opacity, stored as `f32::to_bits` (an atomic float has no
/// stable primitive). Seeded to the default until `apply_visibility_flags` runs.
static SUPPRESSED_OPACITY: AtomicU32 = AtomicU32::new(DEFAULT_SUPPRESSED_OPACITY.to_bits());

/// The opacity a suppressed message renders at (process-wide, like the theme
/// flag). Read per row by `render::render_message`.
pub fn suppressed_opacity() -> f32 {
    f32::from_bits(SUPPRESSED_OPACITY.load(Ordering::Relaxed))
}

static SHOW_PINNED_TWITCH: AtomicBool = AtomicBool::new(true);
static SHOW_PINNED_KICK: AtomicBool = AtomicBool::new(true);
static SHOW_STATUS_BAR: AtomicBool = AtomicBool::new(true);
static CHAT_MODES_PLACEMENT: AtomicU8 = AtomicU8::new(ChatModesPlacement::Top as u8);
static SHOW_TIMESTAMPS_CHAT: AtomicBool = AtomicBool::new(true);
static SHOW_TIMESTAMPS_EVENTS: AtomicBool = AtomicBool::new(true);
static SHOW_TIMESTAMPS_MENTIONS: AtomicBool = AtomicBool::new(true);
static PAUSE_CHAT_ON_HOVER: AtomicBool = AtomicBool::new(false);
static COMPACT_CHAT: AtomicBool = AtomicBool::new(false);
static LINK_PREVIEW_MODE: AtomicU8 = AtomicU8::new(LinkPreviewMode::Tooltip as u8);
static STREAMER_HIDE_THUMBNAILS: AtomicBool = AtomicBool::new(true);

/// How link previews show (off / tooltip / inline; persisted, process-wide).
pub fn link_preview_mode() -> LinkPreviewMode {
    match LINK_PREVIEW_MODE.load(Ordering::Relaxed) {
        x if x == LinkPreviewMode::Off as u8 => LinkPreviewMode::Off,
        x if x == LinkPreviewMode::Inline as u8 => LinkPreviewMode::Inline,
        _ => LinkPreviewMode::Tooltip,
    }
}

/// Whether link-preview thumbnails should be hidden right now: the persisted
/// "hide thumbnails" preference AND streamer mode being active. Read at render
/// time by the tooltip + inline card so they drop the image while live.
pub fn hide_preview_thumbnails() -> bool {
    STREAMER_HIDE_THUMBNAILS.load(Ordering::Relaxed) && crate::streamer_mode::is_active()
}

/// Whether hovering the chat log pauses it (persisted, process-wide).
pub fn pause_chat_on_hover() -> bool {
    PAUSE_CHAT_ON_HOVER.load(Ordering::Relaxed)
}

/// Whether chat renders compactly (less vertical gap between messages;
/// persisted, process-wide). Read per row by `render::render_message`.
pub fn compact_chat() -> bool {
    COMPACT_CHAT.load(Ordering::Relaxed)
}

/// Whether the live status bar (viewer counts) is enabled (a persisted,
/// process-wide preference — see [`Settings::apply_visibility_flags`]).
pub fn show_status_bar() -> bool {
    SHOW_STATUS_BAR.load(Ordering::Relaxed)
}

/// Where the chat-mode bar sits: off, at the top of the chat panel, or above
/// the input box (persisted, process-wide).
pub fn chat_modes_placement() -> ChatModesPlacement {
    match CHAT_MODES_PLACEMENT.load(Ordering::Relaxed) {
        x if x == ChatModesPlacement::Off as u8 => ChatModesPlacement::Off,
        x if x == ChatModesPlacement::Bottom as u8 => ChatModesPlacement::Bottom,
        _ => ChatModesPlacement::Top,
    }
}

/// Whether message timestamps show in the chat log (persisted, process-wide).
pub fn show_timestamps_chat() -> bool {
    SHOW_TIMESTAMPS_CHAT.load(Ordering::Relaxed)
}

/// Whether timestamps show on the events panel's rows (persisted, process-wide).
pub fn show_timestamps_events() -> bool {
    SHOW_TIMESTAMPS_EVENTS.load(Ordering::Relaxed)
}

/// Whether timestamps show on the mentions panel's rows (persisted, process-wide).
pub fn show_timestamps_mentions() -> bool {
    SHOW_TIMESTAMPS_MENTIONS.load(Ordering::Relaxed)
}

/// The process-wide **global** ignore list (the app-wide `ignored_terms`). The
/// shared channel models drop matching messages at ingest, so a globally-ignored
/// message never enters any buffer. Updated live by `BackseaterApp::refresh_ignore`.
/// Per-tab ignore is separate and applied at render (see `ChatView`).
static GLOBAL_IGNORE: RwLock<Option<bks_core::IgnoreList>> = RwLock::new(None);

/// Sets the process-wide global ignore list (called on load + after a settings
/// edit). An empty list means "ignore nothing globally".
pub fn set_global_ignore(ignore: bks_core::IgnoreList) {
    *GLOBAL_IGNORE.write().unwrap() = Some(ignore);
}

/// Whether the message matches the global ignore list (by text or author).
/// Cheap when nothing is ignored.
pub fn global_ignored(msg: &bks_core::Message) -> bool {
    GLOBAL_IGNORE
        .read()
        .unwrap()
        .as_ref()
        .is_some_and(|i| i.matches_message(msg))
}

/// Whether the pinned-message banner is enabled for `platform` (a persisted,
/// process-wide preference — see [`Settings::apply_visibility_flags`]).
/// Platforms without pins default to hidden.
pub fn show_pinned(platform: bks_core::Platform) -> bool {
    match platform {
        bks_core::Platform::Twitch => SHOW_PINNED_TWITCH.load(Ordering::Relaxed),
        bks_core::Platform::Kick => SHOW_PINNED_KICK.load(Ordering::Relaxed),
        _ => false,
    }
}
