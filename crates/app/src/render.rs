//! Maps platform-agnostic [`Message`]s to GPUI elements (Route A: a flex-wrap
//! row of word/emote tokens). All rendering lives here so that swapping to a
//! custom glyph-level element later touches only this file.

use bks_core::{
    Color, Emote, Message, MessageElement, NamePaint, PaintKind, PaintStop, Platform, ReplyParent,
};
use bks_platform::{AutoModStatus, EventKind};
use gpui::prelude::*;
use gpui::{div, img, px, rgb, AnyView, App, FontWeight, MouseButton, SharedString, Window};
use gpui_component::{h_flex, tooltip::Tooltip, v_flex, WindowExt};

use std::sync::atomic::{AtomicU32, Ordering};

use crate::animated_img::animated_img;
use crate::selectable::{SelectableImage, SelectableText, Selection};

/// The chat-log color palette for one theme. All chat rendering reads its colors
/// from the active palette ([`palette`]) instead of fixed constants, so the chat
/// log re-themes (dark ↔ light) along with the kit chrome. Backgrounds are packed
/// `0xRRGGBB`; the name-contrast fix ([`readable_color`]) keys off `chat_bg`.
#[derive(Clone, Copy)]
pub(crate) struct Palette {
    /// The chat-log surface (also the reference the name contrast-fix uses).
    chat_bg: u32,
    /// Link text.
    link: u32,
    /// Timestamp / secondary text.
    timestamp: u32,
    /// Connector notice text (the legacy system line).
    system: u32,
    /// "replying to" context line (also the reply-bar accent, hence crate-visible).
    pub(crate) reply: u32,
    /// Default username color when a chatter has none.
    default_name: u32,
    /// Background tint for a message that mentions the user, plus the accent
    /// bar color drawn on the row's left edge.
    mention_bg: u32,
    mention_accent: u32,
    /// Background tint + label color for a chatter's first message.
    first_message_bg: u32,
    first_message_label: u32,
    /// Background tint + label/accent color for a "Highlight My Message"
    /// channel-point redemption (Twitch's built-in highlight reward).
    highlighted_bg: u32,
    highlighted_label: u32,
    /// Public-event (sub/gift/raid) row tint + text (the pinned banner reuses
    /// them, hence crate-visible).
    pub(crate) event_bg: u32,
    pub(crate) event_text: u32,
    /// Watch-streak row tint + text.
    streak_bg: u32,
    streak_text: u32,
    /// "went live" row tint + text.
    live_bg: u32,
    live_text: u32,
    /// "went offline" row tint + text.
    offline_bg: u32,
    offline_text: u32,
    /// Error notice row tint + text (selectable, copyable).
    error_bg: u32,
    error_text: u32,
    /// AutoMod held-message row tint + text, plus its Allow/Deny action colors.
    automod_bg: u32,
    automod_text: u32,
    automod_allow: u32,
    automod_deny: u32,
    /// The tab strip background (behind the chips).
    tab_bar_bg: u32,
    /// Unselected-tab chip background (a recessed tone; the active tab is filled
    /// with the accent instead).
    tab_inactive_bg: u32,
    /// Floating-panel surface (settings/usercard windows) — an *elevated* tone so a
    /// panel reads as raised above the app, not the kit's flat near-black.
    panel_bg: u32,
}

/// The dark palette: deep neutral with deliberate elevation steps — the log is
/// the darkest surface, chrome (tab bar, input) sits one step lifted above it,
/// panels/popovers another step up. Row-tint colors are kept subtle (the accent
/// bar carries the identity); `*_text` tones are chromatic enough to double as
/// those accent-bar colors.
const DARK: Palette = Palette {
    chat_bg: 0x121214,
    link: 0x58a6ff,
    timestamp: 0x6e6e78,
    system: 0x84a878,
    reply: 0x6e6e78,
    default_name: 0xa06bff,
    mention_bg: 0x2b2310,
    mention_accent: 0xf2b84a,
    first_message_bg: 0x122824,
    first_message_label: 0x3ecfb2,
    highlighted_bg: 0x241d33,
    highlighted_label: 0xb39bff,
    event_bg: 0x221e30,
    event_text: 0xc4b5fd,
    streak_bg: 0x2b2413,
    streak_text: 0xe8c56a,
    live_bg: 0x122718,
    live_text: 0x57d98a,
    offline_bg: 0x1e1e22,
    offline_text: 0x8d8d96,
    error_bg: 0x2d191c,
    error_text: 0xf28b8b,
    automod_bg: 0x2c2113,
    automod_text: 0xe8b366,
    automod_allow: 0x57d98a,
    automod_deny: 0xf28b8b,
    tab_bar_bg: 0x1b1b1f,
    tab_inactive_bg: 0x2a2a30,
    panel_bg: 0x222228,
};

/// The light palette: white log, chrome one step darker, same accent scheme as
/// dark (bar colors deep enough to read on the pale tints).
const LIGHT: Palette = Palette {
    chat_bg: 0xffffff,
    link: 0x0969da,
    timestamp: 0x84848e,
    system: 0x557d47,
    reply: 0x84848e,
    default_name: 0x7c3aed,
    mention_bg: 0xfff4d6,
    mention_accent: 0xc77d0a,
    first_message_bg: 0xe0f5f0,
    first_message_label: 0x0f8570,
    highlighted_bg: 0xece4fb,
    highlighted_label: 0x6d3fd1,
    event_bg: 0xf1ecfe,
    event_text: 0x6d4fc4,
    streak_bg: 0xfcf0cf,
    streak_text: 0x8a6410,
    live_bg: 0xe1f6e7,
    live_text: 0x188038,
    offline_bg: 0xefeff1,
    offline_text: 0x71717a,
    error_bg: 0xfdebeb,
    error_text: 0xc22e2e,
    automod_bg: 0xfdf1dc,
    automod_text: 0x9a6209,
    automod_allow: 0x188038,
    automod_deny: 0xc22e2e,
    tab_bar_bg: 0xe9e9ec,
    tab_inactive_bg: 0xdadadf,
    panel_bg: 0xffffff,
};

/// A user-defined custom palette, when a custom theme is active. `None` means a
/// built-in (dark/light) theme is selected and [`palette`] falls back to the flag.
/// Set from `main.rs::apply_theme`; read on every render, so a plain lock is fine.
static CUSTOM_PALETTE: std::sync::RwLock<Option<Palette>> = std::sync::RwLock::new(None);

/// Installs a user-defined palette as the active theme. Pass `None` to clear it
/// and fall back to the built-in dark/light flag.
pub fn set_custom_palette(p: Option<Palette>) {
    *CUSTOM_PALETTE.write().unwrap() = p;
}

/// The active chat palette. A user-defined custom theme (set via
/// [`set_custom_palette`]) wins; otherwise it's selected by the process-wide
/// dark/light flag. Crate-visible so the pinned banner (built in `chatview.rs`)
/// shares the log's event tint.
pub(crate) fn palette() -> Palette {
    if let Some(p) = *CUSTOM_PALETTE.read().unwrap() {
        return p;
    }
    if bks_core::is_dark_theme() {
        DARK
    } else {
        LIGHT
    }
}

/// The curated set of colors a custom theme lets the user pick, and the base
/// (dark or light) it's derived from. The remaining ~20 palette fields (tab
/// tones, panel surface, timestamps, automod, etc.) are synthesized from the
/// background + base so the editor stays short (see [`Palette::from_custom`]).
#[derive(Clone, Copy)]
pub struct CustomColors {
    pub base_dark: bool,
    pub chat_bg: u32,
    pub default_name: u32,
    pub first_message: u32,
    pub highlighted: u32,
    pub event: u32,
    pub streak: u32,
    pub live: u32,
    pub offline: u32,
    pub mention: u32,
    pub link: u32,
    pub error: u32,
}

impl CustomColors {
    /// The default custom-theme starting point: the dark or light built-in's
    /// curated colors, so "New theme" opens on a sensible palette to tweak.
    pub fn from_base(dark: bool) -> Self {
        let p = if dark { DARK } else { LIGHT };
        Self {
            base_dark: dark,
            chat_bg: p.chat_bg,
            default_name: p.default_name,
            first_message: p.first_message_bg,
            highlighted: p.highlighted_bg,
            event: p.event_bg,
            streak: p.streak_bg,
            live: p.live_bg,
            offline: p.offline_bg,
            mention: p.mention_bg,
            link: p.link,
            error: p.error_bg,
        }
    }
}

impl Palette {
    /// Builds a full palette from the curated custom colors. The user-picked
    /// colors are the row backgrounds; each row's *text* color and the chrome
    /// tones (tab bar/chip/panel, timestamps, system, automod) are derived from
    /// the background so the whole theme stays coherent from ~10 choices. Row
    /// backgrounds are used as picked; text is a high-contrast readable tone.
    pub fn from_custom(c: CustomColors) -> Self {
        let base = if c.base_dark { DARK } else { LIGHT };
        // A readable foreground for a tinted row bg: white-ish on dark, black-ish
        // on light, nudged toward the tint's hue via a light blend so it reads as
        // "of" that color rather than plain grey.
        let fg = |bg: u32, tint: u32| -> u32 {
            let toward = if c.base_dark { 0xf0f0f0 } else { 0x202020 };
            blend(readable_color_on(tint, bg), toward, 0.35)
        };
        // Chrome tones are shades of the chat background: the tab bar and recessed
        // chips are darker on a dark theme / lighter on a light theme; the panel is
        // an elevated (opposite-nudged) tone.
        let shade = |t: f32| blend(c.chat_bg, if c.base_dark { 0x000000 } else { 0xffffff }, t);
        let lift = |t: f32| blend(c.chat_bg, if c.base_dark { 0xffffff } else { 0x000000 }, t);
        Palette {
            chat_bg: c.chat_bg,
            link: c.link,
            timestamp: base.timestamp,
            system: base.system,
            reply: base.reply,
            default_name: c.default_name,
            mention_bg: c.mention,
            mention_accent: fg(c.mention, c.mention),
            first_message_bg: c.first_message,
            first_message_label: fg(c.first_message, c.first_message),
            highlighted_bg: c.highlighted,
            highlighted_label: fg(c.highlighted, c.highlighted),
            event_bg: c.event,
            event_text: fg(c.event, c.event),
            streak_bg: c.streak,
            streak_text: fg(c.streak, c.streak),
            live_bg: c.live,
            live_text: fg(c.live, c.live),
            offline_bg: c.offline,
            offline_text: fg(c.offline, c.offline),
            error_bg: c.error,
            error_text: fg(c.error, c.error),
            automod_bg: base.automod_bg,
            automod_text: base.automod_text,
            automod_allow: base.automod_allow,
            automod_deny: base.automod_deny,
            tab_bar_bg: shade(0.5),
            tab_inactive_bg: shade(0.25),
            panel_bg: lift(0.12),
        }
    }
}

/// Longest URL shown verbatim in the confirmation dialog before truncation.
const URL_PREVIEW_CHARS: usize = 120;

/// Opens a confirmation dialog before navigating to `url`; the browser only
/// opens if the user confirms. Guards against accidental or malicious links.
fn confirm_open_url(url: String, window: &mut Window, cx: &mut App) {
    let mut shown: String = url.chars().take(URL_PREVIEW_CHARS).collect();
    if url.chars().count() > URL_PREVIEW_CHARS {
        shown.push('…');
    }
    // Scheme-less links (`www.x.com`, linkified from chat) must not reach the OS
    // raw: ShellExecute treats a bare string as a file path, not a URL, so the
    // open silently fails (and a crafted string shouldn't get that ambiguity).
    let target = if url.contains("://") {
        url
    } else {
        format!("https://{url}")
    };
    window.open_alert_dialog(cx, move |alert, _, _| {
        let target = target.clone();
        alert
            .confirm()
            .title("Open link?")
            .description(shown.clone())
            .on_ok(move |_, _, cx| {
                cx.open_url(&target);
                true
            })
    });
}

/// The chat-log background for the active theme. Slightly lighter than the kit's
/// near-black (dark theme) so dark usernames (also contrast-fixed, see
/// [`readable_color`]) read better. Used as the chat-log container's background
/// *and* as the reference the name contrast-fix adjusts against.
pub fn chat_bg() -> u32 {
    palette().chat_bg
}

/// The tab strip background (behind the chips), theme-aware.
pub fn tab_bar_bg() -> u32 {
    palette().tab_bar_bg
}

/// The live (green) accent text color, theme-aware — the tab tooltip's "● LIVE"
/// and the tab strip's live dot.
pub fn live_text() -> u32 {
    palette().live_text
}

/// The offline (muted) accent text color, theme-aware.
pub fn offline_text() -> u32 {
    palette().offline_text
}

/// The link text color, theme-aware — also the hover tint for clickable
/// channel names in the tab tooltip.
pub fn link_color() -> u32 {
    palette().link
}

/// The unselected-tab chip background (recessed vs the active tab).
pub fn tab_inactive_bg() -> u32 {
    palette().tab_inactive_bg
}

/// The floating-panel surface (settings/usercard windows) — an elevated tone so a
/// panel reads as raised, not the kit's flat near-black `background`.
pub fn panel_bg() -> u32 {
    palette().panel_bg
}

/// Whether the chat background is dark, memoized on the bg value: the hover
/// tints and panel border ask this per visible row per frame, and `luminance`'s
/// three `powf`s are the expensive part. The packed color and verdict share one
/// atomic so a theme change atomically re-keys the cache.
fn chat_bg_is_dark() -> bool {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CACHE: AtomicU64 = AtomicU64::new(u64::MAX);
    let bg = chat_bg();
    let cached = CACHE.load(Ordering::Relaxed);
    if cached != u64::MAX && (cached >> 32) as u32 == bg {
        return cached & 1 == 1;
    }
    let dark = luminance(bg) < 0.5;
    CACHE.store((u64::from(bg) << 32) | u64::from(dark), Ordering::Relaxed);
    dark
}

/// The subtle full-width tint a chat row gets while hovered. A translucent
/// overlay (not a palette field) so it adapts to any custom theme: a touch of
/// white on a dark log, a touch of black on a light one.
pub(crate) fn row_hover() -> gpui::Hsla {
    if chat_bg_is_dark() {
        gpui::white().opacity(0.04)
    } else {
        gpui::black().opacity(0.04)
    }
}

/// A hover tint for chrome (tab chips, small buttons) — like [`row_hover`] but a
/// step stronger so it reads on the already-lifted chrome surfaces.
pub fn chrome_hover() -> gpui::Hsla {
    if chat_bg_is_dark() {
        gpui::white().opacity(0.07)
    } else {
        gpui::black().opacity(0.06)
    }
}

/// An opaque hover tone for chrome floating over the log (the collapsed-pin
/// chip): the panel surface nudged toward the foreground. The translucent
/// [`chrome_hover`] can't be used there — a hover style *replaces* the base
/// background, so a see-through hover lets the occluded rows bleed through.
pub(crate) fn panel_hover() -> u32 {
    let toward = if chat_bg_is_dark() { 0xffffff } else { 0x000000 };
    blend(palette().panel_bg, toward, 0.07)
}

/// The accent color for the active tab's indicator line, theme-aware.
pub fn accent() -> u32 {
    palette().default_name
}

/// The `(background tint, accent bar)` color pairs for the log's highlighted
/// rows. The chat log paints these on its full-width row wrapper (so the tint
/// bleeds edge-to-edge); panel contexts use the same pairs for their
/// self-contained pills.
pub(crate) fn highlight_mention() -> (u32, u32) {
    let p = palette();
    (p.mention_bg, p.mention_accent)
}

/// The background of a briefly-flashed (jumped-to) row: the flash tone blended
/// over the row's `base` background at `strength` (1.0 = full flash, fading to
/// the base as it eases to 0). The flash tone is the mention accent, so the
/// jumped-to row lights up in the same family as a live mention.
pub(crate) fn flash_over(base: u32, strength: f32) -> u32 {
    let t = strength.clamp(0.0, 1.0) * 0.55;
    blend(base, palette().mention_accent, t)
}

