//! App asset source: serves our bundled images (Kick standard badges) and
//! delegates everything else to gpui-component's assets.
//!
//! GPUI's `img("some/path")` loads non-URL strings through the registered
//! [`AssetSource`]. Kick's standard badges (mod/vip/og/...) have no public CDN,
//! so we embed the images in the binary and serve them under `kick/badges/<type>.webp`.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString};

/// Embedded Kick badge images, keyed by the path `img()` requests. The `type`
/// matches the `identity.badges[].type` Kick sends in chat events.
macro_rules! kick_badges {
    ($($name:literal),* $(,)?) => {
        &[$((
            concat!("kick/badges/", $name, ".webp"),
            include_bytes!(concat!("../assets/kick/badges/", $name, ".webp")).as_slice(),
        )),*]
    };
}

/// The Twitch platform icon, bundled as a small raster. We do NOT use the remote
/// Twitch-logo SVG: gpui's `img()` SVG path rasterizes an SVG at its *intrinsic*
/// document size × 2 (here 2400×2800 → 4800×5600 = ~105 MB decoded for a 16px icon!),
/// and never frees it. A 64px PNG is ~14 KB decoded.
const TWITCH_ICON: (&str, &[u8]) = (
    "twitch/twitch.png",
    include_bytes!("../assets/twitch/twitch.png"),
);

/// The Kick platform icon, a small bundled raster (a bold green "K"). Same
/// reasoning as [`TWITCH_ICON`]: bundle a small PNG, never the Kick logo SVG.
const KICK_ICON: (&str, &[u8]) = ("kick/kick.png", include_bytes!("../assets/kick/kick.png"));

/// The YouTube platform icon, a small bundled raster (red rounded rect + white
/// play triangle). Same reasoning as [`TWITCH_ICON`]: bundle a small PNG.
const YOUTUBE_ICON: (&str, &[u8]) = (
    "youtube/youtube.png",
    include_bytes!("../assets/youtube/youtube.png"),
);

/// Bundled lucide icons the kit doesn't ship (same ISC icon set), served next
/// to the kit's own `icons/` so both come from matching vector art:
/// `bell-off` (the muted-mention chip toggle) and the moderation-button set.
macro_rules! app_icons {
    ($($name:literal),* $(,)?) => {
        &[$((
            concat!("icons/", $name, ".svg"),
            include_bytes!(concat!("../assets/icons/", $name, ".svg")).as_slice(),
        )),*]
    };
}

const APP_ICONS: &[(&str, &[u8])] = app_icons![
    "bell-off",
    "ban",
    "clock",
    "timer",
    "hourglass",
    "alarm-clock",
    "trash-2",
    "circle-alert",
    "octagon-alert",
    "pin",
    "reply",
    "message-square-off",
];

/// Mod-button icon names the settings editor offers and `render` resolves:
/// short name → the SVG asset path (ours or the kit's). A [`ModButton::icon`]
/// matching a name here draws the vector icon; anything else draws as text.
/// One icon per mod action — ban / timeout / delete / warn / pin, plus
/// monitor + restrict for when those commands exist — with extra timeout and
/// warn variants so several buttons of the same kind (e.g. 10m vs 1h timeouts)
/// stay distinguishable. `clock`/`trash` keep their lucide names because
/// seeded default buttons in saved settings reference them.
pub const MOD_ICONS: &[(&str, &str)] = &[
    ("ban", "icons/ban.svg"),
    ("clock", "icons/clock.svg"),
    ("timer", "icons/timer.svg"),
    ("hourglass", "icons/hourglass.svg"),
    ("alarm", "icons/alarm-clock.svg"),
    ("trash", "icons/trash-2.svg"),
    // Kit-shipped SVG.
    ("warn", "icons/triangle-alert.svg"),
    ("warn-circle", "icons/circle-alert.svg"),
    ("warn-octagon", "icons/octagon-alert.svg"),
    ("pin", "icons/pin.svg"),
    // Kit-shipped SVG.
    ("monitor", "icons/eye.svg"),
    ("restrict", "icons/message-square-off.svg"),
];

/// The SVG asset path for a mod-button icon name, `None` when `name` isn't a
/// known icon (the button face renders the name as text/emoji instead).
pub fn mod_icon_path(name: &str) -> Option<&'static str> {
    MOD_ICONS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, path)| *path)
}

const KICK_BADGES: &[(&str, &[u8])] = kick_badges![
    "bot",
    "broadcaster",
    "founder",
    "moderator",
    "og",
    "sidekick",
    "staff",
    "sub_gifter",
    "subscriber",
    "trainwreckstv",
    "verified",
    "vip",
];

/// Serves bundled app assets, delegating unknown paths to gpui-component.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path == TWITCH_ICON.0 {
            return Ok(Some(Cow::Borrowed(TWITCH_ICON.1)));
        }
        if path == KICK_ICON.0 {
            return Ok(Some(Cow::Borrowed(KICK_ICON.1)));
        }
        if path == YOUTUBE_ICON.0 {
            return Ok(Some(Cow::Borrowed(YOUTUBE_ICON.1)));
        }
        if let Some((_, bytes)) = APP_ICONS.iter().find(|(p, _)| *p == path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        if let Some((_, bytes)) = KICK_BADGES.iter().find(|(p, _)| *p == path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        gpui_component_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        gpui_component_assets::Assets.list(path)
    }
}

/// The asset path for a Kick badge `type`, or `None` if we don't bundle one.
/// Runs per badge per Kick message, so it compares slices instead of building a
/// candidate `String` per bundled badge.
pub fn kick_badge_path(badge_type: &str) -> Option<&'static str> {
    KICK_BADGES.iter().map(|(p, _)| *p).find(|p| {
        p.strip_prefix("kick/badges/")
            .and_then(|rest| rest.strip_suffix(".webp"))
            == Some(badge_type)
    })
}
