//! Active color-theme flag (dark vs light), readable from any crate.
//!
//! The chat-log palette (backgrounds, highlight tints, name-contrast reference)
//! is picked in the UI's `render` module, which has no access to the kit's
//! `cx.theme()` inside its free functions. So, like [`preferred_scale`], the
//! chosen mode is mirrored into this process-wide flag at startup and whenever
//! the user toggles it; `render` reads it to select its palette. Lives in
//! `bks-core` (depended on by everything) so no crate needs a new dependency.
//!
//! [`preferred_scale`]: crate::preferred_scale

use std::sync::atomic::{AtomicBool, Ordering};

/// `true` for the dark theme (the default), `false` for light.
static DARK_THEME: AtomicBool = AtomicBool::new(true);

/// Sets whether the dark theme is active. Call at startup from the saved setting
/// and again whenever the user switches themes.
pub fn set_dark_theme(dark: bool) {
    DARK_THEME.store(dark, Ordering::Relaxed);
}

/// Whether the dark theme is active.
pub fn is_dark_theme() -> bool {
    DARK_THEME.load(Ordering::Relaxed)
}