pub(crate) fn highlight_first_message() -> (u32, u32) {
    let p = palette();
    (p.first_message_bg, p.first_message_label)
}

/// The `(background tint, accent bar/label)` for a "Highlight My Message"
/// channel-point redemption.
pub(crate) fn highlight_highlighted() -> (u32, u32) {
    let p = palette();
    (p.highlighted_bg, p.highlighted_label)
}

/// `accent` (a platform-assigned row color — Twitch announcement colors)
/// overrides the kind's accent-bar/label tone; the background tint stays the
/// kind's own so the row still reads as an event on both themes.
pub(crate) fn highlight_event(kind: EventKind, accent: Option<u32>) -> (u32, u32) {
    let p = palette();
    let (bg, fg) = match kind {
        EventKind::WatchStreak => (p.streak_bg, p.streak_text),
        _ => (p.event_bg, p.event_text),
    };
    (bg, accent.unwrap_or(fg))
}

pub(crate) fn highlight_live(live: bool) -> (u32, u32) {
    let p = palette();
    if live {
        (p.live_bg, p.live_text)
    } else {
        (p.offline_bg, p.offline_text)
    }
}

pub(crate) fn highlight_error() -> (u32, u32) {
    let p = palette();
    (p.error_bg, p.error_text)
}

pub(crate) fn highlight_automod() -> (u32, u32) {
    let p = palette();
    (p.automod_bg, p.automod_text)
}

/// A hairline border tone for floating chrome (hover-action pill, overlays):
/// the panel surface nudged toward the foreground so the edge reads on both
/// dark and light themes.
pub(crate) fn panel_border() -> u32 {
    let toward = if chat_bg_is_dark() { 0xffffff } else { 0x000000 };
    blend(palette().panel_bg, toward, 0.14)
}

/// Minimum contrast ratio (WCAG-style, 1..21) a username must clear against the
/// chat background. Below it the name is moved away from the background (lightened
/// on a dark bg, darkened on a light bg) until it passes — BTTV's
/// "readable colors". 3.0 is WCAG's large/bold-text threshold (names render bold);
/// 2.0 let dim blues/reds/purples (pure blue was only ~2.2 on the near-black log)
/// through unadjusted, while 3.0 still preserves the hue rather than washing to
/// near-white the way the AA body-text threshold (4.5) would.
const MIN_NAME_CONTRAST: f32 = 3.0;

/// Relative luminance of a packed `0xRRGGBB` color (WCAG sRGB formula), in 0..1.
fn luminance(color: u32) -> f32 {
    let chan = |c: u32| {
        let s = (c & 0xff) as f32 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    let r = chan(color >> 16);
    let g = chan(color >> 8);
    let b = chan(color);
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// WCAG contrast ratio (1..21) between two packed colors.
fn contrast_ratio(a: u32, b: u32) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Linearly blends each channel of `color` toward `target` by `t` (0..1).
fn blend(color: u32, target: u32, t: f32) -> u32 {
    let mix = |c: u32, tc: u32| {
        let (c, tc) = ((c & 0xff) as f32, (tc & 0xff) as f32);
        (c + (tc - c) * t).round().clamp(0.0, 255.0) as u32
    };
    (mix(color >> 16, target >> 16) << 16)
        | (mix(color >> 8, target >> 8) << 8)
        | mix(color, target)
}

/// Twitch's 15 default username colors — what twitch.tv assigns a chatter who
/// never picked one. Used as the colorless-name fallback, seeded per user
/// (Chatterino's `getRandomColor`), so every colorless chatter keeps a stable
/// identity color instead of all sharing the palette's single default (which
/// stays in use for chrome accents). [`readable_color`] then fixes contrast.
const TWITCH_NAME_COLORS: [u32; 15] = [
    0xff0000, // Red
    0x0000ff, // Blue
    0x00ff00, // Green
    0xb22222, // FireBrick
    0xff7f50, // Coral
    0x9acd32, // YellowGreen
    0xff4500, // OrangeRed
    0x2e8b57, // SeaGreen
    0xdaa520, // GoldenRod
    0xd2691e, // Chocolate
    0x5f9ea0, // CadetBlue
    0x1e90ff, // DodgerBlue
    0xff69b4, // HotPink
    0x8a2be2, // BlueViolet
    0x00ff7f, // SpringGreen
];

/// The stable per-user color for a chatter with no color set: seeded by the
/// numeric user id (like Chatterino/twitch.tv), falling back to a character sum
/// of the id or login for platforms with non-numeric ids.
fn fallback_name_color(author: &bks_core::Author) -> u32 {
    let seed = author.user_id.parse::<u64>().unwrap_or_else(|_| {
        let src = if author.user_id.is_empty() {
            &author.login
        } else {
            &author.user_id
        };
        src.chars().map(|c| c as u64).sum()
    });
    TWITCH_NAME_COLORS[(seed % TWITCH_NAME_COLORS.len() as u64) as usize]
}

/// Returns `color` adjusted just enough to clear [`MIN_NAME_CONTRAST`] against the
/// chat background, so usernames stay readable while keeping their hue (BTTV
/// "Readable Colors"). On a dark background a too-dark name is lightened toward
/// white; on a light background a too-bright name is darkened toward black. Colors
/// that already pass are returned unchanged.
fn readable_color(color: u32) -> u32 {
    readable_color_on(color, chat_bg())
}

/// [`readable_color`] against an explicit background, so the algorithm is testable
/// independent of the active theme. The adjust direction (toward white on a dark
/// bg, toward black on a light bg) is chosen from the background's luminance.
fn readable_color_on(color: u32, bg: u32) -> u32 {
    if contrast_ratio(color, bg) >= MIN_NAME_CONTRAST {
        return color;
    }
    // Move away from the background: toward white if the bg is dark, toward black
    // if it's light. A fixed 12-step ramp is finer than the eye distinguishes.
    let (target, fallback) = if luminance(bg) < 0.5 {
        (0xffffff, 0xffffff)
    } else {
        (0x000000, 0x000000)
    };
    for step in 1..=12 {
        let adj = blend(color, target, step as f32 / 12.0);
        if contrast_ratio(adj, bg) >= MIN_NAME_CONTRAST {
            return adj;
        }
    }
    fallback
}
/// Longest reply-parent body shown inline before it's truncated with an ellipsis.
const REPLY_PREVIEW_CHARS: usize = 80;

/// The font size the sizes below were tuned against; everything scales relative
/// to the user's chosen size so emotes/icons/badges/timestamps stay proportional
/// (a smaller font gives a compact row, not big icons next to small text).
const BASELINE_FONT: f32 = 18.0;
const EMOTE_HEIGHT: f32 = 26.0;
const PLATFORM_ICON_SIZE: f32 = 16.0;
const BADGE_SIZE: f32 = 18.0;

/// Extra leading added to the line box on top of the font's ascent + descent, as
/// a fraction of the font size. The row body applies the resulting line height to
/// every text token via `.line_height()`, overriding gpui's 1.618 default so
/// wrapped lines within a message sit tight (see `Scale::new`).
const LINE_LEADING: f32 = 0.1;

/// Fallback line-height factor used before the font's real metrics have been
/// published (a touch above 1.0, close to a typical UI font's metrics height).
const LINE_HEIGHT_FALLBACK: f32 = 1.35;

/// Vertical metrics of the active UI font, as per-em ratios ×1000 (atomics so
/// the render path reads them lock-free — same process-wide pattern as
/// `bks_core::is_dark_theme`). Published by `main.rs::apply_font` at launch and
/// on every font change; the defaults are Segoe UI's metrics in case anything
/// renders before the first publish.
static FONT_ASCENT: AtomicU32 = AtomicU32::new(1079);
static FONT_DESCENT: AtomicU32 = AtomicU32::new(252);
static FONT_CAP_HEIGHT: AtomicU32 = AtomicU32::new(700);

/// Publishes the active font's vertical metrics (per-em ratios) for row layout.
/// A missing cap height (not every font carries one) falls back to a typical
/// 0.7 em.
pub fn set_font_metrics(ascent: f32, descent: f32, cap_height: f32) {
    let store =
        |slot: &AtomicU32, v: f32| slot.store((v * 1000.0).round() as u32, Ordering::Relaxed);
    store(&FONT_ASCENT, ascent);
    store(&FONT_DESCENT, descent);
    store(
        &FONT_CAP_HEIGHT,
        if cap_height > 0.0 { cap_height } else { 0.7 },
    );
}

fn font_metric(slot: &AtomicU32) -> f32 {
    slot.load(Ordering::Relaxed) as f32 / 1000.0
}

/// All sizes for one row, derived from the active chat font size.
#[derive(Clone, Copy)]
struct Scale {
    font: f32,
    emote: f32,
    icon: f32,
    badge: f32,
    /// Secondary text (system notices, hover chips), kept a touch smaller.
    small: f32,
    /// One text line box's height (gpui rounds it to whole pixels). Prefix items
    /// (icon/time/badges) sit in a box of this exact height so they share the
    /// text tokens' box top and line up with the body text.
    line: f32,
    /// The text baseline's offset from a line box's top: half-leading + ascent,
    /// exactly how gpui positions a shaped line inside its box (`line.rs::paint`).
    baseline: f32,
    /// Cap height at the full font size — how far digits/capitals reach above
    /// the baseline. Their band's middle is the text's optical center.
    cap: f32,
}

impl Scale {
    fn new(font_size: f32) -> Self {
        let factor = font_size / BASELINE_FONT;
        let ascent = font_metric(&FONT_ASCENT) * font_size;
        let descent = font_metric(&FONT_DESCENT) * font_size;
        // Line box sized to the font's real metrics height (ascent + descent)
        // plus a little leading, rounded to whole pixels like gpui does. The body
        // applies this via `.line_height()` so text tokens shape at it (not gpui's
        // 1.618 default), and the prefix boxes (icon/time/badges) use the same
        // value so they stay aligned with the body text. Falls back to a fixed
        // factor before font metrics are published.
        let metrics_line = ascent + descent + font_size * LINE_LEADING;
        let line = if metrics_line > 0.0 {
            metrics_line.round()
        } else {
            (font_size * LINE_HEIGHT_FALLBACK).round()
        };
        Self {
            font: font_size,
            emote: EMOTE_HEIGHT * factor,
            icon: PLATFORM_ICON_SIZE * factor,
            badge: BADGE_SIZE * factor,
            small: font_size * 0.72,
            line,
            baseline: (line - ascent - descent) / 2.0 + ascent,
            cap: font_metric(&FONT_CAP_HEIGHT) * font_size,
        }
    }
}

/// Places a text prefix item (the timestamp, a glyph badge) in a box exactly one
/// text line tall. Full-size text fills the box, so it shares the body text's
/// baseline with no nudging; images go through [`image_line_box`] instead.
fn line_box(scale: Scale) -> gpui::Div {
    h_flex().h(px(scale.line)).items_center()
}

/// Places a prefix image (platform icon / badge) of height `image_h` in a
/// one-line-tall box with its center on the text's *optical* center — the
/// middle of the cap band (baseline − cap/2), where digits and capitals center.
/// Geometric centering (`items_center`) sat images ~1px off the glyphs, because
/// fonts reserve uneven ascent/descent space around
/// them. The offset lands on whole pixels — a fractional position blurs the
/// image, which also read as misalignment — rounded *down* (up-bias): measured
/// against screenshots, nearest-rounding sat icons ~1px below the digits'
/// center, and an image next to descender-less digits reads better a hair high
/// than a hair low.
fn image_line_box(scale: Scale, image_h: f32) -> gpui::Div {
    let top = (scale.baseline - (scale.cap + image_h) / 2.0)
        .floor()
        .max(0.0);
    h_flex().h(px(scale.line)).items_start().pt(px(top))
}

/// The platform indicator shown before each message, already in its line box:
/// the platform's real logo when it has one ([`Platform::icon_url`]), otherwise
/// a brand-colored glyph. Sized from the row's [`Scale`] so it tracks the font.
fn platform_badge(platform: Platform, scale: Scale) -> gpui::Div {
    // All platforms share one fixed-width slot (the widest logo's width at this
    // scale) with the logo centered inside, so the timestamp after the icon
    // starts at the same x on every row regardless of which platform's logo
    // (they differ in aspect) precedes it.
    let slot = px(Platform::icon_slot_width(scale.icon));
    match platform.icon_url() {
        Some(url) => {
            let (w, h) = platform.icon_size(scale.icon);
            image_line_box(scale, h).w(slot).justify_center().child(
                img(SharedString::from(url))
                    .id(SharedString::from(platform.label()))
                    .h(px(h))
                    .w(px(w)),
            )
        }
        None => line_box(scale).w(slot).justify_center().child(
            div()
                .rounded_sm()
                .text_size(px(scale.small))
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(platform.color().to_u32()))
                .child(SharedString::from(platform.glyph())),
        ),
    }
}

/// Height of the large emote/badge preview image shown at the top of a hover
/// tooltip. Fixed (not font-scaled) — it's a popover, not a
/// row item.
const TOOLTIP_PREVIEW_HEIGHT: f32 = 72.0;
const TOOLTIP_BADGE_PREVIEW_HEIGHT: f32 = 36.0;

/// The hover text for an emote: its name, then "<provider>
/// Emote", then "By: <author>" when known. Lines are omitted when their fact is
/// missing (e.g. native emotes have no author).
fn emote_tooltip_text(emote: &Emote) -> String {
    let mut lines = vec![emote.name.clone()];
    if !emote.tooltip.provider.is_empty() {
        lines.push(format!("{} Emote", emote.tooltip.provider));
    }
    if let Some(author) = &emote.tooltip.author {
        lines.push(format!("By: {author}"));
    }
    lines.join("\n")
}

/// Builds a hover tooltip view: a large preview image of `image_url` above
/// `text` (one or more lines). Used for both emotes and badges; `preview_height`
/// sizes the image. The closure form lets gpui rebuild it per show. The preview
/// is an [`animated_img`] (a stable id keys its frame state), so animated
/// emotes play in the tooltip too.
fn image_tooltip(
    image_url: SharedString,
    text: SharedString,
    preview_height: f32,
    window: &mut Window,
    cx: &mut App,
) -> AnyView {
    Tooltip::element(move |_, _| {
        v_flex()
            .gap_1()
            .items_center()
            .child(animated_img(
                SharedString::from(format!("tip:{image_url}")),
                image_url.clone(),
                px(preview_height),
            ))
            .child(text.clone())
    })
    .build(window, cx)
}

/// Opacity applied to a removed (banned/timed-out/deleted) message.
const STRUCK_OPACITY: f32 = 0.45;

/// A word longer than this (chars) is allowed to wrap mid-word so it can't
/// overflow the row; shorter words keep their width and wrap only between words.
const LONG_WORD_CHARS: usize = 24;

/// Opacity for backfilled chat-history rows, so they read as older than live chat.
pub(crate) const HISTORY_OPACITY: f32 = 0.6;

/// Negative right margin (px) applied to each word token that ends in whitespace,
/// to tighten the visible inter-word gap. Words carry their own trailing space
/// glyph (so copy stays exact), but a full space reads too wide in the flex-wrap
/// row; this pulls the following token left to shave the gap without touching
/// letters *within* a word or breaking selection. One value for every body
/// surface — chat log, mentions panel, automod preview, event rows, and reply
/// previews all key on this (via `text_token` and `inline_tokens`).
const WORD_TIGHTEN: f32 = 1.0;


/// Horizontal padding a highlighted row's tinted pill gets (`px_2`, 8px), so its
/// content has breathing room inside the rounded box. An equal *negative* margin
/// cancels it, so the pill bleeds back to the row's content edge and the
/// highlighted text still lines up with normal (un-highlighted) rows — the tint
/// just extends this far past the text on each side, reading as a floating pill.
const HIGHLIGHT_INSET: f32 = 8.0;

/// Per-message element-id namespaces. gpui ids only need to be unique among
/// siblings, but a row mixes tokens, badges, emote images, and hover actions
/// under one flex row, so each kind's index is folded into a disjoint slice of
/// the integer key (a high kind-tag, [`RowIds::keyed`]) rather than getting its
/// own string prefix. That leaves a single shared base string (`msg.id`) —
/// built once per row render, then cloned per element as cheap `Arc` bumps
/// instead of a `format!` allocation per kind. `render_message` runs per
/// visible row per repaint (animated emotes force ~20ms frames), so this is a
/// hot path. Emote image ids must stay stable across frames or GPUI won't
/// animate them — `msg.id` is stable and so is the per-message index.
struct RowIds {
    base: SharedString,
}

/// Kind tags folded into the top bits of an element-id integer so all the id
/// kinds share one base string yet never collide (tokens/badges/emotes are
/// siblings in the body row; pin/reply chips are siblings in the actions box).
/// Tags sit in the top nibble relative to the target's word size (so a 32-bit
/// build still compiles; indices max out around 2^26 — `MAX_ROWS` ×
/// `ORDINAL_STRIDE` — well below either boundary). Values are arbitrary but
/// must stay distinct + stable across frames. `(SharedString, usize)` reuses
/// gpui's existing `ElementId::NamedInteger` conversion.
const KIND_TOKEN: usize = 0;
const KIND_BADGE: usize = 1 << (usize::BITS - 4);
const KIND_EMOTE: usize = 2 << (usize::BITS - 4);
const KIND_PIN: usize = 3 << (usize::BITS - 4);
const KIND_REPLY: usize = 4 << (usize::BITS - 4);
const KIND_MOD: usize = 5 << (usize::BITS - 4);

impl RowIds {
    /// Call sites with an owned `String` (the `format!` bases of error/automod
    /// rows) move it in; `render_message` clones `msg.id` (its one per-row
    /// allocation). Either way the base is allocated exactly once per row.
    fn new(base: impl Into<SharedString>) -> Self {
        Self { base: base.into() }
    }

    /// An element id in the given kind's namespace for `index`, sharing the row's
    /// base string (a cheap `Arc` bump). `(SharedString, usize)` →
    /// `ElementId::NamedInteger`, the same shape used elsewhere, pre-tagged by kind.
    fn keyed(&self, kind: usize, index: usize) -> (SharedString, usize) {
        (self.base.clone(), kind | index)
    }

    fn token(&self, index: usize) -> (SharedString, usize) {
        self.keyed(KIND_TOKEN, index)
    }

    fn badge(&self, index: usize) -> (SharedString, usize) {
        self.keyed(KIND_BADGE, index)
    }

    fn emote(&self, index: usize) -> (SharedString, usize) {
        self.keyed(KIND_EMOTE, index)
    }

    fn pin(&self) -> (SharedString, usize) {
        self.keyed(KIND_PIN, 0)
    }

    fn mod_button(&self, index: usize) -> (SharedString, usize) {
        self.keyed(KIND_MOD, index)
    }

    fn reply(&self) -> (SharedString, usize) {
        self.keyed(KIND_REPLY, 0)
    }
}

/// The per-row context threaded through the token builders: the row's stable ids
/// and the shared drag-select state. Bundled so the builders don't pass them as
/// two separate args everywhere (the running `ordinal` stays a `&mut` param since
/// it spans rows). One is created per message in [`render_message`].
struct RenderCtx<'a> {
    ids: &'a RowIds,
    selection: &'a Selection,
}

