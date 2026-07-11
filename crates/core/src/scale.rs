//! Preferred image render scale for the active display.
//!
//! Emotes and badges come in size variants (1x/2x/...). We fetch the one that
//! matches the display's DPI — 1x at 100% scaling, 2x above —
//! so we don't waste bytes + decode + memory on a larger variant than the screen
//! can show. Set once at startup from the window's scale factor; read by the
//! emote providers and the Twitch badge map. Lives in `bks-core` (depended on by
//! everything) so no crate needs a new dependency to consult it.

use std::sync::atomic::{AtomicU8, Ordering};

/// `1` or `2`. Defaults to 1 (100% DPI — the common case).
static PREFERRED_SCALE: AtomicU8 = AtomicU8::new(1);

/// Sets the preferred image scale (clamped to 1..=2). Call once at startup with the
/// window's rounded device-pixel-ratio.
pub fn set_preferred_scale(scale: u8) {
    PREFERRED_SCALE.store(scale.clamp(1, 2), Ordering::Relaxed);
}

/// The preferred image scale (`1` or `2`).
pub fn preferred_scale() -> u8 {
    PREFERRED_SCALE.load(Ordering::Relaxed)
}
