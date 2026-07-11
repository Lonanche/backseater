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

/// The smallest / largest chat font size the UI offers, in pixels.
pub const MIN_FONT_SIZE: f32 = 12.0;
pub const MAX_FONT_SIZE: f32 = 28.0;
/// Default chat font size (matches the previous hard-coded value).
pub const DEFAULT_FONT_SIZE: f32 = 18.0;

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
    /// Whether the tab strip shows the global Mentions tab: a pinned pseudo-tab
    /// collecting every tab's mentions in one feed. Off by default.
    #[serde(default)]
    pub mentions_tab: bool,
}

fn default_font_size() -> f32 {
    DEFAULT_FONT_SIZE
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
            show_7tv_paints: true,
            theme: ThemeChoice::Dark,
            custom_themes: Vec::new(),
            streamer_mode: StreamerModeChoice::Auto,
            show_pinned_twitch: true,
            show_pinned_kick: true,
            mentions_tab: false,
        }
    }
}

impl Settings {
    /// Loads saved settings, falling back to defaults if none are saved.
    pub fn load() -> Self {
        match bks_auth::store::load::<Settings>(STORE_NAME) {
            Ok(Some(s)) => s,
            _ => Settings::default(),
        }
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

    /// Pushes the pinned-banner visibility into the process-wide flags the chat
    /// views render against (same pattern as the theme flag). Call on load and
    /// after every toggle.
    pub fn apply_pinned_visibility(&self) {
        SHOW_PINNED_TWITCH.store(self.show_pinned_twitch, Ordering::Relaxed);
        SHOW_PINNED_KICK.store(self.show_pinned_kick, Ordering::Relaxed);
    }
}

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;

static SHOW_PINNED_TWITCH: AtomicBool = AtomicBool::new(true);
static SHOW_PINNED_KICK: AtomicBool = AtomicBool::new(true);

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

/// Whether `text` matches the global ignore list. Cheap when nothing is ignored.
pub fn global_ignored(text: &str) -> bool {
    GLOBAL_IGNORE
        .read()
        .unwrap()
        .as_ref()
        .is_some_and(|i| i.matches(text))
}

/// Whether the pinned-message banner is enabled for `platform` (a persisted,
/// process-wide preference — see [`Settings::apply_pinned_visibility`]).
/// Platforms without pins default to hidden.
pub fn show_pinned(platform: bks_core::Platform) -> bool {
    match platform {
        bks_core::Platform::Twitch => SHOW_PINNED_TWITCH.load(Ordering::Relaxed),
        bks_core::Platform::Kick => SHOW_PINNED_KICK.load(Ordering::Relaxed),
        _ => false,
    }
}