/// Weight for author names and @mentions. Chatterino's default username weight
/// is DemiBold (its `boldScale` 63 → OpenType 600), not full Bold — 700 makes
/// names pop harder than the rest of the line than intended.
const NAME_WEIGHT: FontWeight = FontWeight::SEMIBOLD;

/// Builds one selectable text token. Claims the next `ordinal` and tints/bolds
/// via the wrapping div, which the text inherits. Used for the name and for each
/// word of a body run (see [`push_text_words`]).
fn text_token(
    ctx: &RenderCtx,
    ordinal: &mut usize,
    text: String,
    color: Option<u32>,
    bold: bool,
    starts_row: bool,
) -> gpui::AnyElement {
    // Body words carry a trailing space glyph, which reads too wide; tighten it.
    let tighten = text.ends_with(char::is_whitespace);
    let ord = *ordinal;
    *ordinal += 1;
    let token = SelectableText::new(
        ctx.ids.token(ord),
        ord,
        text,
        ctx.selection.clone(),
    )
    .starts_row(starts_row);
    let mut wrap = div();
    if let Some(c) = color {
        wrap = wrap.text_color(rgb(c));
    }
    if bold {
        wrap = wrap.font_weight(NAME_WEIGHT);
    }
    if tighten {
        wrap = wrap.mr(px(-WORD_TIGHTEN));
    }
    wrap.child(token).into_any_element()
}

/// Extracts the 7TV emote id from a `7tv.app/emotes/<id>` URL (or the legacy
/// `old.7tv.app`), trimming a trailing slash / query / fragment. Returns `None`
/// for any other URL, so only real 7TV emote links open the in-app popup. The id
/// itself is whatever 7TV uses; we don't validate its shape, just hand it to the
/// REST lookup which normalizes it.
fn seventv_emote_id(url: &str) -> Option<String> {
    // Accept with or without scheme / www / "old." subdomain.
    let rest = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("www.")
        .trim_start_matches("old.");
    let after = rest.strip_prefix("7tv.app/emotes/")?;
    // The id ends at the first `/`, `?`, or `#`.
    let id: String = after
        .chars()
        .take_while(|c| !matches!(c, '/' | '?' | '#'))
        .collect();
    (!id.is_empty()).then_some(id)
}

/// Whether a word looks like a URL we should render as a clickable link.
fn is_url(word: &str) -> bool {
    word.starts_with("https://") || word.starts_with("http://")
}

/// Splits a body text run into per-word tokens and pushes each into `tokens`.
///
/// The flex-wrap row breaks between flex items, so one token per run can't wrap a
/// long run — splitting into words lets it wrap at word boundaries at any width.
/// Each chunk keeps its trailing whitespace (and a leading-whitespace run keeps
/// it on its first chunk), so concatenating the tokens reproduces the run exactly
/// and copy/selection stay correct. None of these start a row (only the name does).
fn push_text_words(
    tokens: &mut Vec<gpui::AnyElement>,
    ctx: &RenderCtx,
    ordinal: &mut usize,
    text: &str,
    color: Option<u32>,
) {
    for chunk in split_words(text) {
        // A very long word (a no-space blob like a pasted URL/token) is hard-broken
        // into row-width-ish pieces so it wraps instead of overflowing; normal
        // words pass through whole. Each piece is its own token, but they keep the
        // original characters, so copy/selection still reproduce the word exactly.
        if chunk.trim_end().chars().count() > LONG_WORD_CHARS {
            for piece in break_long_word(chunk) {
                tokens.push(text_token(ctx, ordinal, piece, color, false, false));
            }
        } else {
            tokens.push(text_token(
                ctx,
                ordinal,
                chunk.to_string(),
                color,
                false,
                false,
            ));
        }
    }
}

/// Pushes a body text run, detecting URLs word-by-word: a `7tv.app/emotes/<id>`
/// link becomes a clickable token that opens the emote popup in-app; any other
/// `http(s)` URL becomes a normal (confirm-then-open) link; everything else is
/// plain text. Falls back to [`push_text_words`] (plain text only) when no link
/// handlers are supplied (e.g. the usercard's message list isn't interactive).
#[allow(clippy::too_many_arguments)] // Render helper threading per-row handlers.
fn push_run(
    tokens: &mut Vec<gpui::AnyElement>,
    ctx: &RenderCtx,
    ordinal: &mut usize,
    text: &str,
    color: Option<u32>,
    seventv_link_click: Option<&SeventvLinkClick>,
    link_hover: Option<&LinkHover>,
    link_preview_hover: Option<&LinkPreviewHover>,
) {
    // Without link handlers there's nothing interactive to build — keep it plain.
    if seventv_link_click.is_none() && link_hover.is_none() {
        return push_text_words(tokens, ctx, ordinal, text, color);
    }
    for chunk in split_words(text) {
        // Separate the word from its trailing whitespace so a URL link doesn't
        // swallow the space (copy stays exact: the space is re-emitted as text).
        let word = chunk.trim_end();
        let trailing = &chunk[word.len()..];

        if let (Some(cb), Some(id)) = (seventv_link_click, seventv_emote_id(word)) {
            tokens.push(seventv_link_token(ctx, ordinal, word, id, cb.clone()));
        } else if is_url(word) {
            push_link(tokens, ctx, ordinal, word, word, link_hover, link_preview_hover);
        } else if word.chars().count() > LONG_WORD_CHARS {
            for piece in break_long_word(word) {
                tokens.push(text_token(ctx, ordinal, piece, color, false, false));
            }
        } else if !word.is_empty() {
            tokens.push(text_token(
                ctx,
                ordinal,
                word.to_string(),
                color,
                false,
                false,
            ));
        }
        // Re-emit the trailing whitespace as its own text token.
        if !trailing.is_empty() {
            tokens.push(text_token(
                ctx,
                ordinal,
                trailing.to_string(),
                color,
                false,
                false,
            ));
        }
    }
}

/// A 7TV emote *link* in chat: a blue, clickable, hover-underlined token (like a
/// normal link) that, on a plain click, opens the emote popup for `emote_id`
/// instead of navigating. Selectable so it copies as the original URL text.
fn seventv_link_token(
    ctx: &RenderCtx,
    ordinal: &mut usize,
    text: &str,
    emote_id: String,
    on_click: SeventvLinkClick,
) -> gpui::AnyElement {
    let ord = *ordinal;
    *ordinal += 1;
    let token = SelectableText::new(
        ctx.ids.token(ord),
        ord,
        text.to_string(),
        ctx.selection.clone(),
    );
    let sel = ctx.selection.clone();
    div()
        .id(ctx.ids.token(ord))
        .text_color(rgb(palette().link))
        .cursor_pointer()
        .hover(|s| s.underline())
        .on_mouse_up(
            MouseButton::Left,
            move |ev: &gpui::MouseUpEvent, window, cx| {
                if !sel.has_selection() {
                    on_click(&emote_id, ev.position, window, cx);
                }
            },
        )
        .child(token)
        .into_any_element()
}

/// Hard-splits an over-long, space-less word into chunks of at most
/// [`LONG_WORD_CHARS`] characters (on char boundaries), so the flex row can wrap
/// between the pieces. Concatenating the pieces yields the original word, so copy
/// stays exact. Any trailing whitespace rides along on the last piece.
fn break_long_word(word: &str) -> Vec<String> {
    let chars: Vec<char> = word.chars().collect();
    chars
        .chunks(LONG_WORD_CHARS)
        .map(|c| c.iter().collect())
        .collect()
}

/// Splits `text` into chunks, each a word plus its trailing whitespace (a leading
/// whitespace stretch stays attached to the first word). Concatenating the chunks
/// yields the original string. Empty input yields nothing.
fn split_words(text: &str) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut seen_word = false; // saw a non-space since the last boundary
    let mut prev_space = false;
    for (i, c) in text.char_indices() {
        let space = c.is_whitespace();
        // Boundary: a non-space that follows whitespace, once we've had a word —
        // close the previous chunk (which carries its trailing spaces) here.
        if !space && prev_space && seen_word {
            chunks.push(&text[start..i]);
            start = i;
            seen_word = false;
        }
        seen_word |= !space;
        prev_space = space;
    }
    if start < text.len() {
        chunks.push(&text[start..]);
    }
    chunks
}

/// A callback run when a chatter's name is clicked (a plain click, not the end of
/// a drag-select). Built per-message by the view, closing over the view handle and
/// the clicked message's id (the click resolves the chatter from that); here we
/// only move it into the click handler and invoke it.
pub type NameClick = Box<dyn Fn(&mut Window, &mut App)>;

/// A callback run when a row's author name is *right*-clicked: inserts `@name ` into
/// the composer (tagging that chatter) and switches the send target to their
/// platform. Built per-row by the view, capturing the name + platform; `None`
/// outside the live log.
pub type NameRightClick = Box<dyn Fn(&mut Window, &mut App)>;

/// The author-name token. Like [`text_token`] (selectable, bold, colored) but,
/// when `click` is set, also a hover-highlighted click target that fires `click`
/// on a plain click — guarded by `selection.has_selection()` so the click that
/// ends a drag-select doesn't open the card.
///
/// With a 7TV `paint`, the name is rendered as a (per-character) gradient instead
/// of one flat color — GPUI fills glyphs with a solid color, so a gradient name is
/// approximated by coloring each character from its position along the gradient.
/// Each character stays its own selectable token, so copy still yields the name.
fn name_token(
    ctx: &RenderCtx,
    ordinal: &mut usize,
    text: String,
    color: u32,
    paint: Option<&NamePaint>,
    click: Option<NameClick>,
    right_click: Option<NameRightClick>,
) -> gpui::AnyElement {
    // Trailing margin gives the gap to the first body word (the row has no gap).
    let base = div().mr_1().font_weight(NAME_WEIGHT);
    // The click/hover target wraps the name; it needs a stable id (the name's
    // first ordinal) when clickable.
    let click_ord = *ordinal;

    let inner: gpui::AnyElement = match paint {
        // A flat (or no) paint: one token, fast path.
        Some(NamePaint {
            kind: PaintKind::Solid(c),
            ..
        }) => paint_name_solid(ctx, ordinal, &text, *c),
        Some(NamePaint {
            kind: PaintKind::Linear { stops, .. },
            ..
        })
        | Some(NamePaint {
            kind: PaintKind::Radial { stops },
            ..
        }) => paint_name_gradient(ctx, ordinal, &text, stops, color),
        None => {
            let ord = *ordinal;
            *ordinal += 1;
            SelectableText::new(
                ctx.ids.token(ord),
                ord,
                text,
                ctx.selection.clone(),
            )
            .starts_row(true)
            .into_any_element()
        }
    };
    // A flat name carries its color on the wrapper (so the whole token inherits it);
    // a gradient colors each char itself, so the wrapper stays uncolored.
    let base = match paint {
        None => base.text_color(rgb(color)),
        Some(NamePaint {
            kind: PaintKind::Solid(c),
            ..
        }) => base.text_color(rgb(*c)),
        Some(_) => base,
    };

    // No interaction at all (usercard list): the plain, uncoloured wrapper.
    if click.is_none() && right_click.is_none() {
        return base.child(inner).into_any_element();
    }
    let sel = ctx.selection.clone();
    // Underline via a bottom border, not text-decoration: a gradient name is
    // a row of per-character boxes, and an inherited `underline()` draws under
    // each box separately, leaving gaps between letters. A border on this
    // wrapper spans the whole name as one unbroken line. Colored to match the
    // name (the base color for a gradient), transparent until hovered.
    let mut wrapper = base
        .id(ctx.ids.token(click_ord))
        .cursor_pointer()
        .border_b_1()
        .border_color(gpui::transparent_black())
        .hover(move |s| s.border_color(rgb(color)));
    if let Some(click) = click {
        let sel = sel.clone();
        wrapper = wrapper.on_mouse_up(MouseButton::Left, move |_, window, cx| {
            if !sel.has_selection() {
                click(window, cx);
            }
        });
    }
    if let Some(right_click) = right_click {
        // Right-click tags the chatter; a drag-select that ends on the name must
        // not fire it (same guard as the left click).
        wrapper = wrapper.on_mouse_up(MouseButton::Right, move |_, window, cx| {
            if !sel.has_selection() {
                right_click(window, cx);
            }
        });
    }
    wrapper.child(inner).into_any_element()
}

/// Builds the name as a single selectable token in flat color `c` (the solid-paint
/// path; the wrapper also sets the color so a clickable wrapper inherits it).
fn paint_name_solid(ctx: &RenderCtx, ordinal: &mut usize, text: &str, _c: u32) -> gpui::AnyElement {
    let ord = *ordinal;
    *ordinal += 1;
    SelectableText::new(
        ctx.ids.token(ord),
        ord,
        text.to_string(),
        ctx.selection.clone(),
    )
    .starts_row(true)
    .into_any_element()
}

/// Builds the name as a horizontal run of per-character selectable tokens, each
/// colored by sampling `stops` at the character's fractional position across the
/// name — approximating a gradient fill (GPUI fills glyphs with a flat color).
/// `fallback` colors the name if the stop list is empty. The first character marks
/// the row start (for selection ordering).
fn paint_name_gradient(
    ctx: &RenderCtx,
    ordinal: &mut usize,
    text: &str,
    stops: &[PaintStop],
    fallback: u32,
) -> gpui::AnyElement {
    let chars: Vec<char> = text.chars().collect();
    let last = chars.len().saturating_sub(1).max(1) as f32;
    let mut spans: Vec<gpui::AnyElement> = Vec::with_capacity(chars.len());
    for (i, ch) in chars.into_iter().enumerate() {
        let ord = *ordinal;
        *ordinal += 1;
        let t = i as f32 / last;
        let color = sample_gradient(stops, t).unwrap_or(fallback);
        let token = SelectableText::new(
            ctx.ids.token(ord),
            ord,
            ch.to_string(),
            ctx.selection.clone(),
        )
        .starts_row(i == 0);
        spans.push(div().text_color(rgb(color)).child(token).into_any_element());
    }
    // `h_flex` keeps the characters on one line; the name never wraps mid-word.
    h_flex().children(spans).into_any_element()
}

/// Samples a gradient defined by `stops` (sorted by `at`, positions in `0..=1`) at
/// position `t`, linearly interpolating between the two surrounding stops. Returns
/// `None` if there are no stops. Clamps to the first/last stop outside their range.
fn sample_gradient(stops: &[PaintStop], t: f32) -> Option<u32> {
    let first = stops.first()?;
    if t <= first.at {
        return Some(first.color);
    }
    let last = stops.last()?;
    if t >= last.at {
        return Some(last.color);
    }
    for pair in stops.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if t >= a.at && t <= b.at {
            let span = (b.at - a.at).max(f32::EPSILON);
            let frac = (t - a.at) / span;
            return Some(lerp_color(a.color, b.color, frac));
        }
    }
    Some(last.color)
}

/// Linearly interpolates each channel between two packed `0xRRGGBB` colors.
fn lerp_color(a: u32, b: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |sa: u32, sb: u32| {
        let (ca, cb) = ((sa & 0xff) as f32, (sb & 0xff) as f32);
        (ca + (cb - ca) * t).round().clamp(0.0, 255.0) as u32
    };
    (mix(a >> 16, b >> 16) << 16) | (mix(a >> 8, b >> 8) << 8) | mix(a, b)
}

/// A callback run when the cursor enters/leaves a link piece, so the view can
/// re-render (the whole link's underline depends on shared hover state). Built
/// per-message by the view; `None` outside the live log (e.g. the usercard list).
pub type LinkHover = std::rc::Rc<dyn Fn(&mut App)>;

/// A callback run when the cursor enters (`true`) or leaves (`false`) a link,
/// with the link's URL and the pointer position — drives the hover link-preview
/// (fetch + tooltip). Built per-message by the view; `None` outside the live log
/// or when previews are off.
pub type LinkPreviewHover =
    std::rc::Rc<dyn Fn(&str, bool, gpui::Point<gpui::Pixels>, &mut Window, &mut App)>;

/// Pushes a link as one or more selectable, clickable tokens: blue, clickable
/// (a plain click opens a confirmation dialog before navigating), and underlined
/// while hovered. A long URL is hard-split into row-width-ish pieces so it wraps
/// instead of overflowing or collapsing to one char per line; the pieces share a
/// link id so hovering any one underlines them all, and they concatenate to the
/// full link text so copy stays exact.
fn push_link(
    tokens: &mut Vec<gpui::AnyElement>,
    ctx: &RenderCtx,
    ordinal: &mut usize,
    url: &str,
    text: &str,
    on_hover: Option<&LinkHover>,
    preview_hover: Option<&LinkPreviewHover>,
) {
    let pieces = if text.chars().count() > LONG_WORD_CHARS {
        break_long_word(text)
    } else {
        vec![text.to_string()]
    };
    // All pieces of this link share an id (the first piece's ordinal) so the
    // shared hover state underlines the whole link, not just the hovered piece.
    let link_id = *ordinal as u64;
    for piece in pieces {
        let ord = *ordinal;
        *ordinal += 1;
        let token = SelectableText::new(
            ctx.ids.token(ord),
            ord,
            piece,
            ctx.selection.clone(),
        );
        let url = url.to_string();
        let sel = ctx.selection.clone();
        let hovered = ctx.selection.is_link_hovered(link_id);
        let hover_sel = ctx.selection.clone();
        let on_hover = on_hover.cloned();
        let preview_hover = preview_hover.cloned();
        let preview_url = url.clone();
        tokens.push(
            div()
                .id(ctx.ids.token(ord))
                .text_color(rgb(palette().link))
                .cursor_pointer()
                .when(hovered, |s| s.underline())
                .on_hover(move |entered, window, cx| {
                    let id = entered.then_some(link_id);
                    if hover_sel.set_hovered_link(id) {
                        if let Some(cb) = &on_hover {
                            cb(cx); // ask the view to repaint with the new state
                        }
                    }
                    // Drive the link preview (fetch + tooltip) on enter/leave.
                    if let Some(cb) = &preview_hover {
                        cb(&preview_url, *entered, window.mouse_position(), window, cx);
                    }
                })
                .on_mouse_up(MouseButton::Left, move |_, window, cx| {
                    // Ignore the click that ends a drag-select.
                    if !sel.has_selection() {
                        confirm_open_url(url.clone(), window, cx);
                    }
                })
                .child(token)
                .into_any_element(),
        );
    }
}

/// A callback run when an emote in a message is clicked: opens an info popup for
/// it. Built per-message by the view; given the clicked emote and the pointer
/// position (so the popup can anchor near the emote). `None` outside the live log.
pub type EmoteClick = std::rc::Rc<dyn Fn(&Emote, gpui::Point<gpui::Pixels>, &mut Window, &mut App)>;

/// A callback run when a 7TV emote *link* in chat text is clicked: given the
/// emote id parsed from the URL and the pointer position, the view fetches the
/// emote and opens its popup. `None` outside the live log.
pub type SeventvLinkClick =
    std::rc::Rc<dyn Fn(&str, gpui::Point<gpui::Pixels>, &mut Window, &mut App)>;

/// A callback run when an `@name` mention in a message body is clicked, with the
/// mentioned name: opens that user's usercard on the row's platform. Whether the
/// name is a real user is unknowable from the text (any word is a legal username
/// shape), so every mention is clickable and the card's stats lookup answers it.
/// Shared by all mentions in the row; `None` outside the live log.
pub type MentionClick = std::rc::Rc<dyn Fn(&str, &mut Window, &mut App)>;

/// A callback run when a row's reply button is clicked: starts a reply to that
/// message. Built per-row by the view, capturing the message's reply identity.
/// `None` outside the live log (and on non-message rows).
pub type ReplyClick = std::rc::Rc<dyn Fn(&mut Window, &mut App)>;

/// A callback run when a row's pin button is clicked: pins that message to the
/// top of its platform's chat. Built per-row by the view, only when the
/// logged-in user can moderate the row's platform; `None` hides the button.
pub type PinClick = std::rc::Rc<dyn Fn(&mut Window, &mut App)>;

/// A callback run when a reply's "↪ replying to" context line is clicked: opens
/// the thread panel showing the whole reply chain, anchored at the click position
/// (window coords). Built per-row by the view for reply messages in the live log;
/// `None` leaves the context line inert.
pub type ThreadClick = std::rc::Rc<dyn Fn(gpui::Point<gpui::Pixels>, &mut Window, &mut App)>;

/// The shared shape of a row's hover-action callbacks ([`ReplyClick`],
/// [`PinClick`]), for the chip builder.
type RowAction = std::rc::Rc<dyn Fn(&mut Window, &mut App)>;

/// A callback run when one of a row's moderation buttons is clicked, with that
/// button's command template ("/timeout {user} 600"). The view substitutes the
/// placeholders from the row's message and dispatches at the row's platform.
/// Built per-row, only when the user can moderate the row's platform.
pub type ModClick = std::rc::Rc<dyn Fn(&str, &mut Window, &mut App)>;

/// Everything a row needs to render its moderation-button strip. The strip is
/// a **uniform gutter across the whole view**: one slot per button applicable
/// to *any* of `platforms`, with slots that don't apply to this row (wrong
/// platform / local echo / row not moderated at all) rendered as invisible
/// ghosts of the same width — so in a merged Twitch+Kick feed every message
/// starts at the same x no matter how many buttons its platform really gets.
pub struct ModStrip {
    pub click: ModClick,
    /// The platforms this view moderates (`ChannelModel::mod_platforms`).
    pub platforms: Vec<Platform>,
    /// Whether this row's own platform is moderated (`false` = the whole strip
    /// is ghosts — the row only keeps the gutter for alignment).
    pub row_moderated: bool,
}

/// The per-row interaction callbacks the view supplies (none in contexts like the
/// usercard list, where rows aren't interactive). `name_click` opens the clicked
/// chatter's usercard; `link_hover` repaints so a wrapped link underlines as one;
/// `emote_click` opens an emote-info popup; `reply_click` starts a reply to the row.
#[derive(Default)]
pub struct RowHandlers {
    pub name_click: Option<NameClick>,
    /// Set in the live log: right-clicking the author name tags them (inserts
    /// `@name ` and switches the send target to their platform).
    pub name_right_click: Option<NameRightClick>,
    pub mention_click: Option<MentionClick>,
    pub link_hover: Option<LinkHover>,
    /// Set in the live log when link previews are on: fires on link enter/leave
    /// to drive the hover preview tooltip.
    pub link_preview_hover: Option<LinkPreviewHover>,
    pub emote_click: Option<EmoteClick>,
    pub seventv_link_click: Option<SeventvLinkClick>,
    pub reply_click: Option<ReplyClick>,
    /// Set in the live log on a reply row: clicking its "replying to" context line
    /// opens the thread panel. `None` leaves the line inert (panels, usercard).
    pub thread_click: Option<ThreadClick>,
    /// Set only when the logged-in user can moderate this row's platform — the
    /// pin button renders (on hover) only then.
    pub pin_click: Option<PinClick>,
    /// Set when this row should carry the left-side moderation-button strip
    /// (the view resolves the visibility mode + hover tracking + which
    /// platforms it moderates — see [`ModStrip`]).
    pub mod_strip: Option<ModStrip>,
    /// A pre-built inline link-preview card (built by the view from the shared
    /// preview cache), appended under the message body. `None` when inline
    /// previews are off or the message has no previewable link.
    pub inline_preview: Option<gpui::AnyElement>,
}

/// Per-message display flags, set by the view from the row's state.
#[derive(Clone, Copy, Default)]
pub struct RowFlags {
    /// The author was banned/timed-out or the message deleted: strike + fade it.
    pub struck: bool,
    /// The message mentions the user: tint its background.
    pub mentioned: bool,
    /// The chat log paints highlights (mention/first-message tint + accent bar)
    /// on its full-width row wrapper so they bleed edge-to-edge; it sets this so
    /// the message doesn't also paint its own. Panels (mentions, pin banner)
    /// leave it false and get the self-contained tint.
    pub external_highlight: bool,
    /// Suppress the row's leading timestamp. Set from the per-surface "show
    /// timestamps" settings by the chat log and mentions panel; other surfaces
    /// (usercard, pin banner) leave it false and always show the time.
    pub hide_timestamp: bool,
    /// The message matched a *suppress* term: render the whole row at very low
    /// opacity (kept visible/readable, but easy to skip). Distinct from `struck`
    /// (which also strikes through) and `historical` (a lighter fade).
    pub suppressed: bool,
}

/// One chat message as a wrapping row: platform · time · name · tokens. When
/// `struck`, the row is struck through and faded (set on a ban/timeout or a
/// message deletion, and kept — an unban doesn't restore it).
/// `ordinal` is a running, document-order counter shared across the whole log:
/// each selectable text token claims the next value, giving the [`Selection`] a
/// total order to walk when assembling copied text. The caller advances it row
/// by row so selection spans messages correctly.
/// `name_click` is `Some` when clicking the author name should do something (open
/// the usercard); the card's list passes `None` so its rows aren't re-clickable.
pub fn render_message(
    msg: &Message,
    flags: RowFlags,
    font_size: f32,
    selection: &Selection,
    ordinal: &mut usize,
    handlers: RowHandlers,
) -> impl IntoElement {
    let RowHandlers {
        name_click,
        name_right_click,
        mention_click,
        link_hover,
        link_preview_hover,
        emote_click,
        seventv_link_click,
        reply_click,
        thread_click,
        pin_click,
        mod_strip,
        inline_preview,
    } = handlers;
    let RowFlags {
        struck,
        mentioned,
        external_highlight,
        hide_timestamp,
        suppressed,
    } = flags;
    let scale = Scale::new(font_size);
    let name_color = readable_color(
        msg.author
            .color
            .map(Color::to_u32)
            .unwrap_or_else(|| fallback_name_color(&msg.author)),
    );
    // Show the time in the user's local timezone (the stored timestamp is UTC).
    let time = msg
        .timestamp
        .with_timezone(&chrono::Local)
        .format("%H:%M")
        .to_string();
    let ids = RowIds::new(msg.id.clone());
    let ctx = RenderCtx {
        ids: &ids,
        selection,
    };

    let mut tokens: Vec<gpui::AnyElement> = Vec::new();
    // The author name is the first selectable token so copying a line yields
    // "name: message". When `name_click` is set it's also a click target (opens
    // the usercard) — wrapped so a plain click fires but a drag still selects.
    tokens.push(name_token(
        &ctx,
        ordinal,
        format!("{}:", msg.author.display_name),
        name_color,
        msg.author.paint.as_ref(),
        name_click,
        name_right_click,
    ));

    // Twitch prepends `@ParentName ` to a reply's body (Kick doesn't). With the
    // "replying to" line shown above, that prefix is redundant — strip it from
    // the first text run. Matched against the known parent author so we only ever
    // remove the actual reply mention, never text the user typed. The mention
    // tokenizer usually turns that prefix into a leading `Mention` element, so
    // skip that form too (and the separator space it leaves on the next run).
    let reply_prefix = msg.reply.as_ref().map(|r| r.author.as_str());
    let mut elements = msg.elements.as_slice();
    let mut trim_reply_gap = false;
    if let (Some(parent), Some(MessageElement::Mention { login })) =
        (reply_prefix, elements.first())
    {
        if login.eq_ignore_ascii_case(parent.trim_start_matches('@')) {
            elements = &elements[1..];
            trim_reply_gap = true;
        }
    }
    let mut first_text = true;

    let mut emote_index = 0usize;
    for element in elements {
        match element {
            MessageElement::Text { text, color } => {
                let shown = if first_text {
                    let stripped = strip_reply_prefix(text, reply_prefix);
                    if trim_reply_gap {
                        stripped.trim_start().to_string()
                    } else {
                        stripped
                    }
                } else {
                    text.clone()
                };
                first_text = false;
                if shown.is_empty() {
                    continue; // The run was only the reply mention prefix.
                }
                // Per-word tokens so a long run wraps at word boundaries; URLs in
                // the run are detected (7TV emote links open the popup, others are
                // normal links).
                push_run(
                    &mut tokens,
                    &ctx,
                    ordinal,
                    &shown,
                    color.map(Color::to_u32),
                    seventv_link_click.as_ref(),
                    link_hover.as_ref(),
                    link_preview_hover.as_ref(),
                );
            }
            MessageElement::Emote(emote) => {
                let ord = *ordinal;
                *ordinal += 1;
                let image =
                    animated_img(ids.emote(emote_index), emote.url.clone(), px(scale.emote));
                // A larger preview + name/provider/author shown on hover.
                let tip_url = SharedString::from(emote.url.clone());
                let tip_text = SharedString::from(emote_tooltip_text(emote));
                emote_index += 1;
                // A plain click opens an info popup for the emote (guarded so the
                // click that ends a drag-select doesn't trigger it). The closure
                // owns a copy of the emote so it can pass its facts to the popup.
                let click = emote_click.clone().map(|cb| {
                    let emote = emote.clone();
                    let sel = selection.clone();
                    (cb, emote, sel)
                });
                // Small horizontal margin stands in for the (removed) row gap so
                // emotes aren't flush against adjacent words. The wrapper carries a
                // stable id so it can host the hover tooltip.
                let mut wrap =
                    div()
                        .id(ids.emote(ord))
                        .mx_px()
                        .tooltip(move |window, cx| {
                            image_tooltip(
                                tip_url.clone(),
                                tip_text.clone(),
                                TOOLTIP_PREVIEW_HEIGHT,
                                window,
                                cx,
                            )
                        });
                if let Some((cb, emote, sel)) = click {
                    wrap = wrap.cursor_pointer().on_mouse_up(
                        MouseButton::Left,
                        move |ev: &gpui::MouseUpEvent, window, cx| {
                            if !sel.has_selection() {
                                cb(&emote, ev.position, window, cx);
                            }
                        },
                    );
                }
                tokens.push(
                    wrap.child(SelectableImage::new(
                        ids.token(ord),
                        ord,
                        emote.name.clone(),
                        selection.clone(),
                        image,
                    ))
                    .into_any_element(),
                );
            }
            MessageElement::Mention { login } => {
                // Clickable when the view supplies the handler: opens the
                // mentioned user's usercard (a bare card + async lookup when
                // they haven't chatted). Guarded like the other click targets
                // so the click ending a drag-select doesn't fire.
                let ord = *ordinal;
                let token = text_token(&ctx, ordinal, format!("@{login}"), None, true, false);
                match &mention_click {
                    Some(cb) => {
                        let cb = cb.clone();
                        let login = login.clone();
                        let sel = selection.clone();
                        tokens.push(
                            div()
                                .id(ids.token(ord))
                                .cursor_pointer()
                                .hover(|s| s.underline())
                                .on_mouse_up(
                                    MouseButton::Left,
                                    move |_, window, cx| {
                                        if !sel.has_selection() {
                                            cb(&login, window, cx);
                                        }
                                    },
                                )
                                .child(token)
                                .into_any_element(),
                        );
                    }
                    None => tokens.push(token),
                }
            }
            MessageElement::Link { url, text } => {
                push_link(
                    &mut tokens,
                    &ctx,
                    ordinal,
                    url,
                    text,
                    link_hover.as_ref(),
                    link_preview_hover.as_ref(),
                );
            }
            MessageElement::Badge(_) => {} // Badge CDN lookup is deferred past M1.
        }
    }

    // Author badges (subscriber/VIP/mod/...), each a small CDN image. The bridge
    // fills these in with resolved URLs; unresolved ones were already dropped.
    let author_badges: Vec<gpui::AnyElement> = msg
        .author
        .badges
        .iter()
        .enumerate()
        .map(|(i, badge)| {
            let image = animated_img(ids.badge(i), badge.url.clone(), px(scale.badge));
            // Twitch fills the badge title; Kick leaves it None (no tooltip yet).
            // When present, hovering shows a larger preview + the title.
            let tip = badge.title.clone().map(|title| {
                (
                    SharedString::from(badge.url.clone()),
                    SharedString::from(title),
                )
            });
            image_line_box(scale, scale.badge)
                .id(ids.badge(i))
                .mr_1()
                .when_some(tip, |b, (url, title)| {
                    b.tooltip(move |window, cx| {
                        image_tooltip(
                            url.clone(),
                            title.clone(),
                            TOOLTIP_BADGE_PREVIEW_HEIGHT,
                            window,
                            cx,
                        )
                    })
                })
                .child(image)
                .into_any_element()
        })
        .collect();

    // The left-side moderation-button strip. The view supplies the context only
    // when the strip should show on this row (visibility mode + hover tracking
    // resolved there — see `LogView`).
    let mod_strip = mod_strip.map(|ctx| mod_button_strip(msg, &ids, scale, ctx));

    // No row gap: body words carry their own whitespace (so copy is exact and
    // words wrap individually). The structural prefix — icon, time, badges, name
    // — gets explicit right margins instead, and emotes their own.
    let body = h_flex()
        .w_full()
        .min_w_0()
        .flex_wrap()
        // Tight line height cascades to every text token so gpui shapes them
        // shorter than its 1.618 default (prefix boxes match via `scale.line`),
        // tightening wrapped-line spacing within a message.
        .line_height(px(scale.line))
        // Top-align: when the message wraps to several lines, centering would push
        // the first lines above the name/badges — start-align keeps the name on the
        // first line with text flowing down beneath it.
        .items_start()
        // Banned/timed-out: strike through the whole row and fade it.
        .when(struck, |row| row.line_through().opacity(STRUCK_OPACITY))
        .children(mod_strip)
        .child(platform_badge(msg.platform, scale).mr_1())
        // The timestamp is chat-font-sized: full-size text fills the line box, so it shares the
        // body text's baseline exactly — a smaller size can't be made to align
        // with both the text baseline and the icon/badge centers at once. Hidden
        // when the surface's "show timestamps" setting is off.
        .when(!hide_timestamp, |row| {
            row.child(
                line_box(scale)
                    .mr_1()
                    .text_color(rgb(palette().timestamp))
                    .child(time),
            )
        })
        .children(author_badges)
        .children(tokens);

    // A first-time chatter's / highlighted-redemption row gets a label pinned to
    // the top-right corner (Twitch-style). Wrap the body so the label sits beside
    // it without being caught in the body's flex-wrap. First-message wins if a row
    // is somehow both.
    let corner_label = if msg.first_message {
        Some(("FIRST MESSAGE", palette().first_message_label))
    } else if msg.highlighted {
        Some(("HIGHLIGHTED", palette().highlighted_label))
    } else {
        None
    };
    let body = if let Some((text, color)) = corner_label {
        h_flex()
            .w_full()
            .min_w_0()
            .items_start()
            .child(div().flex_1().min_w_0().child(body))
            .child(
                div()
                    .flex_none()
                    .ml_2()
                    .mt_0p5()
                    .px_1p5()
                    .rounded_full()
                    .border_1()
                    .border_color(rgb(color))
                    .text_size(px(scale.small * 0.9))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(color))
                    .child(text),
            )
            .into_any_element()
    } else {
        body.into_any_element()
    };

    // Hover actions (📌 pin for moderators, ↩ reply) overlaid on the row's
    // top-right corner, hidden until the row is hovered (group-hover reveals
    // them). An absolute overlay instead of a flex sibling so the message text
    // uses the full row width; the opaque chip background covers whatever text
    // sits under it while hovered. `visibility: hidden` (not opacity 0) matters:
    // gpui skips a hidden element's mouse listeners, so the unhovered overlay
    // can't intercept clicks or selection drags on the text beneath it.
    // The row's hover group is named by the base id itself (group names only
    // have to be distinct from other group names, and nothing else groups on
    // message ids) — like the ids above, a refcount bump instead of a per-frame
    // `format!`.
    let group_id = ids.base.clone();
    let body = if reply_click.is_some() || pin_click.is_some() {
        let mut chips = h_flex()
            .gap_0p5()
            .px_0p5()
            .py_0p5()
            .bg(rgb(palette().panel_bg))
            .border_1()
            .border_color(rgb(panel_border()))
            .rounded_md()
            .shadow_sm();
        if let Some(cb) = pin_click {
            chips = chips.child(hover_action(
                ids.pin(),
                Some("icons/pin.svg"),
                "pin",
                scale,
                cb,
            ));
        }
        if let Some(cb) = reply_click {
            chips = chips.child(hover_action(ids.reply(), None, "↩ reply", scale, cb));
        }
        // The chip pill sits in a full-height right-edge strip that centers it
        // vertically on the row; the strip itself carries no listeners, so it
        // never blocks text under it while hidden or around the pill.
        let actions = h_flex()
            .invisible()
            .group_hover(group_id.clone(), |s| s.visible())
            .absolute()
            .top_0()
            .bottom_0()
            .right_0()
            .items_center()
            .child(chips);
        div()
            .relative()
            .w_full()
            .min_w_0()
            .child(body)
            .child(actions)
            .into_any_element()
    } else {
        body.into_any_element()
    };

    // A reply adds a muted context line above the message; without one we return
    // the row directly so non-reply messages keep their single-row layout. A
    // message that mentions the user, or a chatter's first message in the channel,
    // gets a subtle tint plus a colored accent bar on its left edge — mention
    // wins when a row is both. In the chat log the wrapper paints this full-bleed
    // instead (`external_highlight`).
    let row_accent = if external_highlight {
        None
    } else if mentioned {
        Some(highlight_mention())
    } else if msg.first_message {
        Some(highlight_first_message())
    } else if msg.highlighted {
        Some(highlight_highlighted())
    } else {
        None
    };
    v_flex()
        .group(group_id)
        .w_full()
        .min_w_0()
        // A little vertical padding between messages: the line height is tight so
        // wrapped lines within a message stay close, but adjacent messages need a
        // small gap to read as separate. The highlighted-row pill below reuses
        // this same padded box for its tint. Compact mode zeroes the padding so
        // consecutive rows butt together; the default roomier gap stays 2px.
        .map(|row| {
            if crate::settings::compact_chat() {
                row.py_0()
            } else {
                row.py_0p5()
            }
        })
        // Backfilled history is dimmed to set it apart from live chat.
        .when(msg.historical, |row| row.opacity(HISTORY_OPACITY))
        // A suppressed (term-matched) row is faded so the eye skips it, while
        // staying readable on a closer look. The opacity is user-configurable.
        .when(suppressed, |row| {
            row.opacity(crate::settings::suppressed_opacity())
        })
        // The tinted pill gets horizontal padding for the rounded look, with an
        // equal negative margin so the message content stays flush with normal
        // (un-highlighted) rows instead of being nudged right — the tint floats as
        // a pill that bleeds an equal amount past the text on each side, with the
        // accent bar riding its left edge (same box model as error/automod rows).
        .when_some(row_accent, |row, (bg, accent)| {
            // The outer row already carries `py_0p5`; the pill only adds its
            // horizontal inset/tint so highlighted rows stay the same height.
            row.bg(rgb(bg))
                .rounded_md()
                .border_l_2()
                .border_color(rgb(accent))
                .px(px(HIGHLIGHT_INSET))
                .mx(px(-HIGHLIGHT_INSET))
        })
        .when_some(msg.reply.as_ref(), |col, reply| {
            let id_seed = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                msg.id.hash(&mut h);
                h.finish()
            };
            col.child(reply_line(reply, scale, thread_click.clone(), id_seed))
        })
        .child(body)
        // The inline link-preview card (when enabled + the message has a
        // previewable link), a compact block under the message body.
        .children(inline_preview)
}

/// The data an inline preview card renders. Built by the view from the shared
/// preview cache; `title` empty + all fields blank = the loading skeleton.
pub struct InlinePreview {
    pub title: SharedString,
    /// The muted "channel · views · Clipped by X" line (already composed).
    pub meta: SharedString,
    pub thumbnail_url: Option<SharedString>,
    /// True when the thumbnail is being withheld by streamer mode (as opposed to
    /// the source simply not having one) — renders a 🕶 placeholder like the
    /// usercard avatar, so it reads as intentionally hidden.
    pub thumbnail_hidden: bool,
    /// The clicked-through URL (opens on click, with the confirm dialog).
    pub url: String,
}

/// The fixed height of the inline preview card (thumbnail + two text lines),
/// reserved from the moment a previewable link appears so the row never jumps
/// when the async fetch fills the card in. The thumbnail is 16:9 at this height.
pub const INLINE_PREVIEW_H: f32 = 72.;

/// A compact horizontal link-preview card shown under a chat message: a small
/// 16:9 thumbnail on the left, then the title and a muted meta line. Fixed
/// height ([`INLINE_PREVIEW_H`]) so it reserves its space up front (skeleton
/// while loading, filled in on load — no layout jump). A plain click opens the
/// link through the usual confirm dialog.
pub fn inline_preview_card(preview: InlinePreview, row_id: &str, font_size: f32) -> gpui::AnyElement {
    let scale = Scale::new(font_size);
    let thumb_w = INLINE_PREVIEW_H * 16. / 9.;
    let border = rgb(panel_border());
    let mut card = h_flex()
        // Keyed by the message id, not the URL: the same clip posted several times
        // renders several cards, and a shared id would collide (only the first
        // would be clickable). Message ids are unique per row.
        .id(SharedString::from(format!("inline-preview-{row_id}")))
        .h(px(INLINE_PREVIEW_H))
        .max_w(px(360.))
        .my_1()
        .overflow_hidden()
        .items_stretch()
        .bg(rgb(panel_bg()))
        .border_1()
        .border_color(border)
        .rounded_md()
        .cursor_pointer();

    // Thumbnail on the left. A real image when present; a 🕶 placeholder when
    // streamer mode is hiding it; a plain block while loading / genuinely absent.
    let thumb = div()
        .flex_none()
        .w(px(thumb_w))
        .h_full()
        .bg(rgb(panel_bg()));
    card = card.child(match &preview.thumbnail_url {
        Some(url) => thumb
            .child(
                img(url.clone())
                    .w_full()
                    .h_full()
                    .object_fit(gpui::ObjectFit::Cover),
            )
            .into_any_element(),
        None if preview.thumbnail_hidden => thumb
            .flex()
            .items_center()
            .justify_center()
            .text_color(rgb(palette().timestamp))
            .child("🕶")
            .into_any_element(),
        None => thumb.into_any_element(),
    });

    // Text column: title (up to two lines) + muted meta line.
    let text = v_flex()
        .flex_1()
        .min_w_0()
        .px_2()
        .py_1()
        .justify_center()
        .gap_0p5()
        .child(
            div()
                .font_weight(FontWeight::MEDIUM)
                .text_size(px(scale.small))
                .line_height(px(scale.small * 1.25))
                .line_clamp(2)
                .child(preview.title),
        )
        .when(!preview.meta.is_empty(), |c| {
            c.child(
                div()
                    .text_size(px(scale.small * 0.92))
                    .text_color(rgb(palette().timestamp))
                    .overflow_hidden()
                    .truncate()
                    .child(preview.meta),
            )
        });
    card = card.child(text);

    // A plain click opens the link through the same confirm dialog as clicking
    // the link text (a loading card is clickable too — it carries the real URL).
    let url = preview.url;
    card.on_click(move |_, window, cx| confirm_open_url(url.clone(), window, cx))
        .into_any_element()
}

/// A row's left-side moderation-button strip: the user's button list in list
/// order (the stock delete/ban/timeout are seeded entries in it —
/// `settings::mod_buttons`), one slot per button applicable to any of the
/// view's moderated platforms. A slot renders as a real button when it applies
/// to this row, else as an invisible ghost of the same width (wrong platform;
/// a command the row's platform doesn't support, like /delete on Kick;
/// `{msg-id}` commands on local-echo rows, whose synthetic id no API accepts;
/// or the row's platform isn't moderated at all) — the constant gutter width
/// keeps every message's text at the same x across a merged multi-platform feed.
/// ⚠️ Whether the strip exists is decided per frame by the *view* (mode +
/// hover tracking) — do NOT hide/show it here with a `group_hover` display
/// switch: hover state can flip between prepaint and paint, and painting a
/// subtree that skipped prepaint panics ("must call prepaint before paint").
fn mod_button_strip(msg: &Message, ids: &RowIds, scale: Scale, ctx: ModStrip) -> gpui::AnyElement {
    let list = crate::settings::mod_buttons();
    let echo = msg.id.starts_with("echo-");
    let buttons: Vec<gpui::AnyElement> = list
        .iter()
        .filter(|b| b.platform.is_none_or(|p| ctx.platforms.contains(&p)))
        .enumerate()
        .map(|(i, b)| {
            let real = ctx.row_moderated
                && b.platform.is_none_or(|p| p == msg.platform)
                && crate::commands::supported_on(&b.command, msg.platform)
                && !(echo && crate::commands::needs_msg_id(&b.command));
            mod_button_chip(ids.mod_button(i), b, scale, ctx.click.clone(), !real)
        })
        .collect();
    // An emptied list leaves no box behind — an empty line box + margin would
    // still indent the message.
    if buttons.is_empty() {
        return gpui::Empty.into_any_element();
    }
    let size = mod_button_size(scale);
    image_line_box(scale, size)
        .flex_none()
        .mr_1()
        .child(h_flex().gap_px().children(buttons))
        .into_any_element()
}

/// One moderation button's square size at this font scale.
fn mod_button_size(scale: Scale) -> f32 {
    scale.badge + 4.0
}

/// One moderation button: a vector icon when the button's `icon` names one
/// (see `assets::mod_icon_path`), else the text/emoji itself. Tooltip = the
/// button's name; mouse-down fires the command callback with propagation
/// stopped (like the hover chips, so the click doesn't start a selection).
/// A `ghost` renders the same face invisible — identical width (it IS the
/// button, just not painted), no listeners (gpui skips a hidden element's) —
/// keeping the strip's gutter width constant across platforms.
fn mod_button_chip(
    id: (SharedString, usize),
    button: &crate::settings::ModButton,
    scale: Scale,
    cb: ModClick,
    ghost: bool,
) -> gpui::AnyElement {
    let size = mod_button_size(scale);
    let command = SharedString::from(button.command.clone());
    let tip = SharedString::from(if button.name.is_empty() {
        button.command.clone()
    } else {
        button.name.clone()
    });
    let face = match crate::assets::mod_icon_path(&button.icon) {
        // gpui's `svg()` paints only when the svg element ITSELF has a text
        // color (`svg.rs::paint` zips the path with its own `style.text.color`
        // — nothing cascades from the wrapper), so set it here, not above.
        Some(path) => gpui::svg()
            .path(path)
            .size(px(scale.badge * 0.8))
            .flex_none()
            .text_color(rgb(palette().timestamp))
            .into_any_element(),
        None => div()
            .text_size(px(scale.small * 0.9))
            .child(SharedString::from(button.icon.clone()))
            .into_any_element(),
    };
    let base = no_strike(
        div()
            .id(id)
            .flex_none()
            .min_w(px(size))
            .h(px(size))
            .px_0p5()
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .child(face),
    );
    if ghost {
        return base.invisible().into_any_element();
    }
    base.cursor_pointer()
        .text_color(rgb(palette().timestamp))
        .hover(|s| s.bg(chrome_hover()).text_color(rgb(palette().default_name)))
        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            cb(&command, window, cx);
        })
        .into_any_element()
}

/// Cancels an inherited strikethrough on a row-embedded control: a struck
/// row's `line_through` cascades into every text child, and a child refinement
/// can't *unset* it (refine only copies `Some` values) — but a zero-thickness
/// override draws nothing. Buttons stay readable on banned/deleted rows.
fn no_strike<E: gpui::Styled>(mut el: E) -> E {
    el.text_style().strikethrough = Some(gpui::StrikethroughStyle {
        thickness: px(0.),
        ..Default::default()
    });
    el
}

/// One action chip in a row's hover overlay (pin, "↩ reply"): highlighted
/// on its own hover, firing `cb` on mouse-down (with propagation stopped so the
/// same mouse-down doesn't refocus the log and steal focus back from the input).
/// `icon` is an optional SVG drawn before the label (with its own text color —
/// nothing cascades onto `svg()`). The show/hide-on-row-hover lives on the
/// overlay container, not the chip.
fn hover_action(
    id: impl Into<gpui::ElementId>,
    icon: Option<&'static str>,
    label: &'static str,
    scale: Scale,
    cb: RowAction,
) -> gpui::AnyElement {
    no_strike(
        div()
            .id(id)
            .flex_none()
            .flex()
            .items_center()
            .gap_0p5()
            .px_1p5()
            .rounded_sm()
            .cursor_pointer()
            .text_size(px(scale.small))
            .text_color(rgb(palette().timestamp))
            .hover(|s| {
                s.bg(chrome_hover())
                    .text_color(rgb(palette().default_name))
            })
            .when_some(icon, |chip, path| {
                chip.child(
                    gpui::svg()
                        .path(path)
                        .size(px(scale.small))
                        .flex_none()
                        .text_color(rgb(palette().timestamp)),
                )
            })
            .child(SharedString::from(label)),
    )
    .on_mouse_down(MouseButton::Left, move |_, window, cx| {
        cb(window, cx);
        cx.stop_propagation();
    })
    .into_any_element()
}

/// Removes a leading `@<author>` reply mention (and the space after it) from a
/// reply's first text run, so it isn't shown twice alongside the "replying to"
/// line. `author` is the known parent author; matching is case-insensitive. No
/// `author` (not a reply) or a non-matching run is returned unchanged.
fn strip_reply_prefix(text: &str, author: Option<&str>) -> String {
    let Some(author) = author else {
        return text.to_string();
    };
    let rest = text.strip_prefix('@').unwrap_or(text);
    if rest.len() >= author.len()
        && rest.is_char_boundary(author.len())
        && rest[..author.len()].eq_ignore_ascii_case(author)
    {
        let after = &rest[author.len()..];
        // Only strip when the name is a whole word (end of run or followed by a
        // space), then drop one separating space.
        if after.is_empty() || after.starts_with(char::is_whitespace) {
            return after.trim_start().to_string();
        }
    }
    text.to_string()
}

/// The muted "↪ replying to @name: text" line shown above a reply, with the
/// parent body truncated to [`REPLY_PREVIEW_CHARS`]. When `thread_click` is set
/// (live log), the line is clickable to open the thread panel and hints at it
/// with a pointer cursor + a hover underline; `id_seed` gives the clickable
/// element a stable id (the message id).
fn reply_line(
    reply: &ReplyParent,
    scale: Scale,
    thread_click: Option<ThreadClick>,
    id_seed: u64,
) -> impl IntoElement {
    let mut preview: String = reply.text.chars().take(REPLY_PREVIEW_CHARS).collect();
    if reply.text.chars().count() > REPLY_PREVIEW_CHARS {
        preview.push('…');
    }
    let text = SharedString::from(format!("↪ replying to @{}: {preview}", reply.author));
    let base = div()
        .text_size(px(scale.small))
        .text_color(rgb(palette().reply));
    match thread_click {
        Some(cb) => base
            .id(("reply-thread-line", id_seed as usize))
            .cursor_pointer()
            .hover(|s| s.underline())
            .on_mouse_down(
                MouseButton::Left,
                move |ev: &gpui::MouseDownEvent, window: &mut Window, cx: &mut App| {
                    // Don't let the click also reach the log row underneath (its
                    // hover/selection machinery) — this click only opens the thread.
                    cx.stop_propagation();
                    cb(ev.position, window, cx)
                },
            )
            .child(text)
            .into_any_element(),
        None => base.child(text).into_any_element(),
    }
}

/// A connector notice (connected, errors, ...), rendered muted.
pub fn render_system(text: &str, font_size: f32) -> impl IntoElement {
    div()
        .text_size(px(Scale::new(font_size).small))
        .text_color(rgb(palette().system))
        .child(SharedString::from(text.to_string()))
}

/// A full-width date band drawn above the first row of each new local calendar
/// day ("Wednesday, July 16, 2026" between hairlines) — Chatterino inserts a
/// system row for this; ours is render-derived from the row's neighbors (see
/// the log's item closure), so it needs no buffer row of its own. The hairlines
/// derive from the palette's secondary-text tone at low alpha, so custom themes
/// re-tint them automatically.
pub fn render_day_divider(label: &str, font_size: f32) -> impl IntoElement {
    let scale = Scale::new(font_size);
    let p = palette();
    let hairline = || {
        div()
            .flex_1()
            .h(px(1.0))
            .bg(gpui::rgba((p.timestamp << 8) | 0x55))
    };
    h_flex()
        .w_full()
        .min_w_0()
        .items_center()
        .gap_2()
        .py_1()
        .child(hairline())
        .child(
            div()
                .flex_none()
                .text_size(px(scale.small))
                .text_color(rgb(p.timestamp))
                .child(SharedString::from(label.to_string())),
        )
        .child(hairline())
}

/// A user-facing error: a red-tinted row whose text is drag-selectable (built from
/// the same per-word [`SelectableText`] tokens as chat bodies, so it wraps and
/// copies), with a small "⧉ copy" affordance that writes the full text to the
/// clipboard. Threads the shared `selection` + running `ordinal` like
/// [`render_message`] so a drag can span it together with chat rows. Colors come
/// from the active [`palette`] so the row re-themes with the rest of the log.
pub fn render_error(
    text: &str,
    font_size: f32,
    selection: &Selection,
    ordinal: &mut usize,
) -> impl IntoElement {
    let scale = Scale::new(font_size);
    let p = palette();
    let ids = RowIds::new(format!("err:{}", stable_id(text)));
    let ctx = RenderCtx {
        ids: &ids,
        selection,
    };

    // The error text as selectable per-word tokens. The first token starts a row so
    // a multi-row copy newline-separates it from neighbouring chat.
    let mut tokens: Vec<gpui::AnyElement> = Vec::new();
    tokens.push(text_token(
        &ctx,
        ordinal,
        "⚠ ".to_string(),
        Some(p.error_text),
        false,
        true,
    ));
    push_text_words(&mut tokens, &ctx, ordinal, text, Some(p.error_text));

    // A copy button; clicking it puts the whole error text on the clipboard.
    let copy_text = text.to_string();
    let copy = div()
        .id(SharedString::from(format!("err-copy:{}", stable_id(text))))
        .flex_none()
        .ml_2()
        .px_1()
        .rounded_sm()
        .cursor_pointer()
        .text_size(px(scale.small))
        .text_color(rgb(p.timestamp))
        .hover(move |s| s.text_color(rgb(p.error_text)))
        .child(SharedString::from("⧉ copy"))
        .on_mouse_up(MouseButton::Left, move |_, _window, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy_text.clone()));
        });

    // Bare content — the log's row wrapper paints the tint + accent bar.
    h_flex()
        .w_full()
        .min_w_0()
        .items_start()
        .text_size(px(scale.font))
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .flex_wrap()
                .items_start()
                .children(tokens),
        )
        .child(copy)
}

/// A held AutoMod row's Allow/Deny click: `(message_id, allow)`.
pub type AutoModClick = std::rc::Rc<dyn Fn(&str, bool, &mut Window, &mut App)>;

/// A small outlined action chip for the AutoMod row (Allow / Deny).
fn automod_chip(
    id: String,
    label: &'static str,
    color: u32,
    font_size: f32,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    div()
        .id(SharedString::from(id))
        .flex_none()
        .px_2()
        .rounded_sm()
        .border_1()
        .border_color(rgb(color))
        .cursor_pointer()
        .text_size(px(font_size))
        .text_color(rgb(color))
        .hover(|s| s.opacity(0.75))
        .child(SharedString::from(label))
        .on_mouse_up(MouseButton::Left, move |_, window, cx| on_click(window, cx))
        .into_any_element()
}

/// A message AutoMod held for review: an amber-tinted row naming the chatter and
/// why it was held ("automod: swearing, level 4" / "blocked term"), the held text
/// (selectable, like an error row), and Allow/Deny chips while unresolved. Once a
/// moderator acts — or the hold expires — a status line ("✔ allowed by mod")
/// replaces the chips. Only moderators ever see these rows (the EventSub feed
/// they arrive on requires it).
#[allow(clippy::too_many_arguments)]
pub fn render_automod(
    message_id: &str,
    user: &str,
    text: &str,
    reason: &str,
    resolved: Option<(AutoModStatus, &str)>,
    font_size: f32,
    selection: &Selection,
    ordinal: &mut usize,
    on_action: AutoModClick,
    name_click: Option<&MentionClick>,
) -> impl IntoElement {
    let scale = Scale::new(font_size);
    let p = palette();
    let ids = RowIds::new(format!("automod:{message_id}"));
    let ctx = RenderCtx {
        ids: &ids,
        selection,
    };

    // Header + held text as selectable per-word tokens (they wrap and copy like
    // chat bodies); the header starts a row so a multi-row copy stays separated.
    // The held-message author's name is a clickable token (opens their usercard)
    // split out of the header text, with the plain wording on either side.
    let mut header: Vec<gpui::AnyElement> = Vec::new();
    header.push(text_token(
        &ctx,
        ordinal,
        "⛨ ".to_string(),
        Some(p.automod_text),
        false,
        true,
    ));
    push_text_words(
        &mut header,
        &ctx,
        ordinal,
        "AutoMod held a message from ",
        Some(p.automod_text),
    );
    header.push(automod_name_token(
        &ctx,
        ordinal,
        user,
        p.automod_text,
        name_click,
    ));
    push_text_words(
        &mut header,
        &ctx,
        ordinal,
        &format!(" ({reason}):"),
        Some(p.automod_text),
    );
    let mut body: Vec<gpui::AnyElement> = Vec::new();
    push_text_words(&mut body, &ctx, ordinal, text, None);

    let actions: gpui::AnyElement = match resolved {
        Some((status, moderator)) => {
            let (line, color) = match status {
                AutoModStatus::Approved => (
                    if moderator.is_empty() {
                        "✔ allowed".to_string()
                    } else {
                        format!("✔ allowed by {moderator}")
                    },
                    p.automod_allow,
                ),
                AutoModStatus::Denied => (
                    if moderator.is_empty() {
                        "✖ denied".to_string()
                    } else {
                        format!("✖ denied by {moderator}")
                    },
                    p.automod_deny,
                ),
                AutoModStatus::Expired => ("expired unreviewed".to_string(), p.timestamp),
            };
            div()
                .text_size(px(scale.small))
                .text_color(rgb(color))
                .child(SharedString::from(line))
                .into_any_element()
        }
        None => {
            let allow = {
                let on_action = on_action.clone();
                let id = message_id.to_string();
                automod_chip(
                    format!("automod-allow:{message_id}"),
                    "✔ Allow",
                    p.automod_allow,
                    scale.small,
                    move |window, cx| on_action(&id, true, window, cx),
                )
            };
            let deny = {
                let id = message_id.to_string();
                automod_chip(
                    format!("automod-deny:{message_id}"),
                    "✖ Deny",
                    p.automod_deny,
                    scale.small,
                    move |window, cx| on_action(&id, false, window, cx),
                )
            };
            h_flex().gap_2().child(allow).child(deny).into_any_element()
        }
    };

    // Bare content — the log's row wrapper paints the tint + accent bar.
    h_flex()
        .w_full()
        .min_w_0()
        .items_start()
        .text_size(px(scale.font))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    h_flex()
                        .min_w_0()
                        .flex_wrap()
                        .items_start()
                        .children(header),
                )
                .child(h_flex().min_w_0().flex_wrap().items_start().children(body))
                .child(actions),
        )
}

/// The AutoMod header's chatter name as one selectable token that, when a
/// [`MentionClick`] is supplied, is also clickable (opens the chatter's usercard)
/// and underlines on hover — a drag-select ending on it copies instead of firing.
/// Without a handler it's a plain selectable token like the rest of the header.
fn automod_name_token(
    ctx: &RenderCtx,
    ordinal: &mut usize,
    user: &str,
    color: u32,
    name_click: Option<&MentionClick>,
) -> gpui::AnyElement {
    let click_ord = *ordinal;
    let inner = text_token(ctx, ordinal, user.to_string(), Some(color), false, false);
    match name_click {
        Some(cb) => {
            let cb = cb.clone();
            let login = user.to_string();
            let sel = ctx.selection.clone();
            div()
                .id(ctx.ids.token(click_ord))
                .cursor_pointer()
                .border_b_1()
                .border_color(gpui::transparent_black())
                .hover(move |s| s.border_color(rgb(color)))
                .on_mouse_up(MouseButton::Left, move |_, window, cx| {
                    if !sel.has_selection() {
                        cb(&login, window, cx);
                    }
                })
                .child(inner)
                .into_any_element()
        }
        None => inner,
    }
}

/// Extracts the clickable name from an event word, or `None` if the word isn't
/// a name to open a usercard for. A word is clickable when it's an `@mention` or
/// when it (stripped of surrounding punctuation) equals the event's `actor` — the
/// acting user "alice" in "alice redeemed …" / "alice subscribed …". Returns the
/// bare name (no `@`, no surrounding punctuation) in its original casing; the
/// usercard lookup (`open_usercard_named`) case-folds it.
fn event_name_login(word: &str, actor: Option<&str>) -> Option<String> {
    // The word core: leading `@` and any surrounding non-name punctuation
    // trimmed (so "@alice," and "alice." both yield "alice"), matching the
    // mention grammar in `bks_core::mention`.
    let core = word
        .trim_matches(|c: char| !(c.is_alphanumeric() || c == '_'))
        .trim_start_matches('@');
    if core.is_empty() {
        return None;
    }
    let is_mention = word.trim_start().starts_with('@');
    let is_actor = actor.is_some_and(|a| a.eq_ignore_ascii_case(core));
    (is_mention || is_actor).then(|| core.to_string())
}

/// Builds the word-token elements for an event's text line, making the acting
/// user's name (`actor`) and any `@mention` clickable when a [`MentionClick`] is
/// supplied — clicking opens that user's usercard, exactly like a chat mention.
/// `muted` tints the words (the events panel's condensed detail text); the actor,
/// when it appears, stays at full color so a clickable name reads as one. Non-name
/// words are plain, non-interactive `div`s (unchanged from the old inline tokens).
fn event_word_tokens(
    text: &str,
    actor: Option<&str>,
    muted: bool,
    row_id: u64,
    mention_click: Option<&MentionClick>,
) -> Vec<gpui::AnyElement> {
    let p = palette();
    split_words(text)
        .into_iter()
        .filter(|word| !word.trim().is_empty())
        .enumerate()
        .map(|(i, word)| {
            let base = div().when(muted, |d| d.text_color(rgb(p.timestamp)));
            match (mention_click, event_name_login(word, actor)) {
                (Some(cb), Some(login)) => {
                    let cb = cb.clone();
                    // The id base is the row's text hash, so a clickable name in
                    // one event row can't collide with one in another.
                    base.id(("event-name", row_id.wrapping_add(i as u64)))
                        // A clickable name stays full-strength even in muted
                        // detail text, and underlines on hover like a mention.
                        .when(muted, |d| d.text_color(rgb(p.link)))
                        .cursor_pointer()
                        .hover(|s| s.underline())
                        .on_mouse_up(MouseButton::Left, move |_, window, cx| {
                            cb(&login, window, cx);
                        })
                        .child(SharedString::from(word.to_string()))
                        .into_any_element()
                }
                _ => base.child(SharedString::from(word.to_string())).into_any_element(),
            }
        })
        .collect()
}

/// A public channel event (sub/gift/raid/watch-streak): a highlighted row with
/// the platform icon and the ready-made event text. On a sub/resub the chatter's
/// attached chat message (`message`) is rendered *under* the system text as its
/// own chat-style line — timestamp, badges, colored name, body with emotes inline
/// — like twitch.tv shows a resub. `timestamp` (when given) prefixes the system
/// text; the events panel passes it, the chat log doesn't (the log's neighbors
/// carry the time). Watch-streak rows (`kind`) get a distinct amber tint. The
/// text/message column wraps; the icon pins to the top so it stays aligned with
/// the first line when the content spans several.
/// `panel` selects the box model: `true` (the events panel) draws a
/// self-contained tinted pill inside the row box; `false` (the chat log) stays
/// bare — the log's full-width row wrapper paints the tint + accent bar
/// edge-to-edge instead.
#[allow(clippy::too_many_arguments)]
pub fn render_event(
    platform: Platform,
    kind: EventKind,
    text: &str,
    timestamp: Option<chrono::DateTime<chrono::Utc>>,
    message: Option<&Message>,
    accent: Option<u32>,
    actor: Option<&str>,
    mention_click: Option<&MentionClick>,
    font_size: f32,
    panel: bool,
) -> impl IntoElement {
    let scale = Scale::new(font_size);
    let p = palette();
    let (bg, fg) = highlight_event(kind, accent);

    // The system text is split into per-word tokens (like a chat body) so a long
    // event string wraps at word boundaries instead of overflowing the (often
    // narrow) row/panel. Each word keeps its trailing whitespace as the gap (no
    // margin — like chat body words; a margin on top of the space reads double
    // wide). A per-row id (hashed from the text) gives each emote a stable
    // element id so GPUI advances its animation frames. The acting user's name
    // and any `@mention` in the text are clickable (open the usercard) when a
    // handler is supplied.
    let row_id = stable_id(text);
    let tokens = event_word_tokens(text, actor, false, row_id, mention_click);

    let time = timestamp.map(|ts| ts.with_timezone(&chrono::Local).format("%H:%M").to_string());

    let header = h_flex()
        .w_full()
        .min_w_0()
        .items_start()
        .gap_2()
        .when_some(time, |row, time| {
            row.child(
                line_box(scale)
                    .flex_none()
                    .text_color(rgb(p.timestamp))
                    .child(time),
            )
        })
        // The icon sits in a one-text-line box and pins to the first line (the row
        // is `items_start`), so it aligns with the leading text rather than
        // floating against the top of a multi-line event.
        .child(platform_badge(platform, scale).flex_none())
        // `flex_1` + `min_w_0` let the content shrink below its width so it wraps
        // to multiple lines instead of overflowing the row on one line.
        .child(
            // Same line box as the timestamp/icon prefixes so the text shares
            // their baseline (see the note in `render_event_compact`); `items_start`
            // matches the row's top-pinned prefixes.
            h_flex()
                .flex_1()
                .min_w_0()
                .flex_wrap()
                .items_start()
                .line_height(px(scale.line))
                .children(tokens),
        );

    v_flex()
        .w_full()
        .min_w_0()
        // In a panel the row draws its own self-contained pill (tint + accent
        // bar inside the row box); in the log the wrapper paints it full-bleed,
        // so only the text color is set here.
        .when(panel, |pill| {
            pill.my_px()
                .px(px(HIGHLIGHT_INSET))
                .py_1()
                .rounded_md()
                .border_l_2()
                .border_color(rgb(fg))
                .bg(rgb(bg))
        })
        .text_color(rgb(fg))
        .text_size(px(scale.font))
        .child(header)
        .when_some(message, |col, msg| {
            col.child(event_message_line(msg, scale, row_id, mention_click, false))
        })
}

/// Per-kind accent for the events panel's dot markers — one hue per event
/// class so a glance sorts subs from gifts from raids. Medium saturation,
/// legible on both themes.
fn event_kind_color(kind: EventKind) -> u32 {
    match kind {
        EventKind::Sub => 0xa970ff,
        EventKind::Gift => 0x2ecc71,
        EventKind::Raid => 0xff8a3d,
        EventKind::Bits => 0x00bcd4,
        EventKind::Reward => 0x4a90e2,
        EventKind::WatchStreak => 0xffb340,
        EventKind::Announcement => 0x9147ff,
        EventKind::Other => 0x8a919e,
    }
}

/// One events-panel row's inputs for [`render_event_compact`].
pub struct PanelEvent<'a> {
    pub platform: Platform,
    pub kind: EventKind,
    pub text: &'a str,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub details: &'a bks_platform::EventDetails,
    /// The attached sub message — pre-gated by the tab's "hide sub messages".
    pub message: Option<&'a Message>,
    /// Whether the row hides an expandable recipient list (draws the chevron;
    /// the panel makes the row itself clickable).
    pub expandable: bool,
    /// The recipient names to reveal — `Some` while expanded.
    pub expanded_names: Option<Vec<String>>,
    /// Opens a clicked name's usercard (the actor + any `@mention`); `None`
    /// leaves the names non-interactive.
    pub mention_click: Option<MentionClick>,
}

/// The redesigned events-panel row: compact and information-first. A
/// kind-colored dot, a small timestamp, and the platform logo prefix the
/// condensed "Actor did-thing" line built from [`bks_platform::EventDetails`]
/// (emphasized actor, muted detail), falling back to the full pre-formatted
/// text for events without structured data. A collapsed mass gift shows a
/// chevron and, while expanded, its recipient list underneath; a sub's
/// attached message renders as a normal chat line below unless the tab hides
/// them.
pub fn render_event_compact(ev: PanelEvent<'_>, font_size: f32) -> impl IntoElement {
    let scale = Scale::new(font_size);
    let p = palette();
    let row_id = stable_id(ev.text);

    let dot = (scale.font * 0.45).round().max(5.0);
    let time = ev
        .timestamp
        .with_timezone(&chrono::Local)
        .format("%H:%M")
        .to_string();

    // The condensed actor + detail line, or the full text as wrapping word
    // tokens (same tokenization as `render_event` so long lines wrap at word
    // boundaries; each word keeps its trailing space as the gap). The actor name
    // and any `@mention` in the text open a usercard when a handler is supplied.
    let actor = ev.details.actor.as_deref();
    let mention_click = ev.mention_click.as_ref();
    // The same line box the timestamp/icon prefixes use (`scale.line`) so the
    // text tokens shape at that height and share their baseline — without it the
    // content shapes at gpui's taller default line height and, under the row's
    // `items_start`, sits lower than the icon/timestamp (the events-tab
    // misalignment). `items_start` matches the prefix boxes' top pin.
    let mut content = h_flex()
        .flex_1()
        .min_w_0()
        .flex_wrap()
        .items_start()
        .line_height(px(scale.line));
    content = match (&ev.details.actor, &ev.details.compact) {
        (Some(actor_name), Some(detail)) => {
            // The actor leads as a distinct bold token; make it clickable itself
            // rather than routing through the word helper, so it keeps its
            // emphasis (the helper only styles muted detail names).
            let actor_token = div()
                .font_weight(FontWeight::SEMIBOLD)
                .mr_1()
                .child(SharedString::from(actor_name.clone()));
            let actor_el = match mention_click {
                Some(cb) => {
                    let cb = cb.clone();
                    let login = actor_name.clone();
                    actor_token
                        .id(("event-actor", row_id))
                        .cursor_pointer()
                        .hover(|s| s.underline())
                        // Swallow the mouse-down so an expandable gift row's
                        // wrapper doesn't toggle its recipient list when the actor
                        // name is clicked — the name opens the usercard on mouse-up.
                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                            cx.stop_propagation();
                        })
                        .on_mouse_up(MouseButton::Left, move |_, window, cx| {
                            cb(&login, window, cx);
                        })
                        .into_any_element()
                }
                None => actor_token.into_any_element(),
            };
            content
                .child(actor_el)
                .children(event_word_tokens(detail, actor, true, row_id, mention_click))
        }
        _ => content.children(event_word_tokens(ev.text, actor, false, row_id, mention_click)),
    };
    if ev.expandable {
        // A visible affordance, not a bare glyph: a small tinted chip in the
        // kind's accent color (the row itself is the click target).
        content = content.child(
            div()
                .ml_1p5()
                .px_1()
                .rounded_sm()
                .bg(chrome_hover())
                .text_size(px(scale.small))
                .text_color(rgb(event_kind_color(ev.kind)))
                .child(SharedString::from(if ev.expanded_names.is_some() {
                    "▾ hide"
                } else {
                    "▸ show"
                })),
        );
    }

    let mut col = v_flex()
        .w_full()
        .min_w_0()
        .py_0p5()
        .text_size(px(scale.font))
        .child(
            h_flex()
                .w_full()
                .min_w_0()
                .items_start()
                .gap_1p5()
                .child(
                    image_line_box(scale, dot).flex_none().child(
                        // The dot takes the event's own accent when the
                        // platform assigned one (announcement colors).
                        div().size(px(dot)).rounded_full().bg(rgb(ev
                            .details
                            .accent
                            .unwrap_or_else(|| event_kind_color(ev.kind)))),
                    ),
                )
                .when(crate::settings::show_timestamps_events(), |row| {
                    row.child(
                        line_box(scale)
                            .flex_none()
                            .text_size(px(scale.small))
                            .text_color(rgb(p.timestamp))
                            .child(SharedString::from(time)),
                    )
                })
                .child(platform_badge(ev.platform, scale).flex_none())
                .child(content),
        );

    if let Some(names) = ev.expanded_names {
        col = col.child(
            div()
                .w_full()
                .min_w_0()
                .pl_4()
                .text_size(px(scale.small))
                .text_color(rgb(p.timestamp))
                .child(SharedString::from(format!("→ {}", names.join(", ")))),
        );
    }

    if let Some(msg) = ev.message {
        col = col.child(event_message_line(msg, scale, row_id, mention_click, true));
    }

    col
}

/// The chat line shown under a sub/resub/redemption's system text: the chatter's
/// attached message rendered like a normal chat row — timestamp, author badges,
/// bold colored name, then the body's words/emotes. The author name and any
/// `@mention` in the body open a usercard when `mention_click` is supplied (the
/// text/selection machinery of a full chat row isn't here, but names are still
/// clickable). A solid 7TV paint colors the name; a gradient paint is collapsed to
/// its midpoint color (the full per-char gradient needs the selectable-token
/// machinery the event row doesn't carry).
///
/// `compact` (the events panel) drops the timestamp, the author's badges, and
/// the name — the event row already shows who subscribed above, so repeating
/// their identity before the message is just noise (a badge next to a nameless
/// line read as a stray); only the message body remains.
fn event_message_line(
    msg: &Message,
    scale: Scale,
    row_id: u64,
    mention_click: Option<&MentionClick>,
    compact: bool,
) -> impl IntoElement {
    let p = palette();
    let time = msg
        .timestamp
        .with_timezone(&chrono::Local)
        .format("%H:%M")
        .to_string();
    let base_color = readable_color(
        msg.author
            .color
            .map(Color::to_u32)
            .unwrap_or_else(|| fallback_name_color(&msg.author)),
    );
    let name_color = match msg.author.paint.as_ref().map(|paint| &paint.kind) {
        Some(PaintKind::Solid(c)) => *c,
        Some(PaintKind::Linear { stops, .. }) | Some(PaintKind::Radial { stops }) => {
            sample_gradient(stops, 0.5).unwrap_or(base_color)
        }
        None => base_color,
    };

    let badges: Vec<gpui::AnyElement> = if compact {
        Vec::new()
    } else {
        msg.author
            .badges
            .iter()
            .enumerate()
            .map(|(i, badge)| {
                image_line_box(scale, scale.badge)
                    .mr_1()
                    .child(animated_img(
                        ("event-badge", row_id.wrapping_add(i as u64)),
                        badge.url.clone(),
                        px(scale.badge),
                    ))
                    .into_any_element()
            })
            .collect()
    };

    h_flex()
        .w_full()
        .min_w_0()
        .flex_wrap()
        .items_start()
        // The same line box the timestamp/badge prefixes use so the name + body
        // text share their baseline — without it they shape at gpui's taller
        // default line height and sit below the timestamp (see `render_message`).
        .line_height(px(scale.line))
        .when(!compact, |row| {
            row.child(
                line_box(scale)
                    .mr_1()
                    .text_color(rgb(p.timestamp))
                    .child(time),
            )
        })
        .children(badges)
        .when(!compact, |row| {
            row.child({
                let name = div()
                    .mr_1()
                    .font_weight(NAME_WEIGHT)
                    .text_color(rgb(name_color))
                    .child(SharedString::from(format!("{}:", msg.author.display_name)));
                // The author (the user who typed the attached message) opens their
                // usercard on click, like their name would in the main log.
                match mention_click {
                    Some(cb) => {
                        let cb = cb.clone();
                        let login = msg.author.login.clone();
                        name.id(("event-msg-author", row_id))
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .on_mouse_up(MouseButton::Left, move |_, window, cx| {
                                cb(&login, window, cx);
                            })
                            .into_any_element()
                    }
                    None => name.into_any_element(),
                }
            })
        })
        .children(inline_tokens(
            &msg.elements,
            scale,
            ("event-emote", row_id),
            mention_click,
        ))
}

/// Renders a message's token stream as inline elements (words + inline emote
/// images), the shared core of the event row and the reply preview. `seed` gives
/// each emote a stable element id (so GPUI animates it); the per-row part is the
/// caller's, the index is added per emote here. When `mention_click` is supplied
/// (the event message line), `@mention` tokens open the mentioned user's
/// usercard; the reply preview passes `None`, keeping the preview inert.
fn inline_tokens(
    message: &[MessageElement],
    scale: Scale,
    seed: (&'static str, u64),
    mention_click: Option<&MentionClick>,
) -> Vec<gpui::AnyElement> {
    let mut tokens: Vec<gpui::AnyElement> = Vec::new();
    let mut emote_index = 0u64;
    for (i, element) in message.iter().enumerate() {
        match element {
            MessageElement::Text { text, .. } => {
                // Each word keeps its trailing whitespace as the inter-word gap,
                // like chat body words; the same tighten pulls the following word
                // left so the gap matches the main log (see `WORD_TIGHTEN`).
                for word in split_words(text) {
                    if word.trim().is_empty() {
                        continue;
                    }
                    let mut token = div().child(SharedString::from(word.to_string()));
                    if word.ends_with(char::is_whitespace) {
                        token = token.mr(px(-WORD_TIGHTEN));
                    }
                    tokens.push(token.into_any_element());
                }
            }
            MessageElement::Emote(emote) => {
                tokens.push(
                    div()
                        .mx_px()
                        .child(animated_img(
                            (seed.0, seed.1.wrapping_add(emote_index)),
                            emote.url.clone(),
                            px(scale.emote),
                        ))
                        .into_any_element(),
                );
                emote_index += 1;
            }
            MessageElement::Mention { login } => {
                let token = div().mr_1().child(SharedString::from(format!("@{login}")));
                tokens.push(match mention_click {
                    Some(cb) => {
                        let cb = cb.clone();
                        let login = login.clone();
                        token
                            .id((seed.0, seed.1.wrapping_add(0x1000_0000 + i as u64)))
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .on_mouse_up(MouseButton::Left, move |_, window, cx| {
                                cb(&login, window, cx);
                            })
                            .into_any_element()
                    }
                    None => token.into_any_element(),
                });
            }
            MessageElement::Link { text, .. } => {
                tokens.push(
                    div()
                        .mr_1()
                        .child(SharedString::from(text.clone()))
                        .into_any_element(),
                );
            }
            MessageElement::Badge(_) => {}
        }
    }
    tokens
}

/// The inline preview of a parent message (words + emotes) for the "replying to"
/// bar above the input. Renders at the given font size with emotes inline, on one
/// wrapping line. `id_seed` keys the emote images stably.
pub fn render_reply_preview(
    elements: &[MessageElement],
    font_size: f32,
    id_seed: u64,
) -> impl IntoElement {
    let scale = Scale::new(font_size);
    h_flex()
        .min_w_0()
        .items_center()
        .overflow_hidden()
        .children(inline_tokens(
            elements,
            scale,
            ("reply-preview-emote", id_seed),
            None,
        ))
}

/// One compact line of a reply thread ("name: message with emotes inline") for
/// the thread context shown in the reply bar. The author name keeps its (readable)
/// color; `id_seed` keys the inline emote images and the name's clickable id.
/// `highlight` tints the row for the message the reply directly targets. When
/// `name_click` is set the author name opens that chatter's usercard.
pub fn render_thread_line(
    msg: &Message,
    font_size: f32,
    id_seed: u64,
    highlight: bool,
    name_click: Option<NameClick>,
) -> impl IntoElement {
    let scale = Scale::new(font_size);
    let name_color = readable_color(
        msg.author
            .color
            .map(Color::to_u32)
            .unwrap_or_else(|| fallback_name_color(&msg.author)),
    );
    let name_text = SharedString::from(format!("{}:", msg.author.display_name));
    let name = div()
        .flex_none()
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(name_color));
    let name = match name_click {
        Some(cb) => name
            .id(("thread-line-name", id_seed as usize))
            .cursor_pointer()
            .hover(|s| s.underline())
            .child(name_text)
            .on_mouse_down(
                MouseButton::Left,
                move |_, window: &mut Window, cx: &mut App| cb(window, cx),
            )
            .into_any_element(),
        None => name.child(name_text).into_any_element(),
    };
    h_flex()
        .w_full()
        .min_w_0()
        .items_baseline()
        .gap_1()
        .px_1()
        .when(highlight, |row| {
            // The message being replied to: a *faint* reply-tinted background
            // (the panel surface nudged toward the reply accent, not the muted
            // reply text color used as an opaque fill) with the reply color as a
            // left accent bar — mirrors the tint+accent pair the log's highlights
            // and the thread panel's seed row use.
            let p = palette();
            row.bg(rgb(blend(p.panel_bg, p.reply, 0.18)))
                .rounded_sm()
                .border_l_2()
                .border_color(rgb(p.reply))
        })
        .child(name)
        .child(
            div().flex_1().min_w_0().overflow_hidden().child(h_flex()
                .min_w_0()
                .items_center()
                .overflow_hidden()
                .children(inline_tokens(
                    &msg.elements,
                    scale,
                    ("thread-line-emote", id_seed),
                    None,
                ))),
        )
}

/// A stream live/offline notice: a highlighted row with the platform icon and a
/// short message. Going live shows the stream title when one is known. Styled
/// like an event row (icon pinned to the first line, text wraps) but with a
/// live/offline color so it reads as a status change, not a sub/raid.
pub fn render_live(
    platform: Platform,
    live: bool,
    title: &str,
    font_size: f32,
) -> impl IntoElement {
    let scale = Scale::new(font_size);
    let (_, fg) = highlight_live(live);
    let text = if live {
        if title.trim().is_empty() {
            format!("● {} stream is live", platform.label())
        } else {
            format!("● {} stream is live — {title}", platform.label())
        }
    } else {
        format!("○ {} stream went offline", platform.label())
    };

    // Bare content — the log's row wrapper paints the tint + accent bar.
    h_flex()
        .w_full()
        .min_w_0()
        .items_start()
        .gap_2()
        .text_color(rgb(fg))
        .text_size(px(scale.font))
        .child(platform_badge(platform, scale).flex_none())
        .child(div().flex_1().min_w_0().child(SharedString::from(text)))
}

/// A stable `u64` derived from a string, for deriving per-row element ids where
/// no message id exists (event rows). Same input → same id across re-renders, so
/// GPUI keeps advancing an emote's animation frames.
fn stable_id(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::{
        break_long_word, contrast_ratio, emote_tooltip_text, event_name_login, lerp_color,
        readable_color_on, sample_gradient, seventv_emote_id, split_words, strip_reply_prefix,
        DARK, LIGHT, LONG_WORD_CHARS, MIN_NAME_CONTRAST,
    };
    use bks_core::PaintStop;
    use bks_core::{Emote, EmoteTooltip};

    fn emote(name: &str, tooltip: EmoteTooltip) -> Emote {
        Emote {
            id: "1".into(),
            name: name.into(),
            url: "u".into(),
            animated: false,
            tooltip,
        }
    }

    #[test]
    fn parses_7tv_emote_links() {
        // With and without scheme, www, old. subdomain; trailing path/query/fragment.
        assert_eq!(
            seventv_emote_id("https://7tv.app/emotes/01F6ME7ADR").as_deref(),
            Some("01F6ME7ADR")
        );
        assert_eq!(
            seventv_emote_id("7tv.app/emotes/abc123").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            seventv_emote_id("https://old.7tv.app/emotes/xyz/").as_deref(),
            Some("xyz")
        );
        assert_eq!(
            seventv_emote_id("https://www.7tv.app/emotes/id?foo=1").as_deref(),
            Some("id")
        );
        // Not an emote link.
        assert_eq!(seventv_emote_id("https://7tv.app/users/bob"), None);
        assert_eq!(seventv_emote_id("https://example.com/emotes/x"), None);
        assert_eq!(seventv_emote_id("https://7tv.app/emotes/"), None);
    }

    #[test]
    fn emote_tooltip_omits_missing_lines() {
        // 7TV: name + provider + author.
        let full = emote(
            "qaixxAim",
            EmoteTooltip {
                provider: "7TV".into(),
                author: Some("Alice".into()),
            },
        );
        assert_eq!(emote_tooltip_text(&full), "qaixxAim\n7TV Emote\nBy: Alice");
        // Native: name + provider only (no author line).
        let native = emote("Kappa", EmoteTooltip::provider("Twitch"));
        assert_eq!(emote_tooltip_text(&native), "Kappa\nTwitch Emote");
        // No facts at all: just the name.
        let bare = emote("Plain", EmoteTooltip::default());
        assert_eq!(emote_tooltip_text(&bare), "Plain");
    }

    #[test]
    fn readable_color_lightens_only_dark_names() {
        let bg = DARK.chat_bg;
        // A bright name already clears the threshold and is returned unchanged.
        let bright = 0x9147ff;
        assert_eq!(readable_color_on(bright, bg), bright);
        // Pure black (a real Twitch name color) is too dark — it gets lightened
        // until it clears the contrast threshold against the chat background.
        let fixed = readable_color_on(0x000000, bg);
        assert_ne!(fixed, 0x000000);
        assert!(contrast_ratio(fixed, bg) >= MIN_NAME_CONTRAST);
        // A dark blue likewise becomes readable but isn't washed out to white.
        let dark_blue = readable_color_on(0x00008b, bg);
        assert!(contrast_ratio(dark_blue, bg) >= MIN_NAME_CONTRAST);
        assert_ne!(dark_blue, 0xffffff);
    }

    #[test]
    fn readable_color_darkens_bright_names_on_light_bg() {
        let bg = LIGHT.chat_bg;
        // A dark name already reads fine on white — returned unchanged.
        let dark = 0x111111;
        assert_eq!(readable_color_on(dark, bg), dark);
        // Pure white (or a near-white name) is invisible on white — it gets
        // darkened until it clears the threshold, not pushed to black needlessly.
        let fixed = readable_color_on(0xffffff, bg);
        assert_ne!(fixed, 0xffffff);
        assert!(contrast_ratio(fixed, bg) >= MIN_NAME_CONTRAST);
        // A bright yellow becomes readable on white.
        let yellow = readable_color_on(0xffff00, bg);
        assert!(contrast_ratio(yellow, bg) >= MIN_NAME_CONTRAST);
    }

    #[test]
    fn gradient_sampling_interpolates_between_stops() {
        let stops = vec![
            PaintStop {
                at: 0.0,
                color: 0x000000,
            },
            PaintStop {
                at: 1.0,
                color: 0xffffff,
            },
        ];
        // Endpoints return the stop colors exactly.
        assert_eq!(sample_gradient(&stops, 0.0), Some(0x000000));
        assert_eq!(sample_gradient(&stops, 1.0), Some(0xffffff));
        // Midpoint is mid-gray.
        assert_eq!(sample_gradient(&stops, 0.5), Some(0x808080));
        // Out-of-range clamps to the nearest stop.
        assert_eq!(sample_gradient(&stops, -1.0), Some(0x000000));
        assert_eq!(sample_gradient(&stops, 2.0), Some(0xffffff));
        // No stops → no color.
        assert_eq!(sample_gradient(&[], 0.5), None);
    }

    #[test]
    fn lerp_color_endpoints_and_mid() {
        assert_eq!(lerp_color(0xff0000, 0x0000ff, 0.0), 0xff0000);
        assert_eq!(lerp_color(0xff0000, 0x0000ff, 1.0), 0x0000ff);
        assert_eq!(lerp_color(0x000000, 0xffffff, 0.5), 0x808080);
    }

    #[test]
    fn break_long_word_is_lossless_and_bounded() {
        let word = "a".repeat(LONG_WORD_CHARS * 2 + 5);
        let pieces = break_long_word(&word);
        assert_eq!(pieces.concat(), word);
        assert!(pieces.iter().all(|p| p.chars().count() <= LONG_WORD_CHARS));
        // Multibyte chars must split on char boundaries, not bytes.
        let emoji = "🦀".repeat(LONG_WORD_CHARS + 3);
        assert_eq!(break_long_word(&emoji).concat(), emoji);
    }

    /// `split_words` must be lossless: the chunks concatenate back to the input,
    /// so per-word tokens still copy to the exact original text.
    #[test]
    fn split_words_is_lossless() {
        for s in [
            "hello world",
            "hello  world  ",
            " leading space",
            "trailing ",
            "one",
            "",
            "a b c d",
            "  ",
        ] {
            assert_eq!(split_words(s).concat(), s, "round-trip failed for {s:?}");
        }
    }

    #[test]
    fn split_words_breaks_at_words_keeping_trailing_space() {
        assert_eq!(
            split_words("hello world foo"),
            vec!["hello ", "world ", "foo"]
        );
        assert_eq!(split_words(" end"), vec![" end"]);
        assert_eq!(split_words("a  b"), vec!["a  ", "b"]);
    }

    #[test]
    fn event_name_login_matches_actor_and_mentions() {
        // The acting user's name is clickable (leading token, with trailing
        // punctuation from the formatted line).
        assert_eq!(event_name_login("alice", Some("alice")).as_deref(), Some("alice"));
        assert_eq!(
            event_name_login("Alice", Some("alice")).as_deref(),
            Some("Alice"),
            "case-insensitive actor match keeps the display casing"
        );
        // Any `@mention` in the text is clickable regardless of the actor.
        assert_eq!(event_name_login("@bob", None).as_deref(), Some("bob"));
        assert_eq!(event_name_login("@bob,", None).as_deref(), Some("bob"));
        // A plain word that isn't the actor and isn't an @mention is inert.
        assert_eq!(event_name_login("redeemed", Some("alice")), None);
        assert_eq!(event_name_login("(500", Some("alice")), None);
        // Punctuation-only / empty words never yield a login.
        assert_eq!(event_name_login("·", None), None);
        assert_eq!(event_name_login("", None), None);
    }

    #[test]
    fn strips_at_prefix_with_space() {
        assert_eq!(
            strip_reply_prefix("@bob hello there", Some("bob")),
            "hello there"
        );
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(strip_reply_prefix("@Bob hi", Some("bob")), "hi");
        assert_eq!(strip_reply_prefix("@bob hi", Some("Bob")), "hi");
    }

    #[test]
    fn only_prefix_yields_empty() {
        assert_eq!(strip_reply_prefix("@bob", Some("bob")), "");
        assert_eq!(strip_reply_prefix("@bob ", Some("bob")), "");
    }

    #[test]
    fn leaves_non_matching_author() {
        assert_eq!(strip_reply_prefix("@alice hi", Some("bob")), "@alice hi");
    }

    #[test]
    fn not_a_reply_is_unchanged() {
        assert_eq!(strip_reply_prefix("@bob hi", None), "@bob hi");
    }

    #[test]
    fn does_not_strip_partial_name_match() {
        // "@bobby" must not match author "bob".
        assert_eq!(strip_reply_prefix("@bobby hi", Some("bob")), "@bobby hi");
    }

    #[test]
    fn trims_leading_space_after_mention() {
        assert_eq!(strip_reply_prefix("@bob   hi", Some("bob")), "hi");
    }
}
